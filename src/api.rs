//! Клиент OpenAI-совместимого chat/completions API (DeepSeek V4).
//! Уроки обзора: ретраи с backoff, уважение Retry-After, скрытый первый ретрай.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Цепочка рассуждений thinking-модели: храним и возвращаем с историей
    /// (interleaved thinking). Kimi K3 требует полное assistant-сообщение
    /// без изменений (иначе 400 на multi-turn/tools); GLM preserved thinking —
    /// неизменный reasoning_content; DeepSeek игнорирует, но принимает
    /// безопасно (доки deepseek-api: «в историю добавляй как обычно»).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(s: impl Into<String>) -> Self {
        Message { role: "system".into(), content: Some(s.into()), reasoning_content: None, tool_calls: None, tool_call_id: None }
    }
    pub fn user(s: impl Into<String>) -> Self {
        Message { role: "user".into(), content: Some(s.into()), reasoning_content: None, tool_calls: None, tool_call_id: None }
    }
    pub fn assistant(content: Option<String>, tool_calls: Option<Vec<ToolCall>>) -> Self {
        Message { role: "assistant".into(), content, reasoning_content: None, tool_calls, tool_call_id: None }
    }
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Message { role: "tool".into(), content: Some(content.into()), reasoning_content: None, tool_calls: None, tool_call_id: Some(call_id.into()) }
    }
    /// Прикрепить цепочку рассуждений (пустая строка отбрасывается).
    pub fn with_reasoning(mut self, reasoning: Option<String>) -> Self {
        self.reasoning_content = reasoning.filter(|s| !s.is_empty());
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// полный текст цепочки рассуждений (для проброса в историю — см.
    /// Message::reasoning_content); None, если провайдер его не вернул
    pub reasoning: Option<String>,
    pub reasoning_len: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub finish_reason: Option<String>,
    pub latency: Duration,
    /// стрим прерван досрочно (преемпция пользователем, урок Codex mailbox)
    pub aborted: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Accounting {
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_latency: Duration,
}

pub struct ApiClient {
    base_url: String,
    api_key: String,
    model: String,
    extra_body: serde_json::Value,
    max_output_tokens: usize,
    http: reqwest::blocking::Client,
    pub accounting: Accounting,
}

impl ApiClient {
    pub fn new(
        base_url: &str, api_key: &str, model: &str,
        timeout_secs: u64, extra_body: serde_json::Value, max_output_tokens: usize,
    ) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(timeout_secs + 30))
            .user_agent("theseus/0.1")
            .build()?;
        // Kimi Code не принимает thinking без reasoning_content в истории
        // (HTTP 400, доки 08.08) — для провайдера kimi ключи мышления
        // выкидываем из extra_body на входе (единая точка: Agent::new,
        // /model, субагенты). reasoning_effort — за компанию: K3 ризонит
        // всегда, уровень у него не настраивается.
        let mut extra_body = extra_body;
        if crate::models::find_model(model).map(|m| m.provider == "kimi") == Some(true) {
            if let Some(obj) = extra_body.as_object_mut() {
                obj.remove("thinking");
                obj.remove("reasoning_effort");
            }
        }
        Ok(ApiClient {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            extra_body,
            max_output_tokens,
            http,
            accounting: Accounting::default(),
        })
    }

    pub fn chat(&mut self, messages: &[Message], tools: &serde_json::Value) -> Result<ChatResponse> {
        self.chat_inner(messages, tools, false, &mut |_| {}, &|| false)
    }

    /// Стриминг-вариант (SSE): текстовые дельты уходят в on_text по мере поступления;
    /// should_stop()==true → досрочный разрыв стрима (преемпция, ChatResponse.aborted=true)
    pub fn chat_stream(&mut self, messages: &[Message], tools: &serde_json::Value,
                       on_text: &mut dyn FnMut(&str),
                       should_stop: &dyn Fn() -> bool) -> Result<ChatResponse> {
        self.chat_inner(messages, tools, true, on_text, should_stop)
    }

    fn chat_inner(&mut self, messages: &[Message], tools: &serde_json::Value,
                  stream: bool, on_text: &mut dyn FnMut(&str),
                  should_stop: &dyn Fn() -> bool) -> Result<ChatResponse> {
        // Принудительная температура провайдера из реестра (Kimi K3 — ровно 1,
        // иначе HTTP 400); для остальных моделей — 0 (детерминизм ML-задач).
        let temperature = crate::models::find_model(&self.model)
            .and_then(|m| m.temperature)
            .unwrap_or(0.0);
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": self.max_output_tokens,
            "temperature": temperature,
        });
        if !tools.is_null() {
            body["tools"] = tools.clone();
            body["tool_choice"] = serde_json::json!("auto");
        }
        if stream {
            body["stream"] = serde_json::json!(true);
        }
        // extra_body (напр. thinking) — поверх
        if let serde_json::Value::Object(m) = &self.extra_body {
            for (k, v) in m { body[k] = v.clone(); }
        }

        let url = format!("{}/chat/completions", self.base_url);
        let mut delay = 2u64;
        let mut last_err = anyhow!("—");
        for attempt in 0..5 {
            let t0 = Instant::now();
            let resp = self.http.post(&url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .json(&body)
                .send();
            match resp {
                Ok(r) => {
                    let status = r.status();
                    // уважение Retry-After (читаем ДО поглощения тела)
                    let retry_after = r.headers().get("retry-after")
                        .and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
                    if status.is_success() {
                        if stream {
                            return self.parse_stream(r, t0.elapsed(), on_text, should_stop);
                        }
                        let text = r.text().unwrap_or_default();
                        return self.parse_response(&text, t0.elapsed());
                    }
                    let text = r.text().unwrap_or_default();
                    // Безопасный срез по границам символов (не байт):
                    // text.len().min(400) может разрезать многобайтовый UTF-8 символ.
                    let preview: String = text.chars().take(400).collect();
                    last_err = anyhow!("HTTP {}: {}", status.as_u16(), preview);
                    // ретрай только на 429/5xx
                    if !(status.as_u16() == 429 || status.is_server_error()) {
                        return Err(last_err.context("API ответил ошибкой без ретрая"));
                    }
                    if let Some(ra) = retry_after {
                        delay = ra.min(120);
                    }
                }
                Err(e) => {
                    last_err = anyhow!("transport: {e}");
                }
            }
            if attempt < 4 {
                if attempt > 0 && std::env::var_os("THESEUS_DEBUG").is_some() {
                    // см. комментарий в tools::run_bash — сырой stderr ломает TUI
                    eprintln!("[api retry {}/4 через {}s] {}", attempt + 1, delay, last_err);
                }
                std::thread::sleep(Duration::from_secs(delay));
                delay = (delay * 2).min(60);
            }
        }
        Err(last_err.context("API недоступен после ретраев"))
    }

    /// Текущий потолок max_tokens и одноразовая эскалация (урок Claude max_output_tokens)
    pub fn set_max_output(&mut self, v: usize) {
        self.max_output_tokens = v;
    }
    pub fn max_output(&self) -> usize {
        self.max_output_tokens
    }

    fn parse_response(&mut self, text: &str, latency: Duration) -> Result<ChatResponse> {
        let v: serde_json::Value = serde_json::from_str(text)
            .with_context(|| {
                let preview: String = text.chars().take(200).collect();
                format!("невалидный JSON ответа: {preview}")
            })?;
        let choice = &v["choices"][0];
        let msg = &choice["message"];
        let tool_calls: Vec<ToolCall> = serde_json::from_value(msg["tool_calls"].clone())
            .unwrap_or_default();
        let usage = &v["usage"];
        let prompt_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0);
        let completion_tokens = usage["completion_tokens"].as_u64().unwrap_or(0);
        let reasoning = msg["reasoning_content"].as_str().map(String::from)
            .filter(|s| !s.is_empty());
        self.accounting.calls += 1;
        self.accounting.prompt_tokens += prompt_tokens;
        self.accounting.completion_tokens += completion_tokens;
        self.accounting.total_latency += latency;
        Ok(ChatResponse {
            content: msg["content"].as_str().map(String::from)
                .filter(|s| !s.is_empty()),
            tool_calls,
            reasoning_len: reasoning.as_deref().map(str::len).unwrap_or(0),
            reasoning,
            prompt_tokens,
            completion_tokens,
            finish_reason: choice["finish_reason"].as_str().map(String::from),
            aborted: false,
            latency,
        })
    }

    /// Разбор SSE-потока: data: {...}\n\n ... data: [DONE]; should_stop → досрочный разрыв
    fn parse_stream(&mut self, r: reqwest::blocking::Response, latency0: Duration,
                    on_text: &mut dyn FnMut(&str), should_stop: &dyn Fn() -> bool) -> Result<ChatResponse> {
        use std::io::{BufRead, BufReader};
        let reader = BufReader::new(r);
        let t0 = Instant::now();
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut finish_reason: Option<String> = None;
        let mut prompt_tokens = 0u64;
        let mut completion_tokens = 0u64;
        let mut aborted = false;
        // накопление tool calls по index
        let mut tc_acc: std::collections::BTreeMap<u64, (String, String, String)> = std::collections::BTreeMap::new();

        for line in reader.lines() {
            if should_stop() {
                aborted = true;
                break;
            }
            let line = match line { Ok(l) => l, Err(_) => break };
            let data = match line.strip_prefix("data:") {
                Some(d) => d.trim(),
                None => continue,
            };
            if data == "[DONE]" { break; }
            let v: serde_json::Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(u) = v.get("usage") {
                prompt_tokens = u["prompt_tokens"].as_u64().unwrap_or(prompt_tokens);
                completion_tokens = u["completion_tokens"].as_u64().unwrap_or(completion_tokens);
            }
            let choice = &v["choices"][0];
            if let Some(fr) = choice["finish_reason"].as_str() {
                finish_reason = Some(fr.to_string());
            }
            let delta = &choice["delta"];
            if let Some(c) = delta["content"].as_str() {
                content.push_str(c);
                on_text(c);
            }
            if let Some(rc) = delta["reasoning_content"].as_str() {
                reasoning.push_str(rc);
            }
            if let Some(tcs) = delta["tool_calls"].as_array() {
                for tc in tcs {
                    let idx = tc["index"].as_u64().unwrap_or(0);
                    let e = tc_acc.entry(idx).or_default();
                    if let Some(id) = tc["id"].as_str() { e.0 = id.to_string(); }
                    if let Some(name) = tc["function"]["name"].as_str() { e.1 = name.to_string(); }
                    if let Some(args) = tc["function"]["arguments"].as_str() { e.2.push_str(args); }
                }
            }
        }
        let latency = if latency0 > Duration::ZERO { latency0 } else { t0.elapsed() };
        let tool_calls: Vec<ToolCall> = tc_acc.into_iter().map(|(_, (id, name, args))| ToolCall {
            id,
            kind: "function".into(),
            function: ToolFunction { name, arguments: args },
        }).collect();
        self.accounting.calls += 1;
        self.accounting.prompt_tokens += prompt_tokens;
        self.accounting.completion_tokens += completion_tokens;
        self.accounting.total_latency += latency;
        Ok(ChatResponse {
            content: if content.is_empty() { None } else { Some(content) },
            tool_calls,
            reasoning_len: reasoning.len(),
            reasoning: if reasoning.is_empty() { None } else { Some(reasoning) },
            prompt_tokens,
            completion_tokens,
            finish_reason,
            latency,
            aborted,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sse_delta_accumulation_smoke() {
        // логика накопления tool-call дельт по index
        let mut acc: std::collections::BTreeMap<u64, (String, String, String)> = std::collections::BTreeMap::new();
        let chunks = [
            (0u64, Some("call_1"), Some("bash"), Some("{\"command\":")),
            (0, None, None, Some(" \"ls\"}")),
        ];
        for (idx, id, name, args) in chunks {
            let e = acc.entry(idx).or_default();
            if let Some(i) = id { e.0 = i.to_string(); }
            if let Some(n) = name { e.1 = n.to_string(); }
            if let Some(a) = args { e.2.push_str(a); }
        }
        let (_, (id, name, args)) = acc.into_iter().next().unwrap();
        assert_eq!(id, "call_1");
        assert_eq!(name, "bash");
        assert_eq!(args, "{\"command\": \"ls\"}");
    }

    /// Нестриминговый разбор ловит reasoning_content целиком (Kimi K3/GLM/DeepSeek);
    /// пустое поле отбрасывается, чтобы не раздувать историю пустышками.
    #[test]
    fn parse_response_captures_reasoning() {
        let mut client = super::ApiClient::new(
            "http://127.0.0.1:9/v1", "k", "m", 1, serde_json::json!({}), 16).unwrap();
        let body = serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "готово",
                            "reasoning_content": "цепочка"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2},
        });
        let resp = client.parse_response(&body.to_string(), std::time::Duration::ZERO).unwrap();
        assert_eq!(resp.reasoning.as_deref(), Some("цепочка"));
        assert_eq!(resp.reasoning_len, "цепочка".len());
        assert_eq!(resp.content.as_deref(), Some("готово"));
        // без reasoning_content — None, а не пустая строка
        let body = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "текст"},
                         "finish_reason": "stop"}],
            "usage": {},
        });
        let resp = client.parse_response(&body.to_string(), std::time::Duration::ZERO).unwrap();
        assert_eq!(resp.reasoning, None);
        assert_eq!(resp.reasoning_len, 0);
    }

    /// Message::with_reasoning: пустая цепочка отбрасывается; serde-раундтрип
    /// старых сессий (без поля reasoning_content) не ломается.
    #[test]
    fn message_reasoning_serde_roundtrip() {
        use super::Message;
        let msg = Message::assistant(Some("a".into()), None).with_reasoning(Some("r".into()));
        let wire = serde_json::to_string(&msg).unwrap();
        assert!(wire.contains("\"reasoning_content\":\"r\""), "wire: {wire}");
        let back: Message = serde_json::from_str(&wire).unwrap();
        assert_eq!(back.reasoning_content.as_deref(), Some("r"));
        // пустое reasoning отбрасывается
        let msg = Message::assistant(Some("a".into()), None).with_reasoning(Some(String::new()));
        assert!(msg.reasoning_content.is_none());
        // старый формат сессии (без поля) десериализуется
        let old: Message = serde_json::from_str(r#"{"role":"assistant","content":"x"}"#).unwrap();
        assert!(old.reasoning_content.is_none());
    }

    /// Kimi-провайдер: thinking выкидывается из extra_body на входе (Kimi Code
    /// отвечает 400 на thinking без reasoning_content в истории; для K3 thinking
    /// всегда включён на стороне сервера, параметр из K2.x не передаём).
    /// reasoning_effort срезается за компанию. DeepSeek-модели не трогаем.
    #[test]
    fn kimi_provider_strips_thinking_from_extra_body() {
        let thinking = serde_json::json!({"thinking": {"type": "enabled"}, "reasoning_effort": "high", "x": 1});
        let kimi = super::ApiClient::new(
            "http://127.0.0.1:9/v1", "k", "k3", 1, thinking.clone(), 16).unwrap();
        assert!(kimi.extra_body.get("thinking").is_none(), "thinking срезан: {}", kimi.extra_body);
        assert!(kimi.extra_body.get("reasoning_effort").is_none(), "effort срезан: {}", kimi.extra_body);
        assert_eq!(kimi.extra_body.get("x"), Some(&serde_json::json!(1)), "прочие ключи целы");
        let ds = super::ApiClient::new(
            "http://127.0.0.1:9/v1", "k", "deepseek-v4-pro", 1, thinking.clone(), 16).unwrap();
        assert!(ds.extra_body.get("thinking").is_some(), "deepseek: thinking сохранён");
        assert!(ds.extra_body.get("reasoning_effort").is_some(), "deepseek: effort сохранён");
        // неизвестная реестру модель — не трогаем (openai-compatible эндпоинты)
        let custom = super::ApiClient::new(
            "http://127.0.0.1:9/v1", "k", "my-local-model", 1, thinking, 16).unwrap();
        assert!(custom.extra_body.get("thinking").is_some());
    }
}
