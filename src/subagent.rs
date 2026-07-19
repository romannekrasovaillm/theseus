//! Субагент explore (урок обзора: изоляция контекста + read-only гарантия на уровне тулсета).
//! Глубина 1: субагент НЕ получает инструмент task.

use crate::api::{ApiClient, Message};
use crate::tools::ToolEnv;
use anyhow::Result;
use std::path::Path;

const EXPLORE_PROMPT: &str = "You are a fast, read-only codebase exploration agent. \
    Answer the user's question by reading files with read_file/list_files/grep. \
    You cannot modify anything. When done, reply with a concise factual answer with file:line refs.";

fn read_only_specs() -> serde_json::Value {
    serde_json::json!([
        {"type":"function","function":{
            "name":"read_file",
            "description":"Read a text file (numbered lines).",
            "parameters":{"type":"object","properties":{
                "path":{"type":"string"},
                "offset":{"type":"integer"},
                "limit":{"type":"integer"}
            },"required":["path"]}}},
        {"type":"function","function":{
            "name":"list_files",
            "description":"List files in a directory.",
            "parameters":{"type":"object","properties":{
                "path":{"type":"string"},
                "max_results":{"type":"integer"}
            }}}},
        {"type":"function","function":{
            "name":"grep",
            "description":"Search files for a substring. Returns path:line: text.",
            "parameters":{"type":"object","properties":{
                "pattern":{"type":"string"},
                "path":{"type":"string"}
            },"required":["pattern"]}}}
    ])
}

pub struct SubConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_secs: u64,
    pub extra_body: serde_json::Value,
    pub max_output_tokens: usize,
}

/// Прогон explore-субагента: свой Agent-loop с read-only инструментами
pub fn run_explore(cfg: &SubConfig, workspace: &Path, prompt: &str, max_turns: usize) -> Result<String> {
    let mut api = ApiClient::new(&cfg.base_url, &cfg.api_key, &cfg.model,
                                 cfg.timeout_secs, cfg.extra_body.clone(), cfg.max_output_tokens)?;
    let mut env = ToolEnv::new(workspace);
    let mut messages = vec![
        Message::system(EXPLORE_PROMPT),
        Message::user(prompt),
    ];
    let tools = read_only_specs();
    for _turn in 1..=max_turns {
        let resp = api.chat(&messages, &tools)?;
        let has_tools = !resp.tool_calls.is_empty();
        messages.push(Message::assistant(resp.content.clone(),
            if has_tools { Some(resp.tool_calls.clone()) } else { None }));
        if !has_tools {
            return Ok(resp.content.unwrap_or_else(|| "(субагент завершил без текста)".into()));
        }
        for call in &resp.tool_calls {
            let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                .unwrap_or(serde_json::json!({}));
            let out = match call.function.name.as_str() {
                "read_file" | "list_files" | "grep" => env.call(&call.function.name, &args),
                other => format!("DENIED: read-only субагент не имеет инструмента {other}"),
            };
            messages.push(Message::tool(&call.id, out));
        }
    }
    Ok("(субагент: лимит ходов)".into())
}
