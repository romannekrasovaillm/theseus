//! Минимальный MCP stdio-клиент (урок обзора: MCP есть у всех трёх харнессов).
//! JSON-RPC 2.0 поверх stdin/stdout: initialize → tools/list → tools/call.

use anyhow::{anyhow, Result};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

pub struct McpServer {
    name: String,
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<String>,
    next_id: u64,
}

impl McpServer {
    pub fn spawn(name: &str, command: &str, args: &[String]) -> Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("нет stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("нет stdout"))?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() { break; }
                    }
                    Err(_) => break,
                }
            }
        });
        let mut s = McpServer { name: name.into(), child, stdin, rx, next_id: 0 };
        s.initialize()?;
        Ok(s)
    }

    fn call_rpc(&mut self, method: &str, params: serde_json::Value, timeout: Duration) -> Result<serde_json::Value> {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{}", serde_json::to_string(&req)?)?;
        self.stdin.flush()?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() { return Err(anyhow!("таймаут {method}")); }
            match self.rx.recv_timeout(left) {
                Ok(line) => {
                    let v: serde_json::Value = serde_json::from_str(&line)
                        .map_err(|e| anyhow!("невалидный JSON-RPC: {e}: {line}"))?;
                    if v["id"].as_u64() == Some(id) {
                        if let Some(err) = v.get("error") {
                            return Err(anyhow!("{method}: {err}"));
                        }
                        return Ok(v["result"].clone());
                    }
                    // нотификация или чужой id — пропускаем
                }
                Err(_) => return Err(anyhow!("таймаут {method}")),
            }
        }
    }

    fn notify(&mut self, method: &str) -> Result<()> {
        let req = json!({"jsonrpc": "2.0", "method": method});
        writeln!(self.stdin, "{}", serde_json::to_string(&req)?)?;
        Ok(self.stdin.flush()?)
    }

    fn initialize(&mut self) -> Result<()> {
        self.call_rpc("initialize", json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "theseus", "version": "0.2.0"}
        }), Duration::from_secs(10))?;
        self.notify("notifications/initialized")?;
        Ok(())
    }

    pub fn list_tools(&mut self) -> Result<Vec<(String, serde_json::Value)>> {
        let res = self.call_rpc("tools/list", json!({}), Duration::from_secs(10))?;
        let mut out = vec![];
        if let Some(tools) = res["tools"].as_array() {
            for t in tools {
                out.push((t["name"].as_str().unwrap_or("?").to_string(), t.clone()));
            }
        }
        Ok(out)
    }

    pub fn call_tool(&mut self, tool: &str, args: serde_json::Value) -> Result<String> {
        let res = self.call_rpc("tools/call", json!({"name": tool, "arguments": args}),
                                Duration::from_secs(60))?;
        let mut texts = vec![];
        if let Some(content) = res["content"].as_array() {
            for c in content {
                if c["type"].as_str() == Some("text") {
                    if let Some(t) = c["text"].as_str() { texts.push(t.to_string()); }
                }
            }
        }
        if texts.is_empty() {
            return Ok(serde_json::to_string_pretty(&res)?);
        }
        Ok(texts.join("\n"))
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

// ---------------- HTTP-транспорт (StreamableHTTP, v0.3.1) ----------------

pub struct McpHttp {
    name: String,
    url: String,
    auth: Option<String>,
    elicit: String,
    http: reqwest::blocking::Client,
    next_id: u64,
}

impl McpHttp {
    pub fn connect(name: &str, url: &str, auth: Option<String>, elicit: Option<String>) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(70))
            .user_agent("theseus/0.3.1")
            .build()?;
        let mut s = McpHttp {
            name: name.into(),
            url: url.into(),
            auth,
            elicit: elicit.unwrap_or_else(|| "decline".into()),
            http,
            next_id: 0,
        };
        s.initialize()?;
        Ok(s)
    }

    fn post(&self, body: &serde_json::Value) -> Result<String> {
        let mut rb = self.http.post(&self.url).json(body);
        if let Some(k) = &self.auth {
            rb = rb.header("Authorization", format!("Bearer {k}"));
        }
        Ok(rb.send()?.text()?)
    }

    fn answer_request(&self, id: u64, method: &str) -> Result<()> {
        let result = if method == "elicitation/create" && self.elicit == "accept" {
            json!({"action": "accept", "content": {"text": "auto-accept per config"}})
        } else if method == "elicitation/create" {
            json!({"action": "decline"})
        } else {
            return Ok(()); // неизвестные server-запросы молча игнорируем
        };
        let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
        let _ = self.post(&resp)?;
        Ok(())
    }

    fn call_rpc(&mut self, method: &str, params: serde_json::Value, _t: Duration) -> Result<serde_json::Value> {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let text = self.post(&req)?;
        // ответ либо JSON, либо SSE-поток (StreamableHTTP)
        let candidates: Vec<String> = if text.trim_start().starts_with('{') {
            vec![text]
        } else {
            text.lines()
                .filter_map(|l| l.strip_prefix("data:").map(|d| d.trim().to_string()))
                .collect()
        };
        for cand in candidates {
            let v: serde_json::Value = match serde_json::from_str(&cand) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v["id"].as_u64() == Some(id) {
                if let Some(err) = v.get("error") {
                    return Err(anyhow!("{method}: {err}"));
                }
                return Ok(v["result"].clone());
            }
            // server-initiated запрос (elicitation и др.)
            if let (Some(rid), Some(m)) = (v["id"].as_u64(), v["method"].as_str()) {
                self.answer_request(rid, m)?;
            }
        }
        Err(anyhow!("нет ответа на {method} (id {id})"))
    }

    fn initialize(&mut self) -> Result<()> {
        self.call_rpc("initialize", json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "theseus", "version": "0.3.1"}
        }), Duration::from_secs(15))?;
        // notifications/initialized — без ожидания ответа
        let _ = self.post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        Ok(())
    }

    pub fn list_tools(&mut self) -> Result<Vec<(String, serde_json::Value)>> {
        let res = self.call_rpc("tools/list", json!({}), Duration::from_secs(15))?;
        let mut out = vec![];
        if let Some(tools) = res["tools"].as_array() {
            for t in tools {
                out.push((t["name"].as_str().unwrap_or("?").to_string(), t.clone()));
            }
        }
        Ok(out)
    }

    pub fn call_tool(&mut self, tool: &str, args: serde_json::Value) -> Result<String> {
        let res = self.call_rpc("tools/call", json!({"name": tool, "arguments": args}),
                                Duration::from_secs(60))?;
        let mut texts = vec![];
        if let Some(content) = res["content"].as_array() {
            for c in content {
                if c["type"].as_str() == Some("text") {
                    if let Some(t) = c["text"].as_str() { texts.push(t.to_string()); }
                }
            }
        }
        if texts.is_empty() {
            return Ok(serde_json::to_string_pretty(&res)?);
        }
        Ok(texts.join("\n"))
    }
}

/// (namespaced имя, сервер, инструмент, схема)
pub type McpToolEntry = (String, String, String, serde_json::Value);

enum ServerKind {
    Stdio(McpServer),
    Http(McpHttp),
}

pub struct McpRegistry {
    servers: Vec<ServerKind>,
    pub tools: Vec<McpToolEntry>,
}

impl McpRegistry {
    pub fn connect_all(cfgs: &[crate::config::McpServerConfig], log: &mut dyn FnMut(&str)) -> Self {
        let mut reg = McpRegistry { servers: vec![], tools: vec![] };
        for c in cfgs {
            // v0.3.1: HTTP-транспорт при наличии url, иначе stdio
            if let Some(url) = &c.url {
                let auth = c.env_key.as_ref().and_then(|k| std::env::var(k).ok());
                match McpHttp::connect(&c.name, url, auth, c.elicit.clone()) {
                    Ok(mut srv) => match srv.list_tools() {
                        Ok(tools) => {
                            log(&format!("MCP {} (http): подключён, инструментов: {}", c.name, tools.len()));
                            for (tname, schema) in tools {
                                reg.tools.push((format!("mcp__{}__{}", c.name, tname), c.name.clone(), tname, schema));
                            }
                            reg.servers.push(ServerKind::Http(srv));
                        }
                        Err(e) => log(&format!("MCP {}: tools/list ошибка: {e}", c.name)),
                    },
                    Err(e) => log(&format!("MCP {} (http): не поднялся: {e}", c.name)),
                }
                continue;
            }
            if c.command.is_empty() {
                log(&format!("MCP {}: ни command, ни url не заданы", c.name));
                continue;
            }
            match McpServer::spawn(&c.name, &c.command, &c.args) {
                Ok(mut srv) => match srv.list_tools() {
                    Ok(tools) => {
                        log(&format!("MCP {}: подключён, инструментов: {}", c.name, tools.len()));
                        for (tname, schema) in tools {
                            reg.tools.push((format!("mcp__{}__{}", c.name, tname), c.name.clone(), tname, schema));
                        }
                        reg.servers.push(ServerKind::Stdio(srv));
                    }
                    Err(e) => log(&format!("MCP {}: tools/list ошибка: {e}", c.name)),
                },
                Err(e) => log(&format!("MCP {}: не поднялся: {e}", c.name)),
            }
        }
        reg
    }

    pub fn is_empty(&self) -> bool { self.tools.is_empty() }

    pub fn call(&mut self, server: &str, tool: &str, args: serde_json::Value) -> Result<String> {
        for s in &mut self.servers {
            match s {
                ServerKind::Stdio(srv) if srv.name == server => return srv.call_tool(tool, args),
                ServerKind::Http(srv) if srv.name == server => return srv.call_tool(tool, args),
                _ => {}
            }
        }
        Err(anyhow!("MCP-сервер {server} не найден"))
    }
}
