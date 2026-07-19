//! Субагент «Ариадна»: локальная модель `qwen3.5-4b-ariadna-grpo` (GGUF)
//! через `llama-server` из llama.cpp.
//!
//! «Ариадна» — дообученная (CPT → SFT → GRPO) локальная модель, которую
//! харнесс использует как дешёвого субагента-проводника (разведка кода,
//! черновые суммаризации, классификация). Раздаёт её `llama-server`
//! с OpenAI-совместимым API (`/v1/chat/completions`, `/health`).
//!
//! Жизненный цикл сервера — через [`ensure_server`]:
//!
//! - `/health` уже отвечает → сервер **переиспользуется**; гард процессом
//!   не владеет и при `Drop` чужой сервер **не трогает**;
//! - иначе spawn `llama-server --model <gguf> --host <h> --port <p> -c <ctx>
//!   --n-gpu-layers <N>`, опрос `/health` каждые 500 мс до `startup_timeout`;
//!   такой гард при `Drop` убивает **только порождённый** процесс;
//! - таймаут/досрочная смерть → ошибка с хвостом лога (stdout+stderr
//!   сервера пишутся во временный файл).
//!
//! # Пример
//!
//! ```no_run
//! use theseus::ariadna::{AriadnaConfig, ensure_server, run_task};
//!
//! let cfg = AriadnaConfig::default();
//! let _guard = ensure_server(&cfg)?; // поднять или переиспользовать
//! let answer = run_task(&cfg, "Ты — Ариадна, проводник по коду.", "Опиши src/api.rs")?;
//! println!("{answer}");
//! # Ok::<(), anyhow::Error>(())
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Имя модели в запросах: llama-server его игнорирует (модель одна),
/// но OpenAI-совместимое поле `model` обязано присутствовать.
const MODEL_NAME: &str = "ariadna";
/// Детерминированность субагента важнее креативности.
const CHAT_TEMPERATURE: f64 = 0.2;
/// Потолок токенов ответа: модель с thinking-режимом тратит заметную часть
/// бюджета на блок <think> — при 1024 ответ иногда не начинался вовсе
/// (живой кейс 18.07: два запроса вернули один лишь «<think»).
const CHAT_MAX_TOKENS: u32 = 3072;

/// Срезать thinking-блоки из ответа модели: `<think>...</think>` целиком и
/// незакрытый хвост `<think>...` (обрезка по бюджету токенов). Если после
/// среза текста нет — честный маркер вместо пустой строки.
fn strip_think_blocks(content: &str) -> String {
    let mut out = String::new();
    let mut rest = content;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        rest = match rest[start + 7..].find("</think>") {
            Some(end) => &rest[start + 7 + end + 8..],
            None => "", // незакрытый think — отбрасываем весь хвост
        };
    }
    out.push_str(rest);
    let trimmed = out.trim();
    if trimmed.is_empty() {
        "(Ариадна вернула только thinking-блок без текста ответа; \
         попробуйте переформулировать задачу короче)".to_string()
    } else {
        trimmed.to_string()
    }
}
/// Путь к бинарю llama-server на машине разработки.
const DEFAULT_SERVER_BIN: &str = "/home/roman/llama.cpp/build/bin/llama-server";
/// Путь к GGUF-файлу дообученной модели на машине разработки.
const DEFAULT_GGUF_PATH: &str =
    "/home/roman/models/ariadna-grpo/qwen35-4b-ariadna-grpo-v10.Q4_K_M.gguf";
/// Поднимаем сервер только на loopback: внешних клиентов у субагента нет.
const DEFAULT_HOST: &str = "127.0.0.1";
/// Порт по умолчанию (вне типовых 8000/8080, чтобы не воевать с другими сервисами).
const DEFAULT_PORT: u16 = 8399;
/// Размер контекста модели в токенах.
const DEFAULT_CTX: u32 = 8192;
/// Сколько слоёв уносить на GPU (99 = «всё, что помещается» в VRAM).
const DEFAULT_GPU_LAYERS: u32 = 99;
/// Сколько ждать готовности сервера после spawn по умолчанию.
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Период опроса `/health` при ожидании старта.
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Таймаут одного пробного `/health` (мёртвый loopback-порт отказывает мгновенно).
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Таймаут chat-запроса: генерация на 4B — десятки секунд, на CPU — минуты.
const CHAT_TIMEOUT: Duration = Duration::from_secs(600);
/// Таймаут установления TCP-соединения.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Сколько байт с конца лога сервера прикладывать к ошибке старта.
const LOG_TAIL_BYTES: usize = 4096;
/// Потолок длины тела ошибки, вставляемого в текст ошибки (символов).
const ERR_BODY_MAX_CHARS: usize = 512;

/// Конфигурация субагента «Ариадна». Дефолт ([`AriadnaConfig::default`])
/// соответствует машине разработки: бинарь llama.cpp, GGUF v10 (GRPO),
/// loopback:8399, контекст 8k, все слои на GPU, старт до 30 секунд.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AriadnaConfig {
    /// Путь к исполняемому файлу `llama-server`.
    pub server_bin: PathBuf,
    /// Путь к GGUF-файлу модели.
    pub gguf_path: PathBuf,
    /// Хост привязки сервера (по умолчанию `127.0.0.1`).
    pub host: String,
    /// Порт привязки сервера (по умолчанию `8399`).
    pub port: u16,
    /// Размер контекста (`-c`).
    pub ctx: u32,
    /// Слои на GPU (`--n-gpu-layers`).
    pub gpu_layers: u32,
    /// Сколько ждать готовности `/health` после spawn.
    pub startup_timeout: Duration,
}

impl Default for AriadnaConfig {
    fn default() -> Self {
        Self {
            server_bin: PathBuf::from(DEFAULT_SERVER_BIN),
            gguf_path: PathBuf::from(DEFAULT_GGUF_PATH),
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            ctx: DEFAULT_CTX,
            gpu_layers: DEFAULT_GPU_LAYERS,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
        }
    }
}

impl AriadnaConfig {
    /// Базовый URL сервера (`http://host:port`) без завершающего слэша.
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }
}

/// Одно сообщение диалога в OpenAI-формате (`role` + `content`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Роль автора: `system`, `user` или `assistant`.
    pub role: String,
    /// Текст сообщения.
    pub content: String,
}

impl ChatMessage {
    fn new(role: &str, content: impl Into<String>) -> Self {
        Self { role: role.to_string(), content: content.into() }
    }

    /// Системное сообщение (инструкция для модели).
    pub fn system(content: impl Into<String>) -> Self {
        Self::new("system", content)
    }

    /// Сообщение пользователя (сама задача).
    pub fn user(content: impl Into<String>) -> Self {
        Self::new("user", content)
    }

    /// Сообщение ассистента (для многоходовых диалогов/few-shot).
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new("assistant", content)
    }
}

/// RAII-гард живого llama-server'а.
///
/// Инвариант владения: гард убивает процесс при `Drop`, **только если сам
/// его породил** ([`owns_process`](Self::owns_process) == `true`); гард
/// переиспользованного («чужого») сервера при `Drop` не делает ничего.
#[derive(Debug)]
pub struct ServerGuard {
    /// Порождённый нами процесс; `None` — сервер был переиспользован.
    child: Option<Child>,
    /// Лог stdout+stderr (только для порождённого сервера).
    log_path: Option<PathBuf>,
    /// Базовый URL, на котором сервер отвечает.
    base_url: String,
}

impl ServerGuard {
    /// Гард для уже живого сервера: процессом не владеем.
    fn reused(base_url: String) -> Self {
        Self { child: None, log_path: None, base_url }
    }

    /// Гард для порождённого нами процесса.
    fn spawned(child: Child, log_path: PathBuf, base_url: String) -> Self {
        Self { child: Some(child), log_path: Some(log_path), base_url }
    }

    /// `true`, если сервер порождён [`ensure_server`] и будет убит при `Drop`.
    pub fn owns_process(&self) -> bool {
        self.child.is_some()
    }

    /// Базовый URL сервера (`http://host:port`).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Путь к лог-файлу сервера для порождённого процесса; для
    /// переиспользованного — `None` (его логи нам не принадлежат).
    pub fn log_path(&self) -> Option<&Path> {
        self.log_path.as_deref()
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // kill + wait: без wait остаётся зомби в таблице процессов.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Доступна ли «Ариадна» на этой машине: существуют ли бинарь
/// `llama-server` и GGUF-файл модели. Дешёвая предпроверка для doctor-а
/// и для решения «роутить ли задачу на локального субагента».
pub fn is_available(cfg: &AriadnaConfig) -> bool {
    cfg.server_bin.is_file() && cfg.gguf_path.is_file()
}

/// Гарантирует работающий llama-server и возвращает [`ServerGuard`].
///
/// Порядок действий: `/health` отвечает 2xx → переиспользовать (гард без
/// процесса); нет бинаря/GGUF → сразу ошибка; иначе spawn с логом во
/// временный файл и опрос `/health` каждые 500 мс до `startup_timeout`.
///
/// # Errors
/// Ошибка, если: нет бинаря/модели; процесс не удалось запустить; процесс
/// умер до готовности; `/health` не ответил за `startup_timeout` (процесс
/// при этом убивается). К ошибкам старта прикладывается хвост лога сервера.
pub fn ensure_server(cfg: &AriadnaConfig) -> Result<ServerGuard> {
    let base_url = cfg.base_url();
    let probe = http_client(HEALTH_PROBE_TIMEOUT)?;
    if health_ok(&probe, &base_url) {
        return Ok(ServerGuard::reused(base_url));
    }
    if !cfg.server_bin.is_file() {
        anyhow::bail!("бинарь llama-server не найден: {}", cfg.server_bin.display());
    }
    if !cfg.gguf_path.is_file() {
        anyhow::bail!("GGUF-модель не найдена: {}", cfg.gguf_path.display());
    }

    let log_path = log_path_for(cfg);
    let log_out = std::fs::File::create(&log_path)
        .with_context(|| format!("создание лог-файла {}", log_path.display()))?;
    let log_err = log_out.try_clone().context("клонирование дескриптора лог-файла")?;
    let mut child = Command::new(&cfg.server_bin)
        .arg("--model")
        .arg(&cfg.gguf_path)
        .arg("--host")
        .arg(&cfg.host)
        .arg("--port")
        .arg(cfg.port.to_string())
        .arg("-c")
        .arg(cfg.ctx.to_string())
        .arg("--n-gpu-layers")
        .arg(cfg.gpu_layers.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err))
        .spawn()
        .with_context(|| format!("запуск {}", cfg.server_bin.display()))?;

    let deadline = Instant::now() + cfg.startup_timeout;
    loop {
        if health_ok(&probe, &base_url) {
            return Ok(ServerGuard::spawned(child, log_path, base_url));
        }
        if let Some(status) = child.try_wait().context("опрос состояния llama-server")? {
            let tail = read_log_tail(&log_path);
            anyhow::bail!(
                "llama-server завершился досрочно (статус: {status}). Хвост лога {}:\n{tail}",
                log_path.display()
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let tail = read_log_tail(&log_path);
            anyhow::bail!(
                "llama-server не поднялся за {:?} ({}). Хвост лога {}:\n{tail}",
                cfg.startup_timeout,
                base_url,
                log_path.display()
            );
        }
        std::thread::sleep(HEALTH_POLL_INTERVAL);
    }
}

/// Один chat-completion запрос к уже работающему серверу.
///
/// Сервер должен быть поднят заранее (см. [`ensure_server`]) — функция
/// сама сервер не стартует. Тело запроса: модель `ariadna`,
/// `temperature` 0.2, `max_tokens` 3072, плюс `chat_template_kwargs.
/// enable_thinking=false` — иначе GRPO-модель генерирует многосотенные
/// thinking-токены на простые вопросы (живой замер: 2500+ токенов на
/// однострочный вопрос, ~10 минут генерации). Неподдерживаемые сервером
/// kwargs игнорируются без ошибок.
///
/// # Errors
/// Ошибка транспорта (сервер недоступен), не-2xx статус (в текст ошибки
/// включается тело ответа), битый JSON, отсутствие `choices[0].message.content`.
pub fn chat(cfg: &AriadnaConfig, messages: &[ChatMessage]) -> Result<String> {
    let url = format!("{}/v1/chat/completions", cfg.base_url());
    let body = serde_json::json!({
        "model": MODEL_NAME,
        "messages": messages,
        "temperature": CHAT_TEMPERATURE,
        "max_tokens": CHAT_MAX_TOKENS,
        "chat_template_kwargs": { "enable_thinking": false },
    });
    let client = http_client(CHAT_TIMEOUT)?;
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().unwrap_or_default();
        anyhow::bail!("llama-server ответил {status}: {}", truncate(&text, ERR_BODY_MAX_CHARS));
    }
    let value: serde_json::Value =
        resp.json().with_context(|| format!("разбор JSON ответа {url}"))?;
    // Индексация Value не паникует: промах даёт Null, as_str → None.
    let content = value["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "в ответе llama-server нет choices[0].message.content: {}",
                truncate(&value.to_string(), ERR_BODY_MAX_CHARS)
            )
        })?;
    Ok(strip_think_blocks(content))
}

/// Разовая задача для «Ариадны»: гарантирует сервер и выполняет один
/// запрос `system + user`. Если сервер не был поднят заранее, он будет
/// порождён здесь и **убит по выходе** (гард живёт до конца вызова) —
/// для серии задач выгоднее самим вызвать [`ensure_server`], держать
/// гард и ходить в [`chat`] напрямую.
///
/// # Errors
/// Сумма ошибок [`ensure_server`] и [`chat`].
pub fn run_task(cfg: &AriadnaConfig, system: &str, task: &str) -> Result<String> {
    let _guard = ensure_server(cfg)?;
    chat(cfg, &[ChatMessage::system(system), ChatMessage::user(task)])
}

/// Жив ли сервер: `/health` отвечает успешным статусом. Любая ошибка
/// транспорта (отказ в соединении, таймаут) трактуется как «не жив».
fn health_ok(client: &reqwest::blocking::Client, base_url: &str) -> bool {
    match client.get(format!("{base_url}/health")).send() {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// HTTP-клиент с таймаутом целиком на запрос и отдельным на connect.
fn http_client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        .build()
        .context("создание HTTP-клиента reqwest")
}

/// Путь лог-файла порождённого сервера (pid + port против коллизий).
fn log_path_for(cfg: &AriadnaConfig) -> PathBuf {
    std::env::temp_dir().join(format!(
        "theseus-ariadna-server-{}-{}.log",
        std::process::id(),
        cfg.port
    ))
}

/// Последние [`LOG_TAIL_BYTES`] байт лога сервера для диагностики.
/// Разрыв UTF-8 на границе допустим: `from_utf8_lossy` подставит U+FFFD.
fn read_log_tail(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) if bytes.is_empty() => "(лог пуст)".to_string(),
        Ok(bytes) => {
            let start = bytes.len().saturating_sub(LOG_TAIL_BYTES);
            String::from_utf8_lossy(&bytes[start..]).into_owned()
        }
        Err(err) => format!("(не удалось прочитать лог {}: {err})", path.display()),
    }
}

/// Усекает строку до `max_chars` символов, добавляя «…», если хвост отрезан.
fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() { format!("{head}…") } else { head }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    /// Текст, который мок кладёт в `choices[0].message.content`.
    const MOCK_REPLY: &str = "мок-ответ ариадны";

    /// Режим мока: что отвечать на `POST /v1/chat/completions`.
    #[derive(Debug, Clone, Copy)]
    enum ChatMode {
        /// Корректный OpenAI-совместимый ответ с [`MOCK_REPLY`].
        Ok,
        /// HTTP 500 с JSON-ошибкой.
        Http500,
        /// HTTP 200, но тело — не JSON вовсе.
        InvalidJson,
        /// HTTP 200, валидный JSON, но без нужных полей.
        EmptyChoices,
    }

    /// Разобранный HTTP-запрос, пришедший на мок.
    #[derive(Debug, Clone)]
    struct CapturedRequest {
        method: String,
        path: String,
        body: String,
    }

    /// Мини-HTTP-мок llama-server'а на std::TcpListener (свой, не mock_sse):
    /// одно соединение — один запрос, ответ с `Connection: close`.
    struct MockLlama {
        port: u16,
        captured: Arc<Mutex<Vec<CapturedRequest>>>,
        shutdown: Arc<AtomicBool>,
        join: Option<JoinHandle<()>>,
    }

    impl MockLlama {
        fn start(mode: ChatMode) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind мока");
            listener.set_nonblocking(true).expect("nonblocking мока");
            let port = listener.local_addr().expect("local_addr мока").port();
            let captured = Arc::new(Mutex::new(Vec::new()));
            let shutdown = Arc::new(AtomicBool::new(false));
            let join = {
                let captured = Arc::clone(&captured);
                let shutdown = Arc::clone(&shutdown);
                std::thread::spawn(move || serve(listener, captured, shutdown, mode))
            };
            Self { port, captured, shutdown, join: Some(join) }
        }

        /// Конфиг на мок (пути бинаря/модели дефолтные: до их проверки дело не доходит).
        fn config(&self) -> AriadnaConfig {
            AriadnaConfig {
                host: "127.0.0.1".to_string(),
                port: self.port,
                startup_timeout: Duration::from_secs(2),
                ..AriadnaConfig::default()
            }
        }

        /// Все захваченные запросы (копия журнала).
        fn captured(&self) -> Vec<CapturedRequest> {
            self.captured.lock().expect("lock captured").clone()
        }

        /// Только chat-запросы из журнала.
        fn chat_requests(&self) -> Vec<CapturedRequest> {
            self.captured()
                .into_iter()
                .filter(|r| r.method == "POST" && r.path == "/v1/chat/completions")
                .collect()
        }
    }

    impl Drop for MockLlama {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Relaxed);
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    /// Цикл приёма соединений мока (неблокирующий accept + флаг остановки).
    fn serve(
        listener: TcpListener,
        captured: Arc<Mutex<Vec<CapturedRequest>>>,
        shutdown: Arc<AtomicBool>,
        mode: ChatMode,
    ) {
        while !shutdown.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    match read_request(&stream) {
                        Ok(req) => {
                            let (status, reason, body) = route(&req, mode);
                            captured.lock().expect("lock captured").push(req);
                            let _ = write_response(&mut stream, status, reason, &body);
                        }
                        Err(_) => { /* полуоткрытое/битое соединение — пропускаем */ }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    }

    /// Маршрутизация запроса моком.
    fn route(req: &CapturedRequest, mode: ChatMode) -> (u16, &'static str, String) {
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/health") => (200, "OK", json!({"status": "ok"}).to_string()),
            ("POST", "/v1/chat/completions") => match mode {
                ChatMode::Ok => (
                    200,
                    "OK",
                    json!({
                        "id": "chatcmpl-mock",
                        "object": "chat.completion",
                        "created": 0,
                        "model": "ariadna",
                        "choices": [{"index": 0, "finish_reason": "stop",
                            "message": {"role": "assistant", "content": MOCK_REPLY}}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                    .to_string(),
                ),
                ChatMode::Http500 => {
                    (500, "Internal Server Error", json!({"error": "boom"}).to_string())
                }
                ChatMode::InvalidJson => (200, "OK", "это вовсе не json".to_string()),
                ChatMode::EmptyChoices => (200, "OK", json!({"choices": []}).to_string()),
            },
            _ => (404, "Not Found", json!({"error": "not found"}).to_string()),
        }
    }

    /// Читает один HTTP-запрос: стартовая строка, заголовки, тело по
    /// `Content-Length` (reqwest для json-тела всегда его шлёт).
    fn read_request(stream: &TcpStream) -> std::io::Result<CapturedRequest> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "пустой запрос"));
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut buf = vec![0u8; content_length];
        reader.read_exact(&mut buf)?;
        Ok(CapturedRequest {
            method,
            path,
            body: String::from_utf8_lossy(&buf).into_owned(),
        })
    }

    /// Пишет HTTP-ответ с `Connection: close` (keep-alive не держим).
    fn write_response(stream: &mut TcpStream, status: u16, reason: &str, body: &str) -> std::io::Result<()> {
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(body.as_bytes())?;
        stream.flush()
    }

    /// Свободный loopback-порт (гонка маловероятна и для наших проверок безвредна).
    fn free_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0))
            .expect("bind :0")
            .local_addr()
            .expect("local_addr")
            .port()
    }

    /// Временный исполняемый shell-скрипт — «поддельный llama-server»
    /// (лишние аргументы командной строки скрипт просто игнорирует).
    fn make_script(name: &str, body: &str) -> PathBuf {
        let path = std::env::temp_dir()
            .join(format!("theseus-ariadna-test-{}-{name}.sh", std::process::id()));
        std::fs::write(&path, body).expect("запись скрипта");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod скрипта");
        }
        path
    }

    /// Временный файл с заданным содержимым (притворяется GGUF/бинарём).
    fn make_temp_file(name: &str, content: &[u8]) -> PathBuf {
        let path = std::env::temp_dir()
            .join(format!("theseus-ariadna-test-{}-{name}", std::process::id()));
        std::fs::write(&path, content).expect("запись файла");
        path
    }

    #[test]
    fn default_config_values() {
        let cfg = AriadnaConfig::default();
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 8399);
        assert_eq!(cfg.ctx, 8192);
        assert_eq!(cfg.gpu_layers, 99);
        assert_eq!(cfg.startup_timeout, Duration::from_secs(30));
        assert!(cfg.server_bin.ends_with("llama-server"));
        assert!(cfg.gguf_path.ends_with("qwen35-4b-ariadna-grpo-v10.Q4_K_M.gguf"));
    }

    #[test]
    fn base_url_uses_host_and_port() {
        let cfg = AriadnaConfig {
            host: "10.0.0.7".to_string(),
            port: 1234,
            ..AriadnaConfig::default()
        };
        assert_eq!(cfg.base_url(), "http://10.0.0.7:1234");
    }

    #[test]
    fn chat_message_constructors_and_serialization() {
        assert_eq!(ChatMessage::system("s"), ChatMessage { role: "system".into(), content: "s".into() });
        assert_eq!(ChatMessage::user("u"), ChatMessage { role: "user".into(), content: "u".into() });
        assert_eq!(
            ChatMessage::assistant("a"),
            ChatMessage { role: "assistant".into(), content: "a".into() }
        );
        let value = serde_json::to_value(ChatMessage::user("привет")).expect("сериализация");
        assert_eq!(value, json!({"role": "user", "content": "привет"}));
    }

    #[test]
    fn is_available_true_for_real_installation() {
        // На машине разработки и бинарь, и GGUF присутствуют (дано задания).
        assert!(is_available(&AriadnaConfig::default()));
    }

    #[test]
    fn is_available_checks_both_paths() {
        let bin = make_temp_file("bin-ok", b"x");
        let gguf = make_temp_file("model-ok", b"y");
        let missing = PathBuf::from("/nonexistent/nothing");
        let cfg = |b: &Path, g: &Path| AriadnaConfig {
            server_bin: b.to_path_buf(),
            gguf_path: g.to_path_buf(),
            ..AriadnaConfig::default()
        };
        assert!(is_available(&cfg(&bin, &gguf)), "оба файла есть");
        assert!(!is_available(&cfg(&missing, &gguf)), "нет бинаря");
        assert!(!is_available(&cfg(&bin, &missing)), "нет модели");
        assert!(!is_available(&cfg(&missing, &missing)), "нет ничего");
        let _ = std::fs::remove_file(&bin);
        let _ = std::fs::remove_file(&gguf);
    }

    #[test]
    fn ensure_server_reuses_healthy_server() {
        let mock = MockLlama::start(ChatMode::Ok);
        let cfg = mock.config();
        let guard = ensure_server(&cfg).expect("живой сервер переиспользуется");
        assert!(!guard.owns_process(), "чужой процесс не должен принадлежать гарду");
        assert!(guard.log_path().is_none(), "у чужого сервера нет нашего лога");
        assert_eq!(guard.base_url(), format!("http://127.0.0.1:{}", mock.port));
    }

    #[test]
    fn guard_drop_keeps_foreign_server_alive() {
        let mock = MockLlama::start(ChatMode::Ok);
        let cfg = mock.config();
        let guard = ensure_server(&cfg).expect("переиспользование");
        let count_before_drop = mock.captured().len();
        drop(guard);
        assert_eq!(
            mock.captured().len(),
            count_before_drop,
            "Drop гарда не должен порождать запросов к серверу"
        );
        // И главное: «чужой» сервер после Drop продолжает отвечать.
        let probe = http_client(Duration::from_secs(2)).expect("http-клиент");
        assert!(health_ok(&probe, &cfg.base_url()), "чужой сервер не должен быть убит");
    }

    #[test]
    fn ensure_server_fails_fast_when_binary_missing() {
        let cfg = AriadnaConfig {
            server_bin: PathBuf::from("/nonexistent/llama-server"),
            gguf_path: PathBuf::from("/nonexistent/model.gguf"),
            host: "127.0.0.1".to_string(),
            port: free_port(),
            ctx: 512,
            gpu_layers: 0,
            startup_timeout: Duration::from_secs(10),
        };
        let started = Instant::now();
        let err = ensure_server(&cfg).expect_err("без бинаря — ошибка");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "ошибка должна быть быстрой, без ожидания таймаута"
        );
        let msg = format!("{err:#}");
        assert!(msg.contains("не найден"), "msg: {msg}");
        assert!(msg.contains("llama-server"), "msg: {msg}");
    }

    #[test]
    fn ensure_server_fails_fast_when_gguf_missing() {
        let bin = make_script("okbin", "#!/bin/sh\nexit 0\n");
        let cfg = AriadnaConfig {
            server_bin: bin.clone(),
            gguf_path: PathBuf::from("/nonexistent/model.gguf"),
            host: "127.0.0.1".to_string(),
            port: free_port(),
            ctx: 512,
            gpu_layers: 0,
            startup_timeout: Duration::from_secs(10),
        };
        let started = Instant::now();
        let err = ensure_server(&cfg).expect_err("без GGUF — ошибка");
        assert!(started.elapsed() < Duration::from_secs(10));
        let msg = format!("{err:#}");
        assert!(msg.contains("GGUF"), "msg: {msg}");
        let _ = std::fs::remove_file(&bin);
    }

    #[test]
    fn ensure_server_times_out_and_reports_log_tail() {
        // «Сервер», который живёт, но /health никогда не обслуживает.
        let bin = make_script("hang", "#!/bin/sh\necho fake-server-starting\nsleep 60\n");
        let gguf = make_temp_file("model-timeout.gguf", b"fake");
        let cfg = AriadnaConfig {
            server_bin: bin.clone(),
            gguf_path: gguf.clone(),
            host: "127.0.0.1".to_string(),
            port: free_port(),
            ctx: 512,
            gpu_layers: 0,
            startup_timeout: Duration::from_secs(2),
        };
        let started = Instant::now();
        let err = ensure_server(&cfg).expect_err("мёртвый порт — ошибка по таймауту");
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_secs(2), "вернулись раньше таймаута: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(15), "зависли сверх таймаута: {elapsed:?}");
        let msg = format!("{err:#}");
        assert!(msg.contains("не поднялся"), "msg: {msg}");
        assert!(msg.contains("fake-server-starting"), "хвост лога потерян: {msg}");
        // Лог-файл остаётся на диске для посмертной диагностики.
        let log = log_path_for(&cfg);
        assert!(log.is_file(), "лог-файл должен существовать");
        let _ = std::fs::remove_file(&bin);
        let _ = std::fs::remove_file(&gguf);
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn ensure_server_reports_early_exit_with_log_tail() {
        // «Сервер», который падает сразу (как llama-server с битым GGUF):
        // ошибка должна прийти раньше таймаута.
        let bin = make_script("earlyexit", "#!/bin/sh\necho 'fatal: cannot load model'\nexit 3\n");
        let gguf = make_temp_file("model-exit.gguf", b"fake");
        let cfg = AriadnaConfig {
            server_bin: bin.clone(),
            gguf_path: gguf.clone(),
            host: "127.0.0.1".to_string(),
            port: free_port(),
            ctx: 512,
            gpu_layers: 0,
            startup_timeout: Duration::from_secs(10),
        };
        let started = Instant::now();
        let err = ensure_server(&cfg).expect_err("досрочный выход — ошибка");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "не должны дожидаться полного таймаута"
        );
        let msg = format!("{err:#}");
        assert!(msg.contains("досрочно"), "msg: {msg}");
        assert!(msg.contains("fatal: cannot load model"), "хвост лога потерян: {msg}");
        let _ = std::fs::remove_file(&bin);
        let _ = std::fs::remove_file(&gguf);
        let _ = std::fs::remove_file(log_path_for(&cfg));
    }

    #[test]
    fn chat_parses_successful_response() {
        let mock = MockLlama::start(ChatMode::Ok);
        let out = chat(&mock.config(), &[ChatMessage::user("привет")]).expect("chat");
        assert_eq!(out, MOCK_REPLY);
    }

    #[test]
    fn chat_sends_openai_compatible_request() {
        let mock = MockLlama::start(ChatMode::Ok);
        let cfg = mock.config();
        let out = chat(&cfg, &[ChatMessage::system("SYS"), ChatMessage::user("TSK")])
            .expect("chat");
        assert_eq!(out, MOCK_REPLY);
        let requests = mock.chat_requests();
        assert_eq!(requests.len(), 1, "ровно один chat-запрос");
        let body: Value = serde_json::from_str(&requests[0].body).expect("тело — JSON");
        assert_eq!(body["model"], json!("ariadna"));
        assert_eq!(body["temperature"], json!(0.2));
        assert_eq!(body["max_tokens"], json!(3072));
        let messages = body["messages"].as_array().expect("messages — массив");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], json!({"role": "system", "content": "SYS"}));
        assert_eq!(messages[1], json!({"role": "user", "content": "TSK"}));
    }

    #[test]
    fn strip_think_blocks_full_cycle() {
        let s = "<think>долгие рассуждения</think>Ответ: 42.";
        assert_eq!(strip_think_blocks(s), "Ответ: 42.");
    }

    #[test]
    fn strip_think_blocks_unclosed_tail() {
        // бюджет токенов исчерпан прямо внутри thinking — хвост без </think>
        let s = "<think>рассуждения без конца";
        assert!(strip_think_blocks(s).contains("только thinking-блок"), "{}", strip_think_blocks(s));
    }

    #[test]
    fn strip_think_blocks_keeps_plain_text() {
        let s = "просто ответ без разметки";
        assert_eq!(strip_think_blocks(s), s);
        // два think-блока между текстом
        let s2 = "до <think>а</think> середина <think>б</think> после";
        assert_eq!(strip_think_blocks(s2), "до  середина  после");
    }

    #[test]
    fn chat_errors_on_http_error_status() {
        let mock = MockLlama::start(ChatMode::Http500);
        let err = chat(&mock.config(), &[ChatMessage::user("x")]).expect_err("500 — ошибка");
        let msg = format!("{err:#}");
        assert!(msg.contains("500"), "msg: {msg}");
        assert!(msg.contains("boom"), "тело ошибки должно попасть в текст: {msg}");
    }

    #[test]
    fn chat_errors_on_non_json_body() {
        let mock = MockLlama::start(ChatMode::InvalidJson);
        let err = chat(&mock.config(), &[ChatMessage::user("x")]).expect_err("не JSON — ошибка");
        let msg = format!("{err:#}");
        assert!(msg.contains("разбор JSON"), "msg: {msg}");
    }

    #[test]
    fn chat_errors_when_content_missing() {
        let mock = MockLlama::start(ChatMode::EmptyChoices);
        let err = chat(&mock.config(), &[ChatMessage::user("x")]).expect_err("пустые choices — ошибка");
        let msg = format!("{err:#}");
        assert!(msg.contains("choices[0].message.content"), "msg: {msg}");
    }

    #[test]
    fn run_task_sends_system_then_user_and_ensures_server() {
        let mock = MockLlama::start(ChatMode::Ok);
        let cfg = mock.config();
        let out = run_task(&cfg, "Ты — Ариадна.", "Найди выход из лабиринта").expect("run_task");
        assert_eq!(out, MOCK_REPLY);
        // run_task обязан сначала убедиться, что сервер жив (GET /health)…
        assert!(
            mock.captured().iter().any(|r| r.method == "GET" && r.path == "/health"),
            "run_task должен пройти через ensure_server"
        );
        // …и только потом слать chat с system+user.
        let requests = mock.chat_requests();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_str(&requests[0].body).expect("тело — JSON");
        let messages = body["messages"].as_array().expect("messages — массив");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], json!("system"));
        assert_eq!(messages[0]["content"], json!("Ты — Ариадна."));
        assert_eq!(messages[1]["role"], json!("user"));
        assert_eq!(messages[1]["content"], json!("Найди выход из лабиринта"));
    }

    /// Живой прогон против настоящего llama-server на GPU.
    /// Запуск: `cargo test live_llama_server_roundtrip -- --ignored`.
    #[test]
    #[ignore = "gpu"]
    fn live_llama_server_roundtrip() {
        let cfg = AriadnaConfig::default();
        if !is_available(&cfg) {
            eprintln!("пропуск: нет бинаря llama-server или GGUF");
            return;
        }
        let guard = ensure_server(&cfg).expect("поднять или переиспользовать сервер");
        let out = chat(&cfg, &[ChatMessage::user("Ответь одним словом: сколько будет 2+2?")])
            .expect("chat к живому серверу");
        assert!(!out.trim().is_empty(), "модель должна что-то ответить");
        drop(guard);
    }
}
