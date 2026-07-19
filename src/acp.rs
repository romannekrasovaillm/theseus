//! Минимальный ACP (Agent Client Protocol) stdio-клиент (урок обзора:
//! внешний агент живёт как дочерний процесс; обращение — по образцу
//! grok-build `xai-grok-mcp/src/acp_transport.rs`).
//!
//! Транспорт — NDJSON поверх stdio: каждое сообщение — одна строка JSON вида
//! `{id,method,params}` (запрос), `{id,result}` / `{id,error}` (ответ) или
//! `{method,params}` (нотификация, без id). Потоки:
//!
//! - **reader** читает stdout агента построчно, разбирает JSON и кладёт
//!   сообщения в канал; битые строки (не JSON) пропускает со счётчиком;
//! - **диспетчер** раскладывает ответы по oneshot-ожидателям (корреляция по
//!   `id`), входящие запросы агента (`session/request_permission`,
//!   `fs/read_text_file` и т.п.) — по зарегистрированным обработчикам,
//!   нотификации `session/update` — в буфер обновлений активного промпта.
//!
//! Клиентские методы протокола: `initialize`, `authenticate`, `session/new`,
//! `session/prompt` (стрим `session/update` собирается в `Vec<Update>`).
//! При смерти агента reader и диспетчер завершаются, все ожидающие ответа
//! получают ошибку, `Drop` убивает процесс и джойнит потоки.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

/// Таймаут ожидания ответа по умолчанию (мс).
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// JSON-RPC: метод не найден (у клиента нет обработчика).
const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC: обработчик на стороне клиента завершился ошибкой.
const HANDLER_ERROR: i64 = -32603;

/// Обработчик входящего запроса агента: получает `params`, возвращает JSON
/// для поля `result` ответа. `Send + Sync` нужны: вызов идёт из потока
/// диспетчера, а регистрация — из потока пользователя.
pub type RequestHandler = Box<dyn Fn(&Value) -> Result<Value> + Send + Sync + 'static>;

/// Клонируемая (разделяемая) форма обработчика для таблицы диспетчера.
type SharedHandler = Arc<dyn Fn(&Value) -> Result<Value> + Send + Sync + 'static>;

/// Oneshot-ответчик ожидающего запроса: ровно один ответ на один `id`.
type Responder = SyncSender<Result<Value, String>>;

/// Нотификация `session/update` от агента (кусочек стрима промпта).
#[derive(Debug, Clone)]
pub struct Update {
    /// Сессия, к которой относится обновление (`params.sessionId`).
    pub session_id: String,
    /// Вид обновления (`params.update.sessionUpdate`, напр.
    /// `agent_message_chunk`, `tool_call`, `plan`).
    pub kind: String,
    /// Сырое тело `params.update` — для разбора конкретных полей.
    pub raw: Value,
}

/// Итог `session/prompt`: финальный `stopReason` плюс все накопленные за
/// время промпта `session/update` этой сессии.
#[derive(Debug, Clone)]
pub struct PromptOutcome {
    /// Причина остановки агента (`end_turn`, `max_tokens`, `cancelled`, ...).
    pub stop_reason: String,
    /// Стрим-обновления сессии в порядке поступления.
    pub updates: Vec<Update>,
}

/// Разделяемое состояние клиента (reader + диспетчер + вызывающие потоки).
/// Порядок блокировок везде один: handlers → pending → stdin, обратного
/// захвата нет — дедлок исключён.
struct Shared {
    /// stdin агента: пишут и вызывающие потоки (запросы), и диспетчер
    /// (ответы на входящие запросы агента).
    stdin: Mutex<ChildStdin>,
    /// Ожидающие ответа исходящие запросы: `id` → oneshot-ответчик.
    pending: Mutex<HashMap<u64, Responder>>,
    /// Обработчики входящих запросов агента: метод → fn.
    handlers: Mutex<HashMap<String, SharedHandler>>,
    /// Накопленные нотификации `session/update` активного промпта.
    updates: Mutex<Vec<Update>>,
    /// Счётчик исходящих JSON-RPC id (монотонный, с 1).
    next_id: AtomicU64,
    /// Таймаут ожидания ответа (мс).
    timeout_ms: AtomicU64,
    /// Число пропущенных битых (не-JSON) строк из stdout агента.
    dropped_lines: AtomicU64,
    /// Агент жив (stdout открыт, reader работает).
    alive: AtomicBool,
}

/// Блокировка мьютекса с внятной ошибкой вместо паники при отравлении.
fn lock<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    m.lock().map_err(|_| anyhow!("мьютекс отравлен (паника в соседнем потоке)"))
}

/// ACP-клиент к внешнему агенту: дочерний процесс + NDJSON по stdio.
///
/// Клонов нет, потокобезопасность внутренняя: `send_request`/`call` можно
/// звать из разных потоков по `&AcpClient`, корреляция ответов — по `id`.
pub struct AcpClient {
    child: Child,
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
    dispatcher: Option<JoinHandle<()>>,
}

impl AcpClient {
    /// Запустить агента дочерним процессом и поднять reader + диспетчер.
    ///
    /// Рукопожатие НЕ выполняется автоматически — вызывайте [`Self::initialize`]
    /// (и при необходимости [`Self::authenticate`]) явно.
    pub fn spawn(command: &str, args: &[&str]) -> Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| anyhow!("не удалось запустить агента `{command}`: {e}"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("нет stdin у `{command}`"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("нет stdout у `{command}`"))?;
        let shared = Arc::new(Shared {
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            handlers: Mutex::new(HashMap::new()),
            updates: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
            timeout_ms: AtomicU64::new(DEFAULT_TIMEOUT_MS),
            dropped_lines: AtomicU64::new(0),
            alive: AtomicBool::new(true),
        });
        let (tx, rx) = mpsc::channel::<Value>();
        let reader_shared = Arc::clone(&shared);
        let reader = std::thread::spawn(move || reader_loop(stdout, tx, reader_shared));
        let dispatcher_shared = Arc::clone(&shared);
        let dispatcher = std::thread::spawn(move || dispatcher_loop(rx, dispatcher_shared));
        Ok(Self {
            child,
            shared,
            reader: Some(reader),
            dispatcher: Some(dispatcher),
        })
    }

    /// Зарегистрировать обработчик входящего запроса агента
    /// (`session/request_permission`, `fs/read_text_file`, `fs/write_text_file`, ...).
    ///
    /// `Ok(value)` обработчика уходит агенту как JSON-RPC `result`, `Err` —
    /// как JSON-RPC `error` (-32603). Запрос метода без обработчика получает
    /// -32601 (method not found). Повторная регистрация метода заменяет
    /// обработчик.
    pub fn on_request(&self, method: &str, handler: RequestHandler) -> Result<()> {
        let shared: SharedHandler = Arc::from(handler);
        let _prev = lock(&self.shared.handlers)?.insert(method.to_string(), shared);
        Ok(())
    }

    /// Отправить запрос и вернуть oneshot-ожидатель ответа по `id`.
    /// Не блокируется: ожидание — через [`PendingResponse::wait`].
    pub fn send_request(&self, method: &str, params: Value) -> Result<PendingResponse> {
        if !self.shared.alive.load(Ordering::SeqCst) {
            return Err(anyhow!("агент не запущен или уже завершился"));
        }
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = mpsc::sync_channel::<Result<Value, String>>(1);
        let _prev = lock(&self.shared.pending)?.insert(id, tx);
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = write_value(&self.shared, &req) {
            // Не оставляем сироту в таблице ожидания.
            if let Ok(mut map) = self.shared.pending.lock() {
                let _gone = map.remove(&id);
            }
            return Err(e.context(format!("запись запроса {method} (id {id}) в stdin агента")));
        }
        Ok(PendingResponse {
            id,
            method: method.to_string(),
            rx,
            shared: Arc::clone(&self.shared),
        })
    }

    /// Отправить запрос и дождаться ответа (не дольше текущего таймаута).
    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        let timeout = self.timeout();
        self.send_request(method, params)?.wait(timeout)
    }

    /// Отправить нотификацию (без `id`, ответа не ждём).
    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        write_value(&self.shared, &json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    /// ACP `initialize`: версия протокола + клиентские возможности
    /// (fs/read_text_file — да, write и terminal — нет). Возвращает
    /// `protocolVersion` и `agentCapabilities` агента.
    pub fn initialize(&self) -> Result<Value> {
        self.call(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": true, "writeTextFile": false},
                    "terminal": false,
                },
                "clientInfo": {"name": "theseus", "version": "0.2.0"},
            }),
        )
    }

    /// ACP `authenticate`: выбор метода авторизации (если агент его требует
    /// по `authMethods` из initialize).
    pub fn authenticate(&self, method_id: &str) -> Result<()> {
        self.call("authenticate", json!({"methodId": method_id}))?;
        Ok(())
    }

    /// ACP `session/new`: открыть сессию в каталоге `cwd`, вернуть `sessionId`.
    pub fn session_new(&self, cwd: &str) -> Result<String> {
        let res = self.call("session/new", json!({"cwd": cwd, "mcpServers": []}))?;
        res["sessionId"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("session/new: в ответе нет sessionId: {res}"))
    }

    /// ACP `session/prompt`: отправить текстовый промпт, дождаться финала и
    /// вернуть `stopReason` вместе со стримом `session/update`, накопленным
    /// за время промпта (буфер перед отправкой очищается).
    pub fn session_prompt(&self, session_id: &str, text: &str) -> Result<PromptOutcome> {
        let _stale = self.take_updates(); // сброс хвоста прошлых промптов
        let res = self.call(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": text}],
            }),
        )?;
        let stop_reason = res["stopReason"].as_str().unwrap_or("unknown").to_string();
        let updates = self
            .take_updates()
            .into_iter()
            .filter(|u| u.session_id == session_id)
            .collect();
        Ok(PromptOutcome { stop_reason, updates })
    }

    /// Забрать все накопленные нотификации `session/update` (буфер очищается).
    /// Полезно при ручной работе через [`Self::call`].
    pub fn take_updates(&self) -> Vec<Update> {
        match self.shared.updates.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            Err(_) => Vec::new(),
        }
    }

    /// Установить таймаут ожидания ответов (применяется к новым `call`/`wait`).
    pub fn set_timeout(&self, d: Duration) {
        let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        self.shared.timeout_ms.store(ms, Ordering::Relaxed);
    }

    /// Текущий таймаут ожидания ответов.
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.shared.timeout_ms.load(Ordering::Relaxed))
    }

    /// Жив ли агент (stdout открыт). `false` после EOF/смерти процесса.
    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }

    /// Сколько битых (не-JSON) строк пропущено reader'ом — диагностика
    /// мусора в stdout агента (println-отладка агента и т.п.).
    pub fn dropped_lines(&self) -> u64 {
        self.shared.dropped_lines.load(Ordering::Relaxed)
    }

    /// PID дочернего процесса агента.
    pub fn child_pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        // Убиваем агента: его stdout закрывается → reader видит EOF и
        // завершается → канал к диспетчеру закрывается → диспетчер будит
        // оставшихся ожидающих и завершается. Джойним оба потока, чтобы не
        // оставлять висючих потоков и зомби-процессов.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        if let Some(h) = self.dispatcher.take() {
            let _ = h.join();
        }
    }
}

/// Oneshot-ожидатель ответа на конкретный исходящий запрос (корреляция по `id`).
pub struct PendingResponse {
    id: u64,
    method: String,
    rx: Receiver<Result<Value, String>>,
    shared: Arc<Shared>,
}

impl PendingResponse {
    /// Id запроса, который ждёт этот ожидатель.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Дождаться ответа (не дольше `timeout`). При таймауте ожидатель
    /// снимается из таблицы — запоздавший ответ будет проигнорирован.
    pub fn wait(self, timeout: Duration) -> Result<Value> {
        match self.rx.recv_timeout(timeout) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!("{e}")),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(mut map) = self.shared.pending.lock() {
                    let _gone = map.remove(&self.id);
                }
                Err(anyhow!(
                    "таймаут запроса {} (id {}): ждали {:?}",
                    self.method,
                    self.id,
                    timeout
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(anyhow!("диспетчер завершился до ответа на {}", self.method))
            }
        }
    }
}

/// Сериализовать сообщение и записать одной строкой в stdin агента.
fn write_value(shared: &Shared, v: &Value) -> Result<()> {
    let text = serde_json::to_string(v)?;
    let mut stdin = lock(&shared.stdin)?;
    writeln!(stdin, "{text}")?;
    stdin.flush()?;
    Ok(())
}

/// Reader-поток: stdout агента → построчный разбор → канал диспетчеру.
/// Битые (не-JSON) строки пропускаются со счётчиком, пустые — молча.
/// Завершается при EOF (смерть агента) или при смерти диспетчера.
fn reader_loop(stdout: ChildStdout, tx: mpsc::Sender<Value>, shared: Arc<Shared>) {
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let Ok(line) = line else { break }; // битый UTF-8 / закрытый пайп
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(msg) => {
                if tx.send(msg).is_err() {
                    break; // диспетчер умер
                }
            }
            Err(_) => {
                shared.dropped_lines.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    shared.alive.store(false, Ordering::SeqCst);
}

/// Диспетчер: ответы → oneshot-ожидателям по `id`; входящие запросы →
/// обработчикам; нотификации `session/update` → в буфер. При закрытии канала
/// (reader умер вместе с агентом) будит всех ожидающих ошибкой.
fn dispatcher_loop(rx: Receiver<Value>, shared: Arc<Shared>) {
    while let Ok(msg) = rx.recv() {
        dispatch(&msg, &shared);
    }
    shared.alive.store(false, Ordering::SeqCst);
    // Агент умер: снимаем всех ожидающих с ошибкой вместо вечного таймаута.
    let senders: Vec<Responder> = match lock(&shared.pending) {
        Ok(mut map) => map.drain().map(|(_, s)| s).collect(),
        Err(_) => Vec::new(),
    };
    for s in senders {
        let _ = s.send(Err("агент завершил работу до ответа".to_string()));
    }
}

/// Разбор одного входящего сообщения от агента.
fn dispatch(msg: &Value, shared: &Shared) {
    let method = msg["method"].as_str();
    let id = &msg["id"];
    match (method, id.is_null()) {
        // Входящий запрос агента (есть и метод, и id).
        (Some(m), false) => incoming_request(shared, id.clone(), m, &msg["params"]),
        // Нотификация (метод без id).
        (Some(_), true) => notification(shared, msg),
        // Ответ на наш запрос (id без метода).
        (None, false) => response(shared, msg),
        // Пустой объект и прочий мусор валидного JSON — игнорируем.
        (None, true) => {}
    }
}

/// Входящий запрос агента: вызвать обработчик и вернуть result/error по `id`.
/// Обработчик вызывается БЕЗ удержания блокировок (Arc вынут из таблицы
/// заранее) — он может безопасно звать обратно в клиент.
fn incoming_request(shared: &Shared, id: Value, method: &str, params: &Value) {
    let handler = lock(&shared.handlers)
        .ok()
        .and_then(|map| map.get(method).map(Arc::clone));
    let response = match handler {
        Some(h) => match h(params) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": HANDLER_ERROR, "message": format!("обработчик {method}: {e}")},
            }),
        },
        None => json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": METHOD_NOT_FOUND, "message": format!("клиент не обрабатывает {method}")},
        }),
    };
    if write_value(shared, &response).is_err() {
        shared.alive.store(false, Ordering::SeqCst);
    }
}

/// Нотификация: собираем только `session/update` (стрим промпта); прочие
/// в минимальной версии пропускаем.
fn notification(shared: &Shared, msg: &Value) {
    if msg["method"].as_str() != Some("session/update") {
        return;
    }
    let params = &msg["params"];
    let update = Update {
        session_id: params["sessionId"].as_str().unwrap_or_default().to_string(),
        kind: params["update"]["sessionUpdate"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        raw: params["update"].clone(),
    };
    if let Ok(mut buf) = shared.updates.lock() {
        buf.push(update);
    }
}

/// Ответ на наш запрос: маршрутизация oneshot-ожидателю по `id`.
/// Неизвестный `id` (просрочен по таймауту или чужой) — игнорируем.
fn response(shared: &Shared, msg: &Value) {
    let Some(id) = msg["id"].as_u64() else { return };
    let sender = lock(&shared.pending).ok().and_then(|mut map| map.remove(&id));
    let Some(sender) = sender else { return };
    let payload = match msg.get("error").filter(|e| !e.is_null()) {
        Some(err) => Err(format!("JSON-RPC ошибка на id {id}: {err}")),
        None => Ok(msg["result"].clone()),
    };
    let _ = sender.send(payload);
}

#[cfg(test)]
mod tests {
    //! Тесты против echo-мока на bash: `while read` цикл, отвечающий
    //! заготовленными JSON-строками (в т.ч. входящими запросами к клиенту и
    //! битой строкой). Мок покрывает все методы, что зовут тесты.
    use super::*;
    use std::time::Instant;

    /// Bash-мок ACP-агента: читает NDJSON-запросы, отвечает по шаблону.
    /// id извлекается sed'ом из поля `"id":N` (в наших конвертах это поле
    /// единственное с таким написанием — camelCase-ключи не совпадают).
    const MOCK: &str = r#"
reply() { printf '%s\n' "$1"; }
getid() { printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }
while IFS= read -r line; do
  id=$(getid "$line")
  case "$line" in
    *'"method":"initialize"'*)
      reply "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{\"loadSession\":false}}}"
      ;;
    *'"method":"authenticate"'*)
      reply "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{}}"
      ;;
    *'"method":"session/new"'*)
      reply "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"sessionId\":\"sess-1\"}}"
      ;;
    *'"method":"session/prompt"'*)
      reply '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Привет"}}}}'
      reply '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"мир"}}}}'
      reply "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"stopReason\":\"end_turn\"}}"
      ;;
    *'"method":"test/echo"'*)
      params=$(printf '%s' "$line" | sed -n 's/.*"params":\(.*\)\}$/\1/p')
      reply "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"echo\":$params}}"
      ;;
    *'"method":"test/hold"'*)
      held=$id
      IFS= read -r line2
      id2=$(getid "$line2")
      reply "{\"jsonrpc\":\"2.0\",\"id\":$id2,\"result\":{\"order\":\"second\"}}"
      sleep 0.3
      reply "{\"jsonrpc\":\"2.0\",\"id\":$held,\"result\":{\"order\":\"first\"}}"
      ;;
    *'"method":"test/ask"'*)
      reply '{"jsonrpc":"2.0","id":900,"method":"session/request_permission","params":{"sessionId":"sess-1","toolCall":{"toolCallId":"t1","title":"run"},"options":[{"optionId":"allow","name":"Allow"}]}}'
      IFS= read -r perm_resp
      reply "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"clientResponded\":$perm_resp}}"
      ;;
    *'"method":"test/readfile"'*)
      reply '{"jsonrpc":"2.0","id":901,"method":"fs/read_text_file","params":{"sessionId":"sess-1","path":"/tmp/x.txt"}}'
      IFS= read -r fs_resp
      reply "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"clientResponded\":$fs_resp}}"
      ;;
    *'"method":"test/garbage"'*)
      reply 'это вовсе не json {{{'
      reply "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"ok\":true}}"
      ;;
    *'"method":"test/slow"'*)
      sleep 2
      reply "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{}}"
      ;;
    *'"method":"test/exit"'*)
      exit 0
      ;;
  esac
done
"#;

    /// Свежий мок-агент на каждый тест (изоляция).
    fn spawn_mock() -> AcpClient {
        AcpClient::spawn("bash", &["-c", MOCK]).expect("мок-агент должен запускаться")
    }

    #[test]
    fn initialize_handshake_returns_agent_capabilities() {
        let client = spawn_mock();
        let res = client.initialize().expect("initialize");
        assert_eq!(res["protocolVersion"], 1);
        assert_eq!(res["agentCapabilities"]["loadSession"], false);
        assert!(client.is_alive());
    }

    #[test]
    fn echo_request_response_round_trip() {
        let client = spawn_mock();
        let res = client
            .call("test/echo", json!({"hello": "world", "n": 42}))
            .expect("echo");
        assert_eq!(res["echo"]["hello"], "world");
        assert_eq!(res["echo"]["n"], 42);
    }

    /// Два запроса подряд: мок отвечает В ОБРАТНОМ порядке (сначала на
    /// второй). Клиент обязан сопоставить ответы по id, а не по порядку
    /// прихода: p1 ждёт «first», p2 — «second», несмотря на обратный стрим.
    #[test]
    fn out_of_order_responses_are_matched_by_id() {
        let client = spawn_mock();
        let p1 = client.send_request("test/hold", json!({})).expect("p1");
        let p2 = client.send_request("test/echo", json!({"tag": "p2"})).expect("p2");
        assert_eq!(p1.id() + 1, p2.id(), "id должны идти подряд");
        let r2 = p2.wait(Duration::from_secs(5)).expect("p2 отвечают первым");
        let r1 = p1.wait(Duration::from_secs(5)).expect("p1 отвечают вторым");
        assert_eq!(r2["order"], "second");
        assert_eq!(r1["order"], "first");
    }

    #[test]
    fn session_prompt_collects_streamed_updates() {
        let client = spawn_mock();
        let sid = client.session_new("/tmp").expect("session/new");
        let out = client.session_prompt(&sid, "расскажи").expect("prompt");
        assert_eq!(out.stop_reason, "end_turn");
        assert_eq!(out.updates.len(), 2);
        assert!(out.updates.iter().all(|u| u.kind == "agent_message_chunk"));
        assert_eq!(out.updates[0].raw["content"]["text"], "Привет");
        assert_eq!(out.updates[1].raw["content"]["text"], "мир");
        assert!(out.updates.iter().all(|u| u.session_id == "sess-1"));
        // Буфер после промпта пуст — обновления не копятся между промптами.
        assert!(client.take_updates().is_empty());
    }

    #[test]
    fn authenticate_and_session_new() {
        let client = spawn_mock();
        client.initialize().expect("initialize");
        client.authenticate("my-auth-method").expect("authenticate");
        assert_eq!(client.session_new("/home").expect("session/new"), "sess-1");
    }

    /// Входящий запрос агента `session/request_permission` должен уйти в
    /// зарегистрированный обработчик, а его ответ — вернуться агенту с тем
    /// же id (900): мок вкладывает наш ответ в финальный result.
    #[test]
    fn incoming_permission_request_goes_to_handler() {
        let client = spawn_mock();
        let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        client
            .on_request(
                "session/request_permission",
                Box::new(move |params: &Value| {
                    if let Ok(mut g) = seen2.lock() {
                        g.push(params.clone());
                    }
                    Ok(json!({"outcome": {"outcome": "selected", "optionId": "allow"}}))
                }),
            )
            .expect("on_request");
        let res = client.call("test/ask", json!({})).expect("test/ask");
        // Агент получил наш ответ именно на id 900 и именно с нашим outcome.
        assert_eq!(res["clientResponded"]["id"], 900);
        assert_eq!(
            res["clientResponded"]["result"]["outcome"]["optionId"],
            "allow"
        );
        let seen = seen.lock().expect("seen");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["toolCall"]["toolCallId"], "t1");
    }

    /// Входящий `fs/read_text_file`: обработчик получает path, отвечает
    /// содержимым; агент подтверждает получение (id 901).
    #[test]
    fn incoming_fs_read_request_goes_to_handler() {
        let client = spawn_mock();
        let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        client
            .on_request(
                "fs/read_text_file",
                Box::new(move |params: &Value| {
                    if let Ok(mut g) = seen2.lock() {
                        g.push(params.clone());
                    }
                    Ok(json!({"content": "fn main() {}"}))
                }),
            )
            .expect("on_request");
        let res = client.call("test/readfile", json!({})).expect("test/readfile");
        assert_eq!(res["clientResponded"]["id"], 901);
        assert_eq!(res["clientResponded"]["result"]["content"], "fn main() {}");
        let seen = seen.lock().expect("seen");
        assert_eq!(seen[0]["path"], "/tmp/x.txt");
    }

    /// Битая строка в stdout не должна ломать транспорт: reader пропускает
    /// её (со счётчиком), следующий валидный ответ доезжает по своему id.
    #[test]
    fn broken_line_is_skipped_and_counted() {
        let client = spawn_mock();
        let res = client.call("test/garbage", json!({})).expect("test/garbage");
        assert_eq!(res["ok"], true);
        assert!(
            client.dropped_lines() >= 1,
            "битая строка должна быть посчитана"
        );
    }

    /// Молчаливый агент: запрос отваливается по таймауту, запись ожидания
    /// снимается, а клиент остаётся рабочим — запоздавший ответ игнорируется,
    /// следующий запрос коррелируется по своему id.
    #[test]
    fn slow_agent_hits_timeout_and_client_recovers() {
        let client = spawn_mock();
        client.set_timeout(Duration::from_millis(300));
        let err = client.call("test/slow", json!({})).expect_err("должен быть таймаут");
        assert!(
            err.to_string().contains("таймаут"),
            "ожидали таймаут, получили: {err}"
        );
        client.set_timeout(Duration::from_secs(5));
        let res = client
            .call("test/echo", json!({"after": "timeout"}))
            .expect("клиент жив после таймаута");
        assert_eq!(res["echo"]["after"], "timeout");
    }

    /// Смерть агента: reader видит EOF и завершается (is_alive → false),
    /// новые запросы сразу получают ошибку, а не висят до таймаута.
    #[test]
    fn reader_and_dispatcher_stop_when_agent_exits() {
        let client = spawn_mock();
        client.initialize().expect("initialize");
        client.notify("test/exit", json!({})).expect("notify");
        let deadline = Instant::now() + Duration::from_secs(3);
        while client.is_alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!client.is_alive(), "клиент должен заметить смерть агента");
        let err = client
            .call("test/echo", json!({}))
            .expect_err("запрос к мёртвому агенту — ошибка");
        assert!(err.to_string().contains("завершился"), "получили: {err}");
    }

    /// Уведомление без id не требует ответа и не портит следующий запрос.
    #[test]
    fn notify_does_not_break_following_request() {
        let client = spawn_mock();
        client
            .notify("session/cancel", json!({"sessionId": "sess-1"}))
            .expect("notify");
        let res = client.call("test/echo", json!({"x": 1})).expect("echo");
        assert_eq!(res["echo"]["x"], 1);
    }
}
