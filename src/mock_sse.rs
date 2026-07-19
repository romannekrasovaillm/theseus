//! Мок SSE-сервера OpenAI-совместимого `chat/completions` для тестов.
//!
//! Поднимает на `std::net::TcpListener` однопоточный HTTP-сервер, который
//! отвечает на `POST /chat/completions` заранее запрограммированными
//! сценариями ([`Scenario`]) в формате server-sent events: кадры
//! `data: {...}\n\n` и завершающий `data: [DONE]`. Одно соединение = один
//! запрос; keep-alive не поддерживается (reqwest это переносит).
//!
//! Возможности:
//! - очередь сценариев: i-й запрос получает i-й сценарий, сверх очереди — 500;
//! - журнал тел запросов (JSON) для проверок — [`MockHandle::requests`];
//! - ожидание инструмента ([`Scenario::expect_tool_call`]): если запрос не
//!   ссылается на инструмент — 400;
//! - 404 на любой другой путь.
//!
//! По образцу `xai-grok-test-support/src/sse.rs` (grok-build), но на чистом
//! std: в theseus нет axum/tokio. Поля `thinking` из `extra_body` клиента
//! просто игнорируются — мок их не разбирает.
//!
//! # Пример
//!
//! ```no_run
//! use theseus::mock_sse::{MockLlm, Scenario};
//!
//! let handle = MockLlm::with_scenarios(vec![Scenario::new().reply_text("Привет!")])
//!     .serve_on_ephemeral()
//!     .unwrap();
//! // ... код под тестом ходит на handle.base_url ...
//! handle.assert_request_count(1);
//! handle.join();
//! ```

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

/// Таймаут чтения/записи одного соединения: подвисший клиент не должен
/// блокировать мок-сервер навсегда.
const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// Максимум строк заголовков на запрос — защита от мусорных клиентов.
const MAX_HEADER_LINES: usize = 128;

/// Одна дельта ответа модели: кусок текста или вызов инструмента.
#[derive(Debug, Clone)]
enum Delta {
    /// Текстовый кусок `choices[0].delta.content`.
    Text(String),
    /// Вызов инструмента `choices[0].delta.tool_calls[...]`.
    ToolCall { name: String, arguments: String },
}

/// Сценарий одного ответа мок-сервера (строитель).
///
/// Порядок вызовов `reply_*` определяет порядок SSE-кадров в потоке. Если ни
/// один `reply_*` не вызван, ответ состоит только из терминального кадра с
/// `finish_reason` — краевой случай «пустого» ответа модели.
#[derive(Debug, Clone)]
pub struct Scenario {
    expected_tool: Option<String>,
    deltas: Vec<Delta>,
    finish: String,
}

impl Scenario {
    /// Пустой сценарий: `finish_reason` по умолчанию — `"stop"`.
    pub fn new() -> Self {
        Self { expected_tool: None, deltas: Vec::new(), finish: "stop".to_string() }
    }

    /// Ожидание: входящий запрос должен ссылаться на инструмент `tool_name`
    /// (объявлен в `tools[*].function.name` или уже вызывался — встречается в
    /// `messages[*].tool_calls[*].function.name`). Нарушение → ответ 400;
    /// сценарий при этом всё равно считается израсходованным.
    pub fn expect_tool_call(mut self, tool_name: impl Into<String>) -> Self {
        self.expected_tool = Some(tool_name.into());
        self
    }

    /// Ответить одним текстовым кадром с содержимым `text`.
    pub fn reply_text(mut self, text: impl Into<String>) -> Self {
        self.deltas.push(Delta::Text(text.into()));
        self
    }

    /// Ответить вызовом инструмента `name` с JSON-аргументами `arguments_json`
    /// (строкой, как на проводе). Несколько вызовов подряд получают разные
    /// `index` и id `call_0`, `call_1`, ...
    pub fn reply_tool_call(mut self, name: impl Into<String>, arguments_json: impl Into<String>) -> Self {
        self.deltas.push(Delta::ToolCall { name: name.into(), arguments: arguments_json.into() });
        self
    }

    /// Ответить потоком текстовых кадров (по одному SSE-кадру на элемент).
    pub fn reply_stream(mut self, chunks: &[&str]) -> Self {
        self.deltas.extend(chunks.iter().map(|c| Delta::Text((*c).to_string())));
        self
    }

    /// Задать `finish_reason` терминального кадра (`"stop"` / `"length"` /
    /// `"tool_calls"`). Значение не валидируется — на провод уходит как есть.
    pub fn finish_reason(mut self, reason: impl Into<String>) -> Self {
        self.finish = reason.into();
        self
    }
}

impl Default for Scenario {
    fn default() -> Self {
        Self::new()
    }
}

/// Очередь сценариев мок-LLM; [`MockLlm::serve_on_ephemeral`] поднимает сервер.
#[derive(Debug)]
pub struct MockLlm {
    scenarios: Vec<Scenario>,
}

impl MockLlm {
    /// Пустая очередь: любой запрос к `/chat/completions` получит 500.
    pub fn new() -> Self {
        Self { scenarios: Vec::new() }
    }

    /// Очередь из готового списка сценариев (порядок = порядок запросов).
    pub fn with_scenarios(scenarios: Vec<Scenario>) -> Self {
        Self { scenarios }
    }

    /// Добавить сценарий в хвост очереди.
    pub fn enqueue(&mut self, scenario: Scenario) -> &mut Self {
        self.scenarios.push(scenario);
        self
    }

    /// Поднять сервер на `127.0.0.1:0` (эфемерный порт) и вернуть хендл.
    ///
    /// # Errors
    /// Ошибка `io`, если не удалось занять локальный сокет.
    pub fn serve_on_ephemeral(self) -> io::Result<MockHandle> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let local_addr = listener.local_addr()?;
        let shared = Arc::new(Shared::new(self.scenarios));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = {
            let shared = Arc::clone(&shared);
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || server_loop(&listener, &shared, &shutdown))
        };
        Ok(MockHandle {
            base_url: format!("http://{local_addr}"),
            local_addr,
            shared,
            shutdown,
            thread: Some(thread),
        })
    }
}

impl Default for MockLlm {
    fn default() -> Self {
        Self::new()
    }
}

/// Хендл запущенного мок-сервера: адрес, журнал запросов, ожидание потока.
///
/// При drop сервер останавливается автоматически (флаг + фиктивное соединение
/// будят `accept`), поэтому поток не утекает даже без явного
/// [`MockHandle::join`].
#[derive(Debug)]
pub struct MockHandle {
    /// Базовый URL вида `http://127.0.0.1:PORT` (без завершающего слэша).
    pub base_url: String,
    local_addr: SocketAddr,
    shared: Arc<Shared>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MockHandle {
    /// Снимок журнала: JSON-тела всех запросов, пришедших на сервер, в порядке
    /// получения (включая запросы на неизвестные пути). Тела, не парсящиеся
    /// как JSON, записываются строкой; запросы без тела не записываются.
    pub fn requests(&self) -> Vec<Value> {
        lock_recover(&self.shared.requests).clone()
    }

    /// Проверка числа записанных запросов.
    ///
    /// # Panics
    /// Паникует, если фактическое число запросов не равно `expected`.
    pub fn assert_request_count(&self, expected: usize) {
        let actual = lock_recover(&self.shared.requests).len();
        assert_eq!(actual, expected, "mock_sse: неожиданное число запросов к мок-серверу");
    }

    /// Остановить сервер и дождаться завершения его потока.
    /// (Вся работа — в `Drop`; метод нужен для явности и читаемости тестов.)
    pub fn join(self) {
        // drop(self) ставит флаг, будит accept фиктивным соединением и ждёт поток.
    }

    /// Сигнал остановки + будим блокирующий `accept` фиктивным соединением.
    fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.local_addr);
    }
}

impl Drop for MockHandle {
    fn drop(&mut self) {
        self.signal_shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Разделяемое состояние сервера: очередь сценариев и журнал запросов.
#[derive(Debug)]
struct Shared {
    scenarios: Mutex<VecDeque<Scenario>>,
    requests: Mutex<Vec<Value>>,
    /// Сырой журнал для фасада [`MockServer`]: метод/путь/тело строкой.
    raw_requests: Mutex<Vec<RecordedRequest>>,
    /// Мягкий режим: при исчерпании очереди отвечать текстом-заглушкой, а не 500
    /// (нужен бинарным e2e: харнесс может слать внеплановые запросы после finish).
    lenient: AtomicBool,
}

impl Shared {
    fn new(scenarios: Vec<Scenario>) -> Self {
        Self {
            scenarios: Mutex::new(VecDeque::from(scenarios)),
            requests: Mutex::new(Vec::new()),
            raw_requests: Mutex::new(Vec::new()),
            lenient: AtomicBool::new(false),
        }
    }
}

/// Lock с восстановлением после poison: паника в соседнем потоке не должна
/// валить мок-сервер (`.unwrap()` вне `#[cfg(test)]` запрещён линтами).
fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Цикл accept: одно соединение = один запрос; выход по флагу shutdown
/// (цикл будит фиктивное соединение из `signal_shutdown`).
fn server_loop(listener: &TcpListener, shared: &Shared, shutdown: &AtomicBool) {
    for stream in listener.incoming() {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        match stream {
            Ok(mut stream) => {
                let _ = serve_connection(&mut stream, shared);
            }
            Err(_) => break,
        }
    }
}

/// Обслужить одно соединение: прочитать запрос, отдать ответ, закрыться.
fn serve_connection(stream: &mut TcpStream, shared: &Shared) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let Some(request) = read_request(stream)? else {
        return Ok(());
    };
    let response = route(&request, shared);
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

/// Разобранный HTTP-запрос (ровно то, что нужно моку).
#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    body_json: Option<Value>,
}

/// Прочитать запрос: стартовая строка + заголовки до пустой строки, затем
/// ровно `Content-Length` байт тела. `Ok(None)` — соединение закрылось до
/// запроса (фиктивное соединение shutdown) или стартовая строка мусорная.
fn read_request(stream: &mut TcpStream) -> io::Result<Option<Request>> {
    let mut reader = BufReader::new(stream);
    let mut start_line = String::new();
    if reader.read_line(&mut start_line)? == 0 {
        return Ok(None);
    }
    let mut parts = start_line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let method = method.to_string();
    let path = path.to_string();

    let mut content_length = 0usize;
    for _ in 0..MAX_HEADER_LINES {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }

    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    let body_json = if body.is_empty() {
        None
    } else {
        let text = String::from_utf8_lossy(&body);
        Some(serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.into_owned())))
    };
    Ok(Some(Request { method, path, body_json }))
}

/// Маршрутизация: `POST /chat/completions` (и `/v1/chat/completions`) —
/// следующий сценарий из очереди; всё остальное — 404. JSON-тело любого
/// запроса предварительно записывается в журнал.
fn route(request: &Request, shared: &Shared) -> String {
    if let Some(body) = &request.body_json {
        lock_recover(&shared.requests).push(body.clone());
    }
    lock_recover(&shared.raw_requests).push(RecordedRequest {
        method: request.method.clone(),
        path: request.path.clone(),
        body: request
            .body_json
            .as_ref()
            .map_or_else(String::new, ToString::to_string),
    });
    let (path, _) = request.path.split_once('?').unwrap_or((request.path.as_str(), ""));
    let is_completions = request.method.eq_ignore_ascii_case("POST")
        && (path == "/chat/completions" || path == "/v1/chat/completions");
    if !is_completions {
        return error_response(404, "not_found", &format!("mock_sse: неизвестный путь «{path}»"));
    }

    let scenario = lock_recover(&shared.scenarios).pop_front();
    let Some(scenario) = scenario else {
        if shared.lenient.load(Ordering::SeqCst) {
            // Мягкий режим: запасной текстовый ответ, чтобы харнесс не зависал.
            let spare = Scenario::new().reply_text("mock_sse: запасной ответ (очередь исчерпана)");
            return ok_sse(&scenario_to_sse(&spare, "mock-model", 0));
        }
        return error_response(500, "mock_exhausted", "mock_sse: очередь сценариев исчерпана");
    };

    if let Some(expected) = &scenario.expected_tool {
        if !request_references_tool(request.body_json.as_ref(), expected) {
            return error_response(
                400,
                "expectation_failed",
                &format!("mock_sse: ожидался вызов инструмента «{expected}», но запрос на него не ссылается"),
            );
        }
    }

    let model = request
        .body_json
        .as_ref()
        .and_then(|b| b["model"].as_str())
        .unwrap_or("mock-model");
    // prompt_tokens эхом считаем по числу сообщений запроса — наглядно в тестах.
    let prompt_tokens = request
        .body_json
        .as_ref()
        .and_then(|b| b["messages"].as_array())
        .map_or(0, |messages| messages.len() as u64);
    ok_sse(&scenario_to_sse(&scenario, model, prompt_tokens))
}

/// Ссылается ли запрос на инструмент `tool`: объявлен в `tools` или уже
/// встречается в истории `messages[*].tool_calls`.
fn request_references_tool(body: Option<&Value>, tool: &str) -> bool {
    let Some(body) = body else {
        return false;
    };
    let in_tools = body["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|t| tool_name_matches(t, tool)));
    let in_messages = body["messages"].as_array().is_some_and(|messages| {
        messages
            .iter()
            .any(|m| m["tool_calls"].as_array().is_some_and(|calls| calls.iter().any(|t| tool_name_matches(t, tool))))
    });
    in_tools || in_messages
}

/// Имя функции в элементе `tools`/`tool_calls` совпадает с ожидаемым.
fn tool_name_matches(value: &Value, tool: &str) -> bool {
    value["function"]["name"].as_str() == Some(tool)
}

/// JSON-ответ об ошибке с нужным статусом (форма `{"error": {...}}` как у OpenAI).
fn error_response(status: u16, kind: &str, message: &str) -> String {
    let body = json!({"error": {"message": message, "type": kind}}).to_string();
    http_response(status, "application/json", &body)
}

/// Ответ 200 `text/event-stream` с готовым SSE-телом.
fn ok_sse(body: &str) -> String {
    http_response(200, "text/event-stream", body)
}

/// Собрать сырой HTTP/1.1-ответ; `Connection: close` — keep-alive не держим.
fn http_response(status: u16, content_type: &str, body: &str) -> String {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let length = body.len();
    format!("HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n{body}")
}

/// Сериализовать сценарий в SSE-тело: по кадру на дельту, терминальный кадр
/// с `finish_reason` и usage, затем `data: [DONE]`. Первый кадр несёт
/// `role: "assistant"` (как реальный OpenAI-поток).
fn scenario_to_sse(scenario: &Scenario, model: &str, prompt_tokens: u64) -> String {
    let mut out = String::new();
    let mut tool_index = 0u64;
    for (i, delta) in scenario.deltas.iter().enumerate() {
        let first = i == 0;
        let delta_json = match delta {
            Delta::Text(text) => {
                if first {
                    json!({"role": "assistant", "content": text})
                } else {
                    json!({"content": text})
                }
            }
            Delta::ToolCall { name, arguments } => {
                let index = tool_index;
                tool_index += 1;
                let id = format!("call_{index}");
                let call = json!({
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                });
                if first {
                    json!({"role": "assistant", "content": null, "tool_calls": [call]})
                } else {
                    json!({"tool_calls": [call]})
                }
            }
        };
        out.push_str("data: ");
        out.push_str(&chunk_json(delta_json, Value::Null, model).to_string());
        out.push_str("\n\n");
    }
    let completion_tokens = scenario.deltas.len() as u64;
    let terminal = json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000,
        "model": model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": scenario.finish}],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        },
    });
    out.push_str("data: ");
    out.push_str(&terminal.to_string());
    out.push_str("\n\n");
    out.push_str("data: [DONE]\n\n");
    out
}

/// Один кадр `chat.completion.chunk` (форма OpenAI) с заданной дельтой.
fn chunk_json(delta: Value, finish_reason: Value, model: &str) -> Value {
    json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
    })
}

// ---------------------------------------------------------------------------
// Фасад совместимости для интеграционных тестов бинарника (tests/integration_cli.rs):
// простой контракт MockServer/MockResponse/RecordedRequest поверх MockLlm/Scenario.
// ---------------------------------------------------------------------------

/// Записанный сырой запрос (для asserts по телу/пути).
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// HTTP-метод (`POST`).
    pub method: String,
    /// Путь запроса (`/v1/chat/completions`).
    pub path: String,
    /// Тело запроса строкой (JSON), пусто при отсутствии тела.
    pub body: String,
}

/// Один заранее заготовленный ответ сервера (конвертируется в [`Scenario`]).
#[derive(Debug, Clone)]
pub struct MockResponse {
    deltas: Vec<Delta>,
    finish: String,
}

impl MockResponse {
    /// Текстовый ответ ассистента (finish_reason = "stop").
    pub fn text(text: &str) -> Self {
        Self {
            deltas: vec![Delta::Text(text.to_string())],
            finish: "stop".to_string(),
        }
    }

    /// Ответ-вызов инструмента (finish_reason = "tool_calls").
    pub fn tool_call(name: &str, args_json: &str) -> Self {
        Self {
            deltas: vec![Delta::ToolCall {
                name: name.to_string(),
                arguments: args_json.to_string(),
            }],
            finish: "tool_calls".to_string(),
        }
    }

    /// Переопределить finish_reason (например `"length"` для теста эскалации).
    #[must_use]
    pub fn with_finish_reason(mut self, reason: &str) -> Self {
        self.finish = reason.to_string();
        self
    }

    fn into_scenario(self) -> Scenario {
        let mut scenario = Scenario::new().finish_reason(self.finish);
        scenario.deltas = self.deltas;
        scenario
    }
}

/// Простой хендл мок-сервера для интеграционных тестов бинарника.
///
/// Отличия от [`MockLlm`]: мягкий режим (при исчерпании очереди отвечает
/// текстом-заглушкой, а не 500 — харнесс шлёт внеплановые запросы после
/// `finish`) и сырой журнал запросов с методом/путём/телом.
#[derive(Debug)]
pub struct MockServer {
    handle: MockHandle,
}

impl MockServer {
    /// Поднять сервер на эфемерном порту с очередью готовых ответов.
    ///
    /// # Errors
    /// Ошибка `io`, если не удалось занять локальный сокет.
    pub fn start(responses: Vec<MockResponse>) -> io::Result<Self> {
        let scenarios: Vec<Scenario> = responses
            .into_iter()
            .map(MockResponse::into_scenario)
            .collect();
        let handle = MockLlm::with_scenarios(scenarios).serve_on_ephemeral()?;
        handle.shared.lenient.store(true, Ordering::SeqCst);
        Ok(Self { handle })
    }

    /// Порт, на котором слушает сервер.
    pub fn port(&self) -> u16 {
        self.handle.local_addr.port()
    }

    /// Снимок сырого журнала запросов (метод/путь/тело) в порядке получения.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock_recover(&self.handle.shared.raw_requests).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ApiClient, Message};

    /// POST JSON на указанный URL (клиент с таймаутом, чтобы тест не висел).
    fn post(url: &str, body: &Value) -> reqwest::blocking::Response {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("клиент собирается");
        client.post(url).json(body).send().expect("POST выполняется")
    }

    fn chat_url(handle: &MockHandle) -> String {
        format!("{}/chat/completions", handle.base_url)
    }

    /// Тело типового потокового chat-запроса.
    fn chat_body(model: &str) -> Value {
        json!({"model": model, "messages": [{"role": "user", "content": "привет"}], "stream": true})
    }

    /// Разобрать SSE-тело: (JSON-кадры, был ли кадр [DONE]).
    fn parse_sse(body: &str) -> (Vec<Value>, bool) {
        let mut frames = Vec::new();
        let mut done = false;
        for line in body.lines() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                done = true;
            } else {
                frames.push(serde_json::from_str(data).expect("каждый кадр — валидный JSON"));
            }
        }
        (frames, done)
    }

    /// Клиент theseus против мока — связка, ради которой мок и писался.
    fn theseus_client(base_url: &str) -> ApiClient {
        ApiClient::new(base_url, "test-key", "mock-model", 30, json!({}), 1024).expect("клиент создаётся")
    }

    #[test]
    fn text_reply_streams_content_and_stop() {
        let handle = MockLlm::with_scenarios(vec![Scenario::new().reply_text("Привет, мир!")])
            .serve_on_ephemeral()
            .unwrap();
        let resp = post(&chat_url(&handle), &chat_body("test-model"));
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"].to_str().unwrap(), "text/event-stream");
        let (frames, done) = parse_sse(&resp.text().unwrap());
        assert!(done, "поток завершается [DONE]");
        assert_eq!(frames.len(), 2, "контентный + терминальный кадры");
        assert_eq!(frames[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(frames[0]["choices"][0]["delta"]["content"], "Привет, мир!");
        assert!(frames[0]["choices"][0]["finish_reason"].is_null());
        assert_eq!(frames[1]["choices"][0]["finish_reason"], "stop");
        assert_eq!(frames[1]["usage"]["prompt_tokens"], 1, "одно сообщение в запросе");
        assert_eq!(frames[1]["usage"]["completion_tokens"], 1, "одна дельта");
        handle.assert_request_count(1);
        handle.join();
    }

    #[test]
    fn tool_call_reply_carries_function_and_finish_reason() {
        let handle = MockLlm::with_scenarios(vec![
            Scenario::new()
                .reply_tool_call("bash", "{\"command\": \"ls -la\"}")
                .finish_reason("tool_calls"),
        ])
        .serve_on_ephemeral()
        .unwrap();
        let resp = post(&chat_url(&handle), &chat_body("m"));
        assert_eq!(resp.status(), 200);
        let (frames, done) = parse_sse(&resp.text().unwrap());
        assert!(done);
        assert_eq!(frames.len(), 2);
        let call = &frames[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["index"], 0);
        assert_eq!(call["id"], "call_0");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "bash");
        assert_eq!(call["function"]["arguments"], "{\"command\": \"ls -la\"}");
        assert_eq!(frames[1]["choices"][0]["finish_reason"], "tool_calls");
        handle.join();
    }

    #[test]
    fn streamed_chunks_arrive_in_order_and_concatenate() {
        let handle = MockLlm::with_scenarios(vec![Scenario::new().reply_stream(&["Раз", " ", "два", "!"])])
            .serve_on_ephemeral()
            .unwrap();
        let resp = post(&chat_url(&handle), &chat_body("m"));
        let (frames, done) = parse_sse(&resp.text().unwrap());
        assert!(done);
        assert_eq!(frames.len(), 5, "4 контентных кадра + терминальный");
        let text: String = frames[..4]
            .iter()
            .map(|f| f["choices"][0]["delta"]["content"].as_str().unwrap())
            .collect();
        assert_eq!(text, "Раз два!");
        assert_eq!(frames[0]["choices"][0]["delta"]["role"], "assistant");
        assert!(frames[1]["choices"][0]["delta"]["role"].is_null(), "роль только в первом кадре");
        assert!(frames[..4].iter().all(|f| f["choices"][0]["finish_reason"].is_null()));
        assert_eq!(frames[4]["usage"]["completion_tokens"], 4);
        handle.join();
    }

    #[test]
    fn queue_serves_scenarios_in_request_order() {
        let handle = MockLlm::with_scenarios(vec![
            Scenario::new().reply_text("первый"),
            Scenario::new().reply_text("второй"),
        ])
        .serve_on_ephemeral()
        .unwrap();
        let first = post(&chat_url(&handle), &chat_body("m"));
        let second = post(&chat_url(&handle), &chat_body("m"));
        let (frames_first, _) = parse_sse(&first.text().unwrap());
        let (frames_second, _) = parse_sse(&second.text().unwrap());
        assert_eq!(frames_first[0]["choices"][0]["delta"]["content"], "первый");
        assert_eq!(frames_second[0]["choices"][0]["delta"]["content"], "второй");
        handle.assert_request_count(2);
        handle.join();
    }

    #[test]
    fn requests_are_recorded_with_bodies() {
        let handle = MockLlm::with_scenarios(vec![Scenario::new().reply_text("ok")])
            .serve_on_ephemeral()
            .unwrap();
        let body = json!({
            "model": "rec-model",
            "messages": [
                {"role": "system", "content": "s"},
                {"role": "user", "content": "u"},
            ],
            "max_tokens": 128,
        });
        let resp = post(&chat_url(&handle), &body);
        assert_eq!(resp.status(), 200);
        let requests = handle.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["model"], "rec-model");
        assert_eq!(requests[0]["messages"].as_array().unwrap().len(), 2);
        assert_eq!(requests[0]["max_tokens"], 128);
        handle.assert_request_count(1);
        handle.join();
    }

    #[test]
    fn unknown_path_returns_404() {
        let handle = MockLlm::new().serve_on_ephemeral().unwrap();
        let resp = post(&format!("{}/v1/models", handle.base_url), &json!({"probe": 1}));
        assert_eq!(resp.status(), 404);
        let error: Value = serde_json::from_str(&resp.text().unwrap()).unwrap();
        assert_eq!(error["error"]["type"], "not_found");
        assert!(error["error"]["message"].as_str().unwrap().contains("/v1/models"));
        // даже неизвестные пути пишутся в журнал (см. doc MockHandle::requests)
        handle.assert_request_count(1);
        handle.join();
    }

    #[test]
    fn exhausted_queue_returns_500() {
        let handle = MockLlm::new().serve_on_ephemeral().unwrap();
        let resp = post(&chat_url(&handle), &chat_body("m"));
        assert_eq!(resp.status(), 500);
        let error: Value = serde_json::from_str(&resp.text().unwrap()).unwrap();
        assert_eq!(error["error"]["type"], "mock_exhausted");
        handle.assert_request_count(1);
        handle.join();
    }

    #[test]
    fn expect_tool_call_passes_when_tool_advertised() {
        let handle = MockLlm::with_scenarios(vec![Scenario::new().expect_tool_call("bash").reply_text("вижу bash")])
            .serve_on_ephemeral()
            .unwrap();
        let mut body = chat_body("m");
        body["tools"] = json!([{"type": "function", "function": {"name": "bash", "parameters": {}}}]);
        let resp = post(&chat_url(&handle), &body);
        assert_eq!(resp.status(), 200);
        handle.join();
    }

    #[test]
    fn expect_tool_call_passes_when_tool_in_history() {
        let handle = MockLlm::with_scenarios(vec![Scenario::new().expect_tool_call("bash").reply_text("продолжаем")])
            .serve_on_ephemeral()
            .unwrap();
        let body = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "u"},
                {"role": "assistant", "tool_calls": [
                    {"id": "call_0", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
                ]},
                {"role": "tool", "tool_call_id": "call_0", "content": "ok"},
            ],
        });
        let resp = post(&chat_url(&handle), &body);
        assert_eq!(resp.status(), 200);
        handle.join();
    }

    #[test]
    fn expect_tool_call_fails_when_tool_missing() {
        let handle = MockLlm::with_scenarios(vec![Scenario::new().expect_tool_call("bash").reply_text("не дождётся")])
            .serve_on_ephemeral()
            .unwrap();
        let resp = post(&chat_url(&handle), &chat_body("m"));
        assert_eq!(resp.status(), 400);
        let error: Value = serde_json::from_str(&resp.text().unwrap()).unwrap();
        assert_eq!(error["error"]["type"], "expectation_failed");
        assert!(error["error"]["message"].as_str().unwrap().contains("bash"));
        handle.join();
    }

    #[test]
    fn finish_reason_length_propagates_to_terminal_chunk() {
        let handle = MockLlm::with_scenarios(vec![Scenario::new().reply_text("обрезано").finish_reason("length")])
            .serve_on_ephemeral()
            .unwrap();
        let resp = post(&chat_url(&handle), &chat_body("m"));
        let (frames, done) = parse_sse(&resp.text().unwrap());
        assert!(done);
        assert_eq!(frames[1]["choices"][0]["finish_reason"], "length");
        handle.join();
    }

    #[test]
    fn empty_scenario_emits_terminal_chunk_only() {
        let handle = MockLlm::with_scenarios(vec![Scenario::new()])
            .serve_on_ephemeral()
            .unwrap();
        let resp = post(&chat_url(&handle), &chat_body("m"));
        let (frames, done) = parse_sse(&resp.text().unwrap());
        assert!(done);
        assert_eq!(frames.len(), 1, "только терминальный кадр");
        assert_eq!(frames[0]["choices"][0]["delta"], json!({}));
        assert_eq!(frames[0]["choices"][0]["finish_reason"], "stop");
        assert_eq!(frames[0]["usage"]["completion_tokens"], 0);
        handle.join();
    }

    #[test]
    fn end_to_end_streaming_text_with_theseus_client() {
        let handle = MockLlm::with_scenarios(vec![Scenario::new().reply_stream(&["Ответ", " ", "мока"])])
            .serve_on_ephemeral()
            .unwrap();
        let mut client = theseus_client(&handle.base_url);
        let mut streamed = String::new();
        let response = client
            .chat_stream(&[Message::user("вопрос")], &json!(null), &mut |piece| streamed.push_str(piece), &|| false)
            .expect("chat_stream отрабатывает");
        assert_eq!(streamed, "Ответ мока");
        assert_eq!(response.content.as_deref(), Some("Ответ мока"));
        assert_eq!(response.finish_reason.as_deref(), Some("stop"));
        assert_eq!(response.prompt_tokens, 1);
        assert_eq!(response.completion_tokens, 3, "три текстовые дельты");
        assert!(!response.aborted);
        assert_eq!(client.accounting.calls, 1);
        handle.assert_request_count(1);
        handle.join();
    }

    #[test]
    fn end_to_end_tool_call_with_theseus_client() {
        let handle = MockLlm::with_scenarios(vec![
            Scenario::new()
                .expect_tool_call("bash")
                .reply_tool_call("bash", "{\"command\": \"ls\"}")
                .finish_reason("tool_calls"),
        ])
        .serve_on_ephemeral()
        .unwrap();
        let mut client = theseus_client(&handle.base_url);
        let tools = json!([{"type": "function", "function": {"name": "bash", "parameters": {"type": "object"}}}]);
        let response = client
            .chat_stream(&[Message::user("покажи файлы")], &tools, &mut |_| {}, &|| false)
            .expect("chat_stream отрабатывает");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_0");
        assert_eq!(response.tool_calls[0].function.name, "bash");
        assert_eq!(response.tool_calls[0].function.arguments, "{\"command\": \"ls\"}");
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
        assert!(response.content.is_none(), "контента нет — только вызов");
        // клиент действительно объявил инструмент моку
        assert_eq!(handle.requests()[0]["tools"][0]["function"]["name"], "bash");
        handle.join();
    }

    #[test]
    fn multiple_tool_calls_get_distinct_indices_and_ids() {
        let handle = MockLlm::with_scenarios(vec![
            Scenario::new()
                .reply_tool_call("read_file", "{\"path\": \"a.rs\"}")
                .reply_tool_call("write_file", "{\"path\": \"b.rs\"}")
                .finish_reason("tool_calls"),
        ])
        .serve_on_ephemeral()
        .unwrap();
        let mut client = theseus_client(&handle.base_url);
        let response = client
            .chat_stream(&[Message::user("два вызова")], &json!(null), &mut |_| {}, &|| false)
            .expect("chat_stream отрабатывает");
        let names: Vec<&str> = response.tool_calls.iter().map(|t| t.function.name.as_str()).collect();
        assert_eq!(names, ["read_file", "write_file"]);
        assert_eq!(response.tool_calls[0].id, "call_0");
        assert_eq!(response.tool_calls[1].id, "call_1");
        handle.join();
    }
}
