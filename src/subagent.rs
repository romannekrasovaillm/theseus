//! Субагенты (урок обзора: изоляция контекста + read-only гарантия на уровне тулсета).
//! Глубина делегирования — 1: субагент НЕ получает инструмент `task`
//! (урок тройки: рекурсивное делегирование размывает и контекст, и бюджет).
//!
//! Прогон ведётся по декларативной спеке [`AgentSpec`] из `crate::agents`:
//! системный промпт, суженный тулсет (фильтр общего реестра инструментов),
//! бюджет (ходы/токены/настенное время) и readonly-гарантия. Итог — компактный
//! [`AgentResult`]: транскрипт субагента родителю не нужен (изоляция контекста).

use crate::agents::{AgentBudget, AgentResult, AgentSpec, BudgetGuard};
use crate::api::{ApiClient, Message};
use crate::background::BgRegistry;
use crate::tools::{self, ToolEnv};
use anyhow::Result;
use std::path::Path;

pub struct SubConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_secs: u64,
    pub extra_body: serde_json::Value,
    pub max_output_tokens: usize,
}

/// Тулсет субагента из общего реестра инструментов (`tool_specs`): фильтр по
/// `spec.allowed_tools` минус `task` (глубина делегирования — 1). Readonly
/// спеки получают только читающие инструменты — гарантия на уровне тулсета,
/// а не на честном слове промпта (урок обзора тройки).
fn sub_toolset(spec: &AgentSpec) -> serde_json::Value {
    let mut all = crate::tools::tool_specs();
    if let Some(arr) = all.as_array_mut() {
        arr.retain(|t| {
            let name = t["function"]["name"].as_str().unwrap_or("");
            name != "task" && spec.allows_tool(name)
        });
    }
    all
}

/// Исполнение одного вызова внутри субагента: файловые/shell — через ToolEnv,
/// веб — свободные функции, фон — собственный BgRegistry прогона.
fn dispatch_sub(env: &mut ToolEnv, bg: &mut BgRegistry, workspace: &Path,
                name: &str, args: &serde_json::Value) -> String {
    match name {
        "web_fetch" => tools::web_fetch(args["url"].as_str().unwrap_or(""), 30)
            .unwrap_or_else(|e| format!("ERROR: {e}")),
        "web_search" => tools::web_search(args["query"].as_str().unwrap_or(""), 30)
            .unwrap_or_else(|e| format!("ERROR: {e}")),
        "task_output" => bg.output(args["id"].as_u64().unwrap_or(0)),
        "task_stop" => bg.stop(args["id"].as_u64().unwrap_or(0)),
        "bash" if args["is_background"].as_bool().unwrap_or(false) => {
            match bg.spawn(args["command"].as_str().unwrap_or(""), &workspace.to_path_buf()) {
                Ok(id) => format!("[bg {id}] запущена в фоне; читайте task_output"),
                Err(e) => format!("ERROR: {e}"),
            }
        }
        _ => env.call(name, args),
    }
}

/// Прогон субагента по спеке: изолированный agent-loop со своим бюджетом.
/// `sandbox` — флаг ядерной песочницы родителя (пробрасывается в bash субагента).
pub fn run_agent(cfg: &SubConfig, workspace: &Path, spec: &AgentSpec,
                 prompt: &str, budget: AgentBudget, sandbox: bool) -> Result<AgentResult> {
    let mut api = ApiClient::new(&cfg.base_url, &cfg.api_key, &cfg.model,
                                 cfg.timeout_secs, cfg.extra_body.clone(), cfg.max_output_tokens)?;
    let mut env = ToolEnv::new(workspace);
    env.sandbox = sandbox;
    let mut bg = BgRegistry::new();
    let mut messages = vec![
        Message::system(spec.system_prompt.clone()),
        Message::user(prompt.to_string()),
    ];
    let tools = sub_toolset(spec);
    let mut guard = BudgetGuard::new(budget);
    loop {
        // настенный лимит — перед очередным обращением к модели
        if let Err(e) = guard.check() {
            return Ok(AgentResult::from_guard(
                format!("(субагент «{}» остановлен: {e})", spec.name), &guard, true));
        }
        let resp = api.chat(&messages, &tools)?;
        // счётные лимиты (ходы/токены) — после хода; обрыв фиксируем, но ход дозавершаем
        let over = guard.consume(resp.prompt_tokens + resp.completion_tokens).err();
        let has_tools = !resp.tool_calls.is_empty();
        messages.push(Message::assistant(resp.content.clone(),
            if has_tools { Some(resp.tool_calls.clone()) } else { None }));
        if !has_tools {
            let text = resp.content.unwrap_or_else(|| "(субагент завершил без текста)".into());
            return Ok(AgentResult::from_guard(text, &guard, over.is_some()));
        }
        for call in &resp.tool_calls {
            let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                .unwrap_or(serde_json::json!({}));
            let out = if spec.allows_tool(&call.function.name) {
                dispatch_sub(&mut env, &mut bg, workspace, &call.function.name, &args)
            } else {
                format!("DENIED: субагент «{}» не имеет инструмента {}", spec.name, call.function.name)
            };
            messages.push(Message::tool(&call.id, out));
        }
        if let Some(e) = over {
            return Ok(AgentResult::from_guard(
                format!("(субагент «{}» остановлен по бюджету: {e})", spec.name), &guard, true));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::builtin_specs;

    /// Тулсет субагента строго по спеке: только разрешённые инструменты и
    /// никогда — `task` (глубина делегирования 1); у readonly-спек в тулсете
    /// нет ни одного пишущего инструмента (WRITE_TOOLS).
    #[test]
    fn sub_toolset_follows_spec_and_drops_task() {
        for spec in builtin_specs() {
            let tools = sub_toolset(&spec);
            let names: Vec<&str> = tools.as_array().expect("массив тулсета").iter()
                .filter_map(|t| t["function"]["name"].as_str()).collect();
            assert!(!names.is_empty(), "пустой тулсет у «{}»", spec.name);
            assert!(!names.contains(&"task"), "task попал в тулсет «{}»", spec.name);
            for n in &names {
                assert!(spec.allows_tool(n), "«{n}» сверх спеки «{}»", spec.name);
                if spec.readonly {
                    assert!(!crate::agents::WRITE_TOOLS.contains(n),
                        "пишущий «{n}» в readonly-спеке «{}»", spec.name);
                }
            }
            // все разрешённые спекой инструменты реально присутствуют
            for want in &spec.allowed_tools {
                assert!(names.contains(&want.as_str()),
                    "«{want}» из спеки «{}» не найден в общем реестре", spec.name);
            }
        }
    }
}
