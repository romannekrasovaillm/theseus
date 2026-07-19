//! Расширения MCP-протокола по спецификации 2024-11-05: resources и prompts.
//!
//! `mcp.rs` трогать нельзя, поэтому здесь — собственный минимальный
//! stdio JSON-RPC клиент поверх stdin/stdout дочернего процесса:
//! initialize-handshake → notifications/initialized → resources/list,
//! resources/read, prompts/list, prompts/get. Пагинация (nextCursor)
//! обрабатывается автоматически, у каждого запроса — correlation id
//! (монотонный u64) и таймаут ожидания ответа.
//!
//! Ограничения: id только числовые (u64); строка stdout с битым JSON
//! считается фатальной ошибкой транспорта (как в `mcp.rs`).

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Таймаут по умолчанию на один JSON-RPC вызов.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Страховка от бесконечной пагинации (сервер, вечно отдающий nextCursor).
const MAX_PAGES: u32 = 100;

/// Версия протокола, которую объявляем в initialize.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Ресурс MCP-сервера (элемент ответа `resources/list`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct McpResource {
    /// URI ресурса (например, `file:///readme.txt`).
    pub uri: String,
    /// Человекочитаемое имя ресурса.
    pub name: String,
    /// MIME-тип содержимого, если сервер его сообщил (`mimeType` в JSON).
    #[serde(rename = "mimeType", default)]
    pub mime_type: Option<String>,
    /// Необязательное описание.
    #[serde(default)]
    pub description: Option<String>,
}

/// Аргумент промпта (элемент `arguments[]` в ответе `prompts/list`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct McpPromptArgument {
    /// Имя аргумента (ключ в `arguments` при `prompts/get`).
    pub name: String,
    /// Необязательное описание.
    #[serde(default)]
    pub description: Option<String>,
    /// Обязателен ли аргумент.
    #[serde(default)]
    pub required: bool,
}

/// Промпт MCP-сервера (элемент ответа `prompts/list`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct McpPrompt {
    /// Имя промпта.
    pub name: String,
    /// Необязательное описание.
    #[serde(default)]
    pub description: Option<String>,
    /// Объявленные аргументы промпта.
    #[serde(default)]
    pub arguments: Vec<McpPromptArgument>,
}

/// Содержимое ресурса (элемент `contents[]` в ответе `resources/read`).
/// По спецификации заполнено ровно одно из полей `text` / `blob`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct McpResourceContent {
    /// URI прочитанного ресурса.
    pub uri: String,
    /// MIME-тип содержимого, если сообщён (`mimeType` в JSON).
    #[serde(rename = "mimeType", default)]
    pub mime_type: Option<String>,
    /// Текстовое содержимое (UTF-8).
    #[serde(default)]
    pub text: Option<String>,
    /// Бинарное содержимое в base64.
    #[serde(default)]
    pub blob: Option<String>,
}

impl McpResourceContent {
    /// true, если пришёл текстовый вариант содержимого.
    pub fn is_text(&self) -> bool {
        self.text.is_some()
    }

    /// true, если пришёл бинарный (base64) вариант содержимого.
    pub fn is_blob(&self) -> bool {
        self.blob.is_some()
    }
}

/// Одно сообщение из результата `prompts/get`.
#[derive(Debug, Clone, PartialEq)]
pub struct McpPromptMessage {
    /// Роль автора сообщения (`user` / `assistant`).
    pub role: String,
    /// Текст сообщения; не-текстовый контент сериализуется в JSON.
    pub text: String,
}

/// Результат вызова `prompts/get`.
#[derive(Debug, Clone, PartialEq)]
pub struct McpPromptResult {
    /// Описание промпта, если сервер его вернул.
    pub description: Option<String>,
    /// Сообщения промпта в порядке, заданном сервером.
    pub messages: Vec<McpPromptMessage>,
}

// ---- приватные serde-обёртки для разбора ответов ----

/// Страница ответа `resources/list`.
#[derive(Debug, Deserialize)]
struct ResourcesPage {
    #[serde(default)]
    resources: Vec<McpResource>,
    #[serde(rename = "nextCursor", default)]
    next_cursor: Option<String>,
}

/// Страница ответа `prompts/list`.
#[derive(Debug, Deserialize)]
struct PromptsPage {
    #[serde(default)]
    prompts: Vec<McpPrompt>,
    #[serde(rename = "nextCursor", default)]
    next_cursor: Option<String>,
}

/// Ответ `resources/read`.
#[derive(Debug, Deserialize)]
struct ReadResult {
    #[serde(default)]
    contents: Vec<McpResourceContent>,
}

/// Сырое сообщение ответа `prompts/get` (content ещё не разобран).
#[derive(Debug, Deserialize)]
struct RawPromptMessage {
    role: String,
    content: serde_json::Value,
}

/// Ответ `prompts/get`.
#[derive(Debug, Deserialize)]
struct PromptGetResult {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    messages: Vec<RawPromptMessage>,
}

/// Извлечь текст из content-блока сообщения промпта.
/// Текстовый блок отдаёт свой `text`; прочие типы (image, resource)
/// возвращаются JSON-строкой, чтобы информация не терялась.
fn content_to_text(content: &serde_json::Value) -> String {
    if content["type"].as_str() == Some("text") {
        content["text"].as_str().unwrap_or_default().to_string()
    } else {
        serde_json::to_string(content).unwrap_or_default()
    }
}

/// Минимальный stdio JSON-RPC клиент для MCP-методов resources/prompts.
///
/// Порождённый процесс читается построчно (line-delimited JSON-RPC 2.0)
/// в отдельном потоке; ответы сопоставляются запросам по correlation id.
pub struct McpExtClient {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<String>,
    next_id: u64,
    timeout: Duration,
    server_info: serde_json::Value,
}

impl McpExtClient {
    /// Породить серверный процесс и выполнить MCP-handshake:
    /// `initialize` → `notifications/initialized`. При неудаче процесс убивается.
    pub fn spawn<S: AsRef<str>>(command: &str, args: &[S]) -> Result<Self> {
        let mut child = Command::new(command)
            .args(args.iter().map(S::as_ref))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("не запустить MCP-сервер: {command}"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("нет stdin у дочернего процесса"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("нет stdout у дочернего процесса"))?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let mut client = McpExtClient {
            child,
            stdin,
            rx,
            next_id: 0,
            timeout: DEFAULT_TIMEOUT,
            server_info: serde_json::Value::Null,
        };
        client.server_info = client.initialize()?;
        Ok(client)
    }

    /// Заменить таймаут ожидания ответа на JSON-RPC вызовы.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Сырой результат `initialize` (protocolVersion, capabilities, serverInfo).
    pub fn server_info(&self) -> &serde_json::Value {
        &self.server_info
    }

    /// Один JSON-RPC вызов с correlation id и дедлайном.
    /// Чужие id и нотификации пропускаются; RPC-ошибка превращается в Err.
    fn call_rpc(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{}", serde_json::to_string(&req)?)?;
        self.stdin.flush()?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(anyhow!("таймаут {method}"));
            }
            match self.rx.recv_timeout(left) {
                Ok(line) => {
                    let v: serde_json::Value = serde_json::from_str(&line)
                        .map_err(|e| anyhow!("{method}: битый JSON от сервера: {e}"))?;
                    if v["id"].as_u64() == Some(id) {
                        if let Some(err) = v.get("error").filter(|e| e.is_object()) {
                            let code = err["code"].as_i64().unwrap_or(0);
                            let msg = err["message"].as_str().unwrap_or("?");
                            return Err(anyhow!("{method}: RPC-ошибка {code}: {msg}"));
                        }
                        return Ok(v["result"].clone());
                    }
                    // нотификация или ответ с чужим id — пропускаем
                }
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(anyhow!("таймаут {method}")),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("{method}: сервер закрыл stdout"));
                }
            }
        }
    }

    /// Отправить нотификацию (ответа не ждём).
    fn notify(&mut self, method: &str) -> Result<()> {
        let req = json!({"jsonrpc": "2.0", "method": method});
        writeln!(self.stdin, "{}", serde_json::to_string(&req)?)?;
        self.stdin.flush()?;
        Ok(())
    }

    /// MCP-handshake: initialize + notifications/initialized.
    /// Возвращает сырой result initialize для инспекции capabilities.
    fn initialize(&mut self) -> Result<serde_json::Value> {
        let res = self.call_rpc(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "theseus-mcp-ext", "version": env!("CARGO_PKG_VERSION")}
            }),
        )?;
        // Несовпадение версии протокола не фатально: сервер вправе
        // ответить другой поддерживаемой версией — просто запомним её.
        self.notify("notifications/initialized")?;
        Ok(res)
    }

    /// Список всех ресурсов сервера (`resources/list` с обходом пагинации).
    pub fn list_resources(&mut self) -> Result<Vec<McpResource>> {
        let mut out = vec![];
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let res = self.call_rpc("resources/list", params)?;
            let page: ResourcesPage =
                serde_json::from_value(res).context("resources/list: разбор ответа")?;
            out.extend(page.resources);
            match page.next_cursor {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => break,
            }
        }
        Ok(out)
    }

    /// Прочитать ресурс по URI (`resources/read`).
    pub fn read_resource(&mut self, uri: &str) -> Result<Vec<McpResourceContent>> {
        let res = self.call_rpc("resources/read", json!({"uri": uri}))?;
        let parsed: ReadResult =
            serde_json::from_value(res).with_context(|| format!("resources/read {uri}: разбор ответа"))?;
        Ok(parsed.contents)
    }

    /// Список всех промптов сервера (`prompts/list` с обходом пагинации).
    pub fn list_prompts(&mut self) -> Result<Vec<McpPrompt>> {
        let mut out = vec![];
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let res = self.call_rpc("prompts/list", params)?;
            let page: PromptsPage =
                serde_json::from_value(res).context("prompts/list: разбор ответа")?;
            out.extend(page.prompts);
            match page.next_cursor {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => break,
            }
        }
        Ok(out)
    }

    /// Получить промпт с подстановкой аргументов (`prompts/get`).
    /// Аргументы — пары (имя, значение); значения по спецификации строковые.
    pub fn get_prompt(&mut self, name: &str, args: &[(&str, &str)]) -> Result<McpPromptResult> {
        let arguments: serde_json::Map<String, serde_json::Value> = args
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect();
        let res = self.call_rpc("prompts/get", json!({"name": name, "arguments": arguments}))?;
        let parsed: PromptGetResult =
            serde_json::from_value(res).with_context(|| format!("prompts/get {name}: разбор ответа"))?;
        let messages = parsed
            .messages
            .into_iter()
            .map(|m| McpPromptMessage { role: m.role, text: content_to_text(&m.content) })
            .collect();
        Ok(McpPromptResult { description: parsed.description, messages })
    }
}

impl Drop for McpExtClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait(); // не оставлять зомби-процесс
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Моковый MCP stdio-сервер (формат — как у test-workspace/mock_mcp.py:
    /// line-delimited JSON-RPC 2.0). Режимы: normal, silent (молчит после
    /// handshake — для теста таймаута), garbage (битый JSON), stray (чужой id
    /// и нотификация перед ответом), paged (пагинация resources/list).
    const MOCK_PY: &str = r#"#!/usr/bin/env python3
"""Тестовый MCP stdio-сервер: resources/prompts (JSON-RPC 2.0, line-delimited)."""
import json
import sys

MODE = sys.argv[1] if len(sys.argv) > 1 else "normal"

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

RESOURCES = [
    {"uri": "file:///readme.txt", "name": "readme", "mimeType": "text/plain",
     "description": "Текстовый файл"},
    {"uri": "file:///logo.png", "name": "logo", "mimeType": "image/png"},
]

PROMPTS = [
    {"name": "greet", "description": "Приветствие",
     "arguments": [{"name": "name", "description": "Кого приветствуем",
                    "required": True}]},
    {"name": "noop", "arguments": []},
]

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except Exception:
        continue
    method = req.get("method")
    rid = req.get("id")
    params = req.get("params") or {}
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": rid,
              "result": {"protocolVersion": "2024-11-05",
                         "capabilities": {"resources": {}, "prompts": {}},
                         "serverInfo": {"name": "mock-ext", "version": "0.2"}}})
        continue
    if method == "notifications/initialized":
        continue
    if MODE == "silent":
        continue  # после handshake молчим — тест таймаута
    if MODE == "garbage":
        sys.stdout.write("{это не json\n")
        sys.stdout.flush()
        continue
    if MODE == "stray":
        # мусор перед настоящим ответом: чужой id и нотификация
        send({"jsonrpc": "2.0", "id": 999, "result": {"resources": []}})
        send({"jsonrpc": "2.0", "method": "notifications/progress",
              "params": {"progress": 1}})
    if method == "resources/list":
        cursor = params.get("cursor")
        if MODE == "paged" and cursor is None:
            send({"jsonrpc": "2.0", "id": rid,
                  "result": {"resources": [RESOURCES[0]], "nextCursor": "p2"}})
        elif MODE == "paged":
            send({"jsonrpc": "2.0", "id": rid,
                  "result": {"resources": [RESOURCES[1]]}})
        else:
            send({"jsonrpc": "2.0", "id": rid, "result": {"resources": RESOURCES}})
    elif method == "resources/read":
        uri = params.get("uri", "")
        if uri == "file:///readme.txt":
            send({"jsonrpc": "2.0", "id": rid,
                  "result": {"contents": [{"uri": uri, "mimeType": "text/plain",
                                           "text": "Привет, MCP!"}]}})
        elif uri == "file:///logo.png":
            send({"jsonrpc": "2.0", "id": rid,
                  "result": {"contents": [{"uri": uri, "mimeType": "image/png",
                                           "blob": "aGVsbG8="}]}})
        else:
            send({"jsonrpc": "2.0", "id": rid,
                  "error": {"code": -32602, "message": "unknown resource uri"}})
    elif method == "prompts/list":
        send({"jsonrpc": "2.0", "id": rid, "result": {"prompts": PROMPTS}})
    elif method == "prompts/get":
        name = params.get("name", "")
        args = params.get("arguments", {})
        if name == "greet":
            who = args.get("name", "мир")
            send({"jsonrpc": "2.0", "id": rid,
                  "result": {"description": "Приветствие",
                             "messages": [{"role": "user",
                                           "content": {"type": "text",
                                                       "text": "Привет, " + who + "!"}}]}})
        else:
            send({"jsonrpc": "2.0", "id": rid,
                  "error": {"code": -32601, "message": "no such prompt"}})
    else:
        send({"jsonrpc": "2.0", "id": rid,
              "error": {"code": -32601, "message": "unknown method"}})
"#;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Записать мок в уникальный временный файл (тесты бегут параллельно).
    fn mock_path(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "theseus_mcp_ext_{}_{}_{}.py",
            std::process::id(),
            tag,
            n
        ));
        std::fs::write(&path, MOCK_PY).unwrap();
        path
    }

    /// Поднять мок-сервер в заданном режиме; файл мока удаляется сразу
    /// после запуска (python уже прочитал скрипт).
    fn spawn_mock(mode: &str) -> McpExtClient {
        let path = mock_path(mode);
        let script = path.to_str().unwrap().to_string();
        let client = McpExtClient::spawn("python3", &[script, mode.to_string()]).unwrap();
        let _ = std::fs::remove_file(&path);
        client
    }

    #[test]
    fn handshake_returns_server_info() {
        let client = spawn_mock("normal");
        let info = client.server_info();
        assert_eq!(info["protocolVersion"].as_str().unwrap(), "2024-11-05");
        assert_eq!(info["serverInfo"]["name"].as_str().unwrap(), "mock-ext");
        assert_eq!(info["serverInfo"]["version"].as_str().unwrap(), "0.2");
        assert!(info["capabilities"]["resources"].is_object());
        assert!(info["capabilities"]["prompts"].is_object());
    }

    #[test]
    fn handshake_bad_command_fails() {
        let no_args: &[&str] = &[];
        match McpExtClient::spawn("theseus-no-such-binary-xyz", no_args) {
            Ok(_) => panic!("ожидали ошибку запуска несуществующего бинарника"),
            Err(e) => assert!(e.to_string().contains("не запустить MCP-сервер"), "err: {e}"),
        }
    }

    #[test]
    fn list_resources_parses_fields() {
        let mut client = spawn_mock("normal");
        let resources = client.list_resources().unwrap();
        assert_eq!(resources.len(), 2);
        assert_eq!(resources[0].uri, "file:///readme.txt");
        assert_eq!(resources[0].name, "readme");
        assert_eq!(resources[0].mime_type.as_deref(), Some("text/plain"));
        assert_eq!(resources[0].description.as_deref(), Some("Текстовый файл"));
        assert_eq!(resources[1].uri, "file:///logo.png");
        assert_eq!(resources[1].mime_type.as_deref(), Some("image/png"));
        assert_eq!(resources[1].description, None);
    }

    #[test]
    fn read_text_resource() {
        let mut client = spawn_mock("normal");
        let contents = client.read_resource("file:///readme.txt").unwrap();
        assert_eq!(contents.len(), 1);
        let c = &contents[0];
        assert_eq!(c.uri, "file:///readme.txt");
        assert!(c.is_text());
        assert!(!c.is_blob());
        assert_eq!(c.text.as_deref(), Some("Привет, MCP!"));
        assert_eq!(c.mime_type.as_deref(), Some("text/plain"));
    }

    #[test]
    fn read_blob_resource() {
        let mut client = spawn_mock("normal");
        let contents = client.read_resource("file:///logo.png").unwrap();
        assert_eq!(contents.len(), 1);
        let c = &contents[0];
        assert!(c.is_blob());
        assert!(!c.is_text());
        assert_eq!(c.blob.as_deref(), Some("aGVsbG8=")); // base64("hello")
        assert_eq!(c.mime_type.as_deref(), Some("image/png"));
    }

    #[test]
    fn list_prompts_with_arguments() {
        let mut client = spawn_mock("normal");
        let prompts = client.list_prompts().unwrap();
        assert_eq!(prompts.len(), 2);
        let greet = &prompts[0];
        assert_eq!(greet.name, "greet");
        assert_eq!(greet.description.as_deref(), Some("Приветствие"));
        assert_eq!(greet.arguments.len(), 1);
        assert_eq!(greet.arguments[0].name, "name");
        assert!(greet.arguments[0].required);
        assert_eq!(greet.arguments[0].description.as_deref(), Some("Кого приветствуем"));
        let noop = &prompts[1];
        assert_eq!(noop.name, "noop");
        assert!(noop.arguments.is_empty());
        assert_eq!(noop.description, None);
    }

    #[test]
    fn get_prompt_substitutes_arguments() {
        let mut client = spawn_mock("normal");
        let result = client.get_prompt("greet", &[("name", "Роман")]).unwrap();
        assert_eq!(result.description.as_deref(), Some("Приветствие"));
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].role, "user");
        assert_eq!(result.messages[0].text, "Привет, Роман!");
    }

    #[test]
    fn method_error_unknown_resource() {
        let mut client = spawn_mock("normal");
        let err = client.read_resource("file:///missing.txt").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("-32602"), "err: {msg}");
        assert!(msg.contains("unknown resource uri"), "err: {msg}");
    }

    #[test]
    fn timeout_on_silent_server() {
        let mut client = spawn_mock("silent");
        client.set_timeout(Duration::from_millis(400));
        let started = Instant::now();
        let err = client.list_resources().unwrap_err();
        assert!(err.to_string().contains("таймаут"), "err: {err}");
        assert!(started.elapsed() < Duration::from_secs(5), "слишком долго ждали");
    }

    #[test]
    fn broken_json_is_error() {
        let mut client = spawn_mock("garbage");
        let err = client.list_resources().unwrap_err();
        assert!(err.to_string().contains("битый JSON"), "err: {err}");
    }

    #[test]
    fn correlation_skips_stray_id_and_notifications() {
        let mut client = spawn_mock("stray");
        let resources = client.list_resources().unwrap();
        assert_eq!(resources.len(), 2);
        assert_eq!(resources[0].name, "readme");
    }

    #[test]
    fn pagination_collects_two_pages() {
        let mut client = spawn_mock("paged");
        let resources = client.list_resources().unwrap();
        assert_eq!(resources.len(), 2);
        assert_eq!(resources[0].uri, "file:///readme.txt");
        assert_eq!(resources[1].uri, "file:///logo.png");
    }

    #[test]
    fn unknown_prompt_method_error() {
        let mut client = spawn_mock("normal");
        let err = client.get_prompt("missing", &[]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("-32601"), "err: {msg}");
        assert!(msg.contains("no such prompt"), "err: {msg}");
    }

    #[test]
    fn content_to_text_non_text_falls_back_to_json() {
        let image = json!({"type": "image", "data": "AAAA", "mimeType": "image/png"});
        let text = content_to_text(&image);
        assert!(text.contains("image"), "text: {text}");
        assert!(text.contains("AAAA"), "text: {text}");
        let plain = json!({"type": "text", "text": "просто текст"});
        assert_eq!(content_to_text(&plain), "просто текст");
    }
}
