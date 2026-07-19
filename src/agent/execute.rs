//! Решения разрешений и исполнение инструментов (с детекторами цикла).

use super::detectors::fingerprint;
use super::events::AgentEvent;
use super::{Agent, GoalState};
use crate::api::ToolCall;
use crate::permissions::{Decision, Mode};
use crate::skills;
use crate::subagent;
use crate::tools;
use std::sync::atomic::Ordering;

/// Вызов внешнего peer-агента по имени (crate::peers): спек из builtin-реестра,
/// исполнение в workspace агента с таймаутом из конфига спеки или args.
fn run_peer_ask(agent: &Agent, name: &str, task: &str, timeout_secs: Option<u64>) -> String {
    let Some(spec) = crate::peers::builtin_peers().into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
    else {
        let known = crate::peers::builtin_peers().iter()
            .map(|p| p.name.clone()).collect::<Vec<_>>().join(", ");
        return format!("ERROR: неизвестный peer-агент «{name}». Доступно: {known}");
    };
    let timeout = std::time::Duration::from_secs(
        timeout_secs.unwrap_or(spec.default_timeout_secs).min(600));
    match crate::peers::peer_ask(&spec, task, &agent.workspace, timeout) {
        Ok(out) => out,
        Err(e) => format!("ERROR: peer «{}» недоступен или упал: {e:#}", spec.name),
    }
}

impl Agent {
    pub(crate) fn decide(&mut self, name: &str, args: &serde_json::Value) -> Decision {
        // пользовательские правила — первыми (v0.3)
        let target = match name {
            "bash" => args["command"].as_str().unwrap_or(""),
            _ => args["path"].as_str().unwrap_or(""),
        };
        if let Some(d) = self.perms.rule_decision(name, target) {
            return d;
        }
        // plan-режим (v0.3): только чтение
        if self.controls.plan.load(Ordering::Relaxed) {
            match name {
                "write_file" | "edit_file" => {
                    return Decision::Deny("plan mode: правки запрещены до exit_plan_mode/одобрения".into());
                }
                "bash" if !self.perms.is_readonly_bash(target) => {
                    return Decision::Deny("plan mode: разрешены только read-only команды".into());
                }
                _ => {}
            }
        }
        match name {
            "read_file" | "list_files" | "grep" => {
                let p = args["path"].as_str().unwrap_or(".");
                self.perms.file_read(p)
            }
            "write_file" | "edit_file" => {
                let p = args["path"].as_str().unwrap_or("");
                self.perms.file_write(p)
            }
            "bash" => self.perms.bash(args["command"].as_str().unwrap_or("")),
            "peer_ask" | "memory_write" if self.perms.mode() == Mode::SemiAuto => {
                // полуавтомат: внешние агенты и долговременная память — с подтверждением
                Decision::Ask(format!("полуавтомат: подтвердите «{name}»"))
            }
            "web_fetch" => {
                let url = args["url"].as_str().unwrap_or("");
                let host = url.split('/').nth(2).unwrap_or("");
                if self.web_domains.is_empty() {
                    Decision::Deny("web_fetch выключен (web_allowed_domains пуст)".into())
                } else if self.web_domains.iter().any(|d| host.ends_with(d.as_str())) {
                    Decision::Allow
                } else {
                    Decision::Deny(format!("домен вне allow-list: {host}"))
                }
            }
            "web_search" => {
                if self.web_domains.is_empty() {
                    Decision::Deny("web_search выключен (web_allowed_domains пуст)".into())
                } else {
                    Decision::Allow
                }
            }
            _ => Decision::Allow,
        }
    }

    pub(crate) fn execute(&mut self, call: &ToolCall) -> String {
        let name = call.function.name.clone();
        let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
            .unwrap_or(serde_json::json!({}));

        // doom-loop детектор (OpenDev #7): ≥3 идентичных (tool,args) в окне 20
        let fp = fingerprint(&name, &args);
        if self.doom_warned.contains(&fp) {
            return "DENIED (doom-loop guard): идентичный вызов уже пропускался после предупреждения — измените подход".into();
        }
        self.fp_window.push_back(fp);
        if self.fp_window.len() > 20 { self.fp_window.pop_front(); }
        let count = self.fp_window.iter().filter(|x| **x == fp).count();
        if count >= 3 && !matches!(name.as_str(), "todo_write" | "finish") {
            self.doom_warned.insert(fp);
            self.emit(AgentEvent::HookNote(format!(
                "⚠ doom-loop: «{name}» ×{count} с одинаковыми аргументами (окно 20)")));
            return format!("[SYSTEM WARNING: doom loop suspected — «{name}» с теми же аргументами уже встречался {count} раз в окне 20. Вызов пропущен. Измените стратегию.]");
        }

        // deny-repeat (OpenDev #6): тот же вызов сразу после отказа
        if self.last_deny_fp == Some(fp) {
            let fires = self.reminder_fires.entry("deny".into()).or_insert(0);
            if *fires < 2 {
                *fires += 1;
                return "REMINDER: этот вызов с теми же аргументами уже был отклонён — не повторяйте его; измените аргументы или подход.".into();
            }
        }

        // PreToolUse-хук (единый движок hooks_ext, V3 #2.2)
        let hook_out = self.fire_ext(crate::hooks_ext::HookEvent::PreToolUse,
            serde_json::json!({"tool": name, "args": args}));
        let reason = crate::hooks_ext::block_reason(&hook_out);
        if !reason.is_empty() {
            self.emit(AgentEvent::HookNote(format!("⛔ хук заблокировал: {reason}")));
            self.emit(AgentEvent::ToolResult { name, preview: reason.clone(), ok: false });
            self.last_deny_fp = Some(fp);
            return format!("BLOCKED by hook: {reason}");
        }

        let out = self.execute_inner(call, &name, args.clone());
        // PostToolUse: stdout хуков добавляется к результату (семантика ext)
        let post = self.fire_ext(crate::hooks_ext::HookEvent::PostToolUse,
            serde_json::json!({"tool": name, "args": args, "ok": !out.starts_with("ERROR")}));
        let extra = crate::hooks_ext::collect_stdout(&post);
        let out = if extra.is_empty() { out } else { format!("{out}\n[hook stdout] {extra}") };
        // deny-repeat tracking
        if out.starts_with("DENIED") || out.starts_with("BLOCKED") {
            self.last_deny_fp = Some(fp);
        } else {
            self.last_deny_fp = None;
        }
        out
    }

    fn execute_inner(&mut self, call: &ToolCall, name: &str, args: serde_json::Value) -> String {
        // v0.2: MCP-роутинг
        if let Some(rest) = name.strip_prefix("mcp__") {
            let (server, tool) = rest.split_once("__").unwrap_or((rest, ""));
            self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: "Allow (MCP)".into() });
            let out = match &mut self.mcp {
                Some(reg) => reg.call(server, tool, args).unwrap_or_else(|e| format!("ERROR: {e}")),
                None => "ERROR: MCP не подключён".into(),
            };
            let ok = !out.starts_with("ERROR");
            self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(200).collect(), ok });
            return out;
        }

        // внутренние инструменты без перм-проверки
        match name {
            "peer_ask" => {
                // мост к внешним CLI-агентам (v0.5.2): мощный инструмент — гейт по режиму:
                // DontAsk → Deny (некому подтвердить), Ask → Ask (попап), Yolo → Allow.
                let agent = args["agent"].as_str().unwrap_or("").to_lowercase();
                let task = args["task"].as_str().unwrap_or("").to_string();
                let mode = self.perms.mode();
                // решение как внутреннее представление: Allow исполняем сразу,
                // Ask — через попап, Deny — с причиной
                enum PeerGate { Allow, Ask(String), Deny(String) }
                let gate = match mode {
                    Mode::DontAsk => PeerGate::Deny(
                        "peer_ask заблокирован в режиме DontAsk — запрос к внешнему агенту требует подтверждения".into()),
                    Mode::Ask | Mode::SemiAuto => PeerGate::Ask(format!(
                        "выполнить задачу у внешнего агента «{agent}»: {task}")),
                    Mode::Yolo => PeerGate::Allow,
                };
                let decision_label = match &gate {
                    PeerGate::Allow => "Allow",
                    PeerGate::Ask(_) => "Ask",
                    PeerGate::Deny(_) => "Deny",
                };
                self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(),
                    decision: decision_label.into() });
                let out = match gate {
                    PeerGate::Deny(reason) => format!("DENIED: {reason}"),
                    PeerGate::Ask(question) => {
                        let allow = self.perm_answerer.as_mut().is_some_and(|f| f(&question));
                        if allow {
                            run_peer_ask(self, &agent, &task, args["timeout_secs"].as_u64())
                        } else {
                            format!("DENIED: пользователь отклонил peer_ask к «{agent}»")
                        }
                    }
                    PeerGate::Allow => run_peer_ask(self, &agent, &task, args["timeout_secs"].as_u64()),
                };
                let ok = !out.starts_with("DENIED") && !out.starts_with("ERROR");
                self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(200).collect(), ok });
                return out;
            }
            "task" => {
                let prompt = args["prompt"].as_str().unwrap_or("").to_string();
                self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: "Allow (subagent explore)".into() });
                let out = match subagent::run_explore(&self.sub, &self.workspace, &prompt, 10) {
                    Ok(s) => s,
                    Err(e) => format!("ERROR: субагент: {e}"),
                };
                let ok = !out.starts_with("ERROR");
                self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(200).collect(), ok });
                return out;
            }
            "skill_search" => {                // прогрессивное раскрытие: поиск по имени/описанию без загрузки тел
                let q = args["query"].as_str().unwrap_or("");
                let limit = args["limit"].as_u64().unwrap_or(8) as usize;
                let hits = skills::search(&self.skills, q, limit);
                let out = if hits.is_empty() {
                    format!("скиллов по запросу «{q}» не найдено (всего: {})", self.skills.len())
                } else {
                    let mut s = format!("скиллов по «{q}»: {} (всего в библиотеке {})\n", hits.len(), self.skills.len());
                    for sk in hits {
                        let d: String = sk.description.chars().take(100).collect();
                        s.push_str(&format!("- {}: {}\n", sk.name, d));
                    }
                    s.push_str("Загрузить полный текст: инструмент skill {name}.");
                    s
                };
                self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: "Allow".into() });
                self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(200).collect(), ok: true });
                return out;
            }
            "skill" => {
                let sk = args["name"].as_str().unwrap_or("");
                let out = match self.skills.iter().find(|s| s.name == sk) {
                    Some(spec) => match skills::load_body(spec) {
                        Ok(body) => format!("=== skill {} (из {})\n\n{}", spec.name, spec.path.display(),
                                            crate::tools::cap_pub(body)),
                        Err(e) => format!("ERROR: {e}"),
                    },
                    None => format!("ERROR: скилл «{sk}» не найден. Доступно: {}",
                        self.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")),
                };
                let ok = !out.starts_with("ERROR");
                self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: "Allow".into() });
                self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(200).collect(), ok });
                return out;
            }
            "memory_write" => {
                let fact = args["fact"].as_str().unwrap_or("");
                let out = match &self.memory {
                    Some(m) => m.write_fact(fact),
                    None => "ERROR: память недоступна".into(),
                };
                self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: "Allow".into() });
                self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(150).collect(), ok: !out.starts_with("ERROR") });
                return out;
            }
            "memory_search" => {
                let q = args["query"].as_str().unwrap_or("");
                let out = match &self.memory {
                    Some(m) => m.search(q, 5),
                    None => "ERROR: память недоступна".into(),
                };
                self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: "Allow".into() });
                self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(150).collect(), ok: !out.starts_with("ERROR") });
                return out;
            }
            "task_output" => {
                let id = args["id"].as_u64().unwrap_or(0);
                let out = self.bg.output(id);
                self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: "Allow".into() });
                self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(200).collect(), ok: !out.starts_with("ERROR") });
                return out;
            }
            "task_stop" => {
                let id = args["id"].as_u64().unwrap_or(0);
                let out = self.bg.stop(id);
                self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: "Allow".into() });
                self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.clone(), ok: true });
                return out;
            }
            "exit_plan_mode" => {
                self.controls.plan.store(false, Ordering::Relaxed);
                self.emit(AgentEvent::PlanChanged(false));
                let summary = args["plan_summary"].as_str().unwrap_or("");
                return format!("PLAN_APPROVED: {summary}. Разрешена реализация.");
            }
            "set_goal" => {
                let text = args["text"].as_str().unwrap_or("").to_string();
                let max_turns = args["max_turns"].as_u64().unwrap_or(10) as usize;
                self.goal = Some(GoalState { text: text.clone(), max_turns, turns_used: 0, audit_sent: false });
                self.emit(AgentEvent::GoalSet(text.clone()));
                return format!("GOAL_SET: {text} (аудит до {max_turns} ходов)");
            }
            _ => {}
        }

        let decision = self.decide(name, &args);
        let dstr = format!("{decision:?}");
        match decision {
            Decision::Allow => {
                self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: dstr });
                // v0.3: bash is_background
                if name == "bash" && args["is_background"].as_bool().unwrap_or(false) {
                    let cmd = args["command"].as_str().unwrap_or("");
                    let out = match self.bg.spawn(cmd, &self.workspace) {
                        Ok(id) => format!("[bg {id}] запущена в фоне; читайте task_output"),
                        Err(e) => format!("ERROR: {e}"),
                    };
                    let ok = !out.starts_with("ERROR");
                    self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.clone(), ok });
                    return out;
                }
                if name == "web_fetch" {
                    let out = tools::web_fetch(args["url"].as_str().unwrap_or(""), 30)
                        .unwrap_or_else(|e| format!("ERROR: {e}"));
                    let ok = !out.starts_with("ERROR");
                    self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(200).collect(), ok });
                    return out;
                }
                if name == "web_search" {
                    let out = tools::web_search(args["query"].as_str().unwrap_or(""), 30)
                        .unwrap_or_else(|e| format!("ERROR: {e}"));
                    let ok = !out.starts_with("ERROR");
                    self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(200).collect(), ok });
                    return out;
                }
                let out = self.env.call(name, &args);
                let ok = !out.starts_with("ERROR");
                self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(200).collect(), ok });
                out
            }
            Decision::Deny(reason) => {
                self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: dstr });
                self.emit(AgentEvent::ToolResult { name: name.into(), preview: reason.clone(), ok: false });
                format!("DENIED: {reason}")
            }
            Decision::Ask(question) => {
                self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: dstr });
                let allow = match self.perm_answerer.as_mut() {
                    Some(f) => f(&question),
                    None => false,
                };
                if allow {
                    self.emit(AgentEvent::ToolResult { name: name.into(), preview: "разрешено пользователем".into(), ok: true });
                    let out = self.env.call(name, &args);
                    let ok = !out.starts_with("ERROR");
                    self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(200).collect(), ok });
                    out
                } else {
                    self.emit(AgentEvent::ToolResult { name: name.into(), preview: "отклонено пользователем".into(), ok: false });
                    format!("DENIED: пользователь отклонил ({question})")
                }
            }
        }
    }


}
