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
fn run_peer_ask(agent: &Agent, name: &str, task: &str, timeout_secs: Option<u64>) -> String {    let Some(spec) = crate::peers::builtin_peers().into_iter()
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

/// Имя — реальный инструмент агента (по реестру tool_specs)?
/// Автоподхват скилла не должен тенить настоящие тулы.
fn skill_tool_name_is_real_tool(name: &str) -> bool {
    tools::tool_specs().as_array()
        .map(|a| a.iter().any(|t| t["function"]["name"].as_str() == Some(name)))
        .unwrap_or(false)
}

/// Сопоставить имя вызова со скиллом: нормализация '_'→'-' и lowercase
/// (agent_sessions → agent-sessions). Чистая функция — для тестов.
fn resolve_skill_as_tool<'a>(name: &str, skills: &'a [skills::SkillSpec]) -> Option<&'a skills::SkillSpec> {
    let normalized = name.to_lowercase().replace('_', "-");
    if normalized.is_empty() {
        return None;
    }
    skills.iter().find(|s| s.name.eq_ignore_ascii_case(&normalized))
}

/// Потолок задач в одном рое: защита от штампа параллельных API-вызовов.
const SWARM_MAX_TASKS: usize = 8;

/// Достройка обрезанного JSON аргументов инструмента (живой кейс 24.07:
/// «{"id": 1» без закрывающей скобки — тихий откат в {} дал «задачу 0»).
/// Достраиваем кавычку и скобки по стеку; хвостовую запятую срезаем.
/// Ключевые слова/числа, оборванные посередине («tru», «12.»), не ремонтируем
/// — тогда None и штатная ошибка невалидного JSON. Чистая функция — для тестов.
pub(crate) fn repair_truncated_json(text: &str) -> Option<serde_json::Value> {
    let mut out = text.trim_end().to_string();
    while out.ends_with(',') {
        out.pop();
        out = out.trim_end().to_string();
    }
    let mut in_string = false;
    let mut escape = false;
    let mut stack: Vec<char> = vec![];
    for ch in out.chars() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_string => escape = true,
            '"' => in_string = !in_string,
            '{' | '[' if !in_string => stack.push(ch),
            '}' if !in_string => {
                if stack.pop() != Some('{') {
                    return None;
                }
            }
            ']' if !in_string => {
                if stack.pop() != Some('[') {
                    return None;
                }
            }
            _ => {}
        }
    }
    if in_string {
        out.push('"');
    }
    for open in stack.iter().rev() {
        out.push(if *open == '{' { '}' } else { ']' });
    }
    serde_json::from_str(&out).ok()
}

/// Разбор аргументов инструмента swarm (чистая функция — для тестов):
/// массив tasks (1..=8), у каждой prompt (обязателен) и agent (default explore).
pub(crate) fn parse_swarm_tasks(args: &serde_json::Value, registry: &crate::agents::AgentRegistry)
    -> Result<Vec<(crate::agents::AgentSpec, String)>, String> {
    let arr = args["tasks"].as_array().ok_or("swarm: нужен массив tasks")?;
    if arr.is_empty() {
        return Err("swarm: пустой массив tasks — рою нечего делать".into());
    }
    if arr.len() > SWARM_MAX_TASKS {
        return Err(format!(
            "swarm: не больше {SWARM_MAX_TASKS} задач за раз (получено {}) — \
             разбейте на несколько роёв", arr.len()));
    }
    let mut out = vec![];
    for (i, item) in arr.iter().enumerate() {
        let prompt = item["prompt"].as_str().unwrap_or("").trim().to_string();
        if prompt.is_empty() {
            return Err(format!("swarm: задача #{} без prompt", i + 1));
        }
        let agent = item["agent"].as_str().unwrap_or("explore");
        let spec = registry.get(agent).map_err(|e| format!("swarm: {e}"))?;
        out.push((spec.clone(), prompt));
    }
    Ok(out)
}

/// Ожидание фоновых задач и сбор результатов одним ответом (чистая функция —
/// для тестов): опрос is_done два раза в секунду до завершения всех ids или
/// таймаута; незавершившиеся честно помечаются, их можно добрать task_output.
pub(crate) fn collect_bg_results(bg: &mut crate::background::BgRegistry, ids: &[u64],
                      timeout: std::time::Duration) -> String {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let all_done = ids.iter().all(|id| bg.is_done(*id) != Some(false));
        if all_done || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let mut out = String::new();
    let mut pending = 0usize;
    for id in ids {
        match bg.is_done(*id) {
            None => out.push_str(&format!("[bg {id}] ERROR: задача не найдена\n\n")),
            Some(done) => {
                if !done {
                    pending += 1;
                }
                out.push_str(&bg.output(*id));
                out.push_str("\n\n");
            }
        }
    }
    if pending > 0 {
        out.push_str(&format!(
            "[swarm_wait] {pending} задач не завершились за таймаут — \
             заберите их позже через task_output по id\n"));
    }
    out
}

impl Agent {
    /// Автоподхват скилла, вызванного как инструмент (живой кейс 23.07: модель
    /// позвала «agent_sessions» — это скилл agent-sessions, а не tool).
    /// Если имя не совпадает с реальным инструментом, но совпадает со скиллом
    /// (нормализация '_'→'-', без учёта регистра) — возвращаем тело скилла
    /// с пояснением вместо «unknown tool». Реальные инструменты не теним.
    pub(crate) fn skill_invoked_as_tool(&self, name: &str) -> Option<String> {
        if skill_tool_name_is_real_tool(name) {
            return None;
        }
        let spec = resolve_skill_as_tool(name, &self.skills)?;
        let body = skills::load_body(spec).ok()?;
        Some(format!(
            "«{name}» — это скилл «{}», а не инструмент (инструмента с таким именем нет). \
             Тело скилла загружено автоматически — действуйте по его инструкциям \
             (скрипты запускайте через bash, файлы читайте read_file):\n\n=== skill {} (из {})\n\n{}",
            spec.name, spec.name, spec.path.display(), tools::cap_pub(body)))
    }

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
        // обрезанные аргументы (живой кейс 24.07: модель прислала «{"id": 1»
        // без закрывающей скобки; тихий дефолт в {} дал фантомную «задачу 0»):
        // достраиваем, а если нельзя — честная ошибка вместо пустых аргументов
        let args: serde_json::Value = match serde_json::from_str(&call.function.arguments) {
            Ok(v) => v,
            Err(_) => match repair_truncated_json(&call.function.arguments) {
                Some(v) => {
                    self.emit(AgentEvent::HookNote(format!(
                        "⚠ аргументы «{name}» были обрезаны моделью — достроены до валидного JSON")));
                    v
                }
                None => {
                    let short: String = call.function.arguments.chars().take(120).collect();
                    self.emit(AgentEvent::ToolCall { name: name.clone(),
                        args: call.function.arguments.clone(), decision: "Deny (bad json)".into() });
                    let out = format!(
                        "ERROR: невалидный JSON в аргументах инструмента «{name}»: «{short}». \
                         Вызов НЕ выполнен — повторите с корректным JSON.");
                    self.emit(AgentEvent::ToolResult { name: name.clone(),
                        preview: out.chars().take(200).collect(), ok: false });
                    return out;
                }
            },
        };

        // doom-loop детектор (OpenDev #7): ≥3 идентичных (tool,args) в окне 20
        let fp = fingerprint(&name, &args);
        if self.doom_warned.contains(&fp) {
            return "DENIED (doom-loop guard): идентичный вызов уже пропускался после предупреждения — измените подход".into();
        }
        self.fp_window.push_back(fp);
        if self.fp_window.len() > 20 { self.fp_window.pop_front(); }
        let count = self.fp_window.iter().filter(|x| **x == fp).count();
        // исключения: todo_write/finish — штатно повторяются; task_output —
        // поллинг фоновой задачи (peers живут минуты) — легитимное ожидание,
        // а не петля: внешняя граница — лимит ходов (живой кейс 23.07: doom
        // заблокировал получение ответов peer-задач — «статус заблокирован»)
        if count >= 3 && !matches!(name.as_str(), "todo_write" | "finish" | "task_output") {
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

    /// Прогон субагента из инструмента task: бюджет из спеки (default_budget
    /// по типу), итог с пометкой обрыва по бюджету и учётом ходов/токенов.
    fn run_subagent(&self, spec: &crate::agents::AgentSpec, prompt: &str) -> String {
        let budget = crate::agents::default_budget(spec);
        match subagent::run_agent(&self.sub, &self.workspace, spec, prompt, budget, self.env.sandbox, None) {
            Ok(res) => {
                let note = if res.truncated { ", ОБОРВАН ПО БЮДЖЕТУ" } else { "" };
                crate::tools::cap_pub(format!("{}\n[subagent {}: {} ходов, {} токенов{}]",
                    res.summary, spec.name, res.turns, res.tokens, note))
            }
            Err(e) => format!("ERROR: субагент «{}»: {e}", spec.name),
        }
    }

    /// Запуск субагента синхронно или в ФОНЕ (v0.6.6, is_background): в фоне —
    /// поток в BgRegistry, Тесей продолжает работу; результат — task_output.
    fn spawn_or_run_subagent(&mut self, spec: &crate::agents::AgentSpec,
                             prompt: &str, is_background: bool) -> String {
        if !is_background {
            return self.run_subagent(spec, prompt);
        }
        let id = self.spawn_bg_subagent(spec, prompt);
        format!("[bg {id}] субагент «{}» запущен в фоне — продолжайте работу; \
                 результат заберите через task_output", spec.name)
    }

    /// Общий запуск фонового субагента (task is_background и рой swarm):
    /// поток в BgRegistry, возвращает id задачи.
    fn spawn_bg_subagent(&mut self, spec: &crate::agents::AgentSpec, prompt: &str) -> u64 {
        // owned-значения для потока (self в фон унести нельзя)
        let sub_cfg = crate::subagent::SubConfig {
            base_url: self.sub.base_url.clone(),
            api_key: self.sub.api_key.clone(),
            model: self.sub.model.clone(),
            timeout_secs: self.sub.timeout_secs,
            extra_body: self.sub.extra_body.clone(),
            max_output_tokens: self.sub.max_output_tokens,
        };
        let ws = self.workspace.clone();
        let spec2 = spec.clone();
        let prompt2 = prompt.to_string();
        let budget = crate::agents::default_budget(spec);
        let sandbox = self.env.sandbox;
        // флаг кооперативной остановки: task_stop выставит его, субагент
        // прочитает на границе хода (живой кейс 24.07 — explore висел 25 минут)
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel2 = cancel.clone();
        let label = format!("subagent {} — {}", spec.name,
            prompt.chars().take(60).collect::<String>());
        self.bg.spawn_fn(label, cancel, move || {
            match crate::subagent::run_agent(&sub_cfg, &ws, &spec2, &prompt2, budget, sandbox,
                                             Some(&cancel2)) {
                Ok(res) => {
                    let note = if res.truncated { ", ОБОРВАН ПО БЮДЖЕТУ" } else { "" };
                    format!("{}\n[subagent {}: {} ходов, {} токенов{}]",
                        res.summary, spec2.name, res.turns, res.tokens, note)
                }
                Err(e) => format!("ERROR: субагент «{}»: {e}", spec2.name),
            }
        })
    }

    /// Запуск роя: все задачи в фон одним махом, ответ — карта bg-id для
    /// последующего сбора через swarm_wait (или point-опроса task_output).
    fn launch_swarm(&mut self, specs: &[(crate::agents::AgentSpec, String)]) -> String {
        let mut ids = vec![];
        let mut lines = vec![];
        for (spec, prompt) in specs {
            let id = self.spawn_bg_subagent(spec, prompt);
            ids.push(id.to_string());
            lines.push(format!("bg {id}: {} — {}", spec.name,
                prompt.chars().take(50).collect::<String>()));
        }
        format!("[swarm] запущено {} задач в фоне:\n{}\nПродолжайте работу; \
                 соберите все результаты одним вызовом swarm_wait {{\"ids\": [{}]}} \
                 (или точечно task_output по id).",
            specs.len(), lines.join("\n"), ids.join(", "))
    }

    /// Запуск peer-агента синхронно или в ФОНЕ (v0.6.6, is_background).
    fn spawn_or_run_peer(&mut self, agent: &str, task: &str,
                         timeout_secs: Option<u64>, is_background: bool) -> String {
        if !is_background {
            return run_peer_ask(self, agent, task, timeout_secs);
        }
        let Some(pspec) = crate::peers::builtin_peers().into_iter()
            .find(|p| p.name.eq_ignore_ascii_case(agent)) else {
            let known = crate::peers::builtin_peers().iter()
                .map(|p| p.name.clone()).collect::<Vec<_>>().join(", ");
            return format!("ERROR: неизвестный peer-агент «{agent}». Доступно: {known}");
        };
        let timeout = std::time::Duration::from_secs(
            timeout_secs.unwrap_or(pspec.default_timeout_secs).min(600));
        let ws = self.workspace.clone();
        let task2 = task.to_string();
        let label = format!("peer {} — {}", pspec.name,
            task.chars().take(60).collect::<String>());
        let name2 = pspec.name.clone();
        let id = self.bg.spawn_fn(label,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)), move || {
            match crate::peers::peer_ask(&pspec, &task2, &ws, timeout) {
                Ok(out) => out,
                Err(e) => format!("ERROR: peer «{name2}» недоступен или упал: {e:#}"),
            }
        });
        format!("[bg {id}] peer «{agent}» запущен в фоне — продолжайте работу; \
                 результат заберите через task_output")
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
                let is_bg = args["is_background"].as_bool().unwrap_or(false);
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
                            self.spawn_or_run_peer(&agent, &task, args["timeout_secs"].as_u64(), is_bg)
                        } else {
                            format!("DENIED: пользователь отклонил peer_ask к «{agent}»")
                        }
                    }
                    PeerGate::Allow => self.spawn_or_run_peer(&agent, &task, args["timeout_secs"].as_u64(), is_bg),
                };
                let ok = !out.starts_with("DENIED") && !out.starts_with("ERROR");
                self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(200).collect(), ok });
                return out;
            }
            "task" => {
                let agent_name = args["agent"].as_str().unwrap_or("explore").to_string();
                let prompt = args["prompt"].as_str().unwrap_or("").to_string();
                let is_bg = args["is_background"].as_bool().unwrap_or(false);
                // маршрутизация по реестру типов (v0.6.2): explore/plan/code_review/
                // test_runner из crate::agents — раньше был захардкожен только explore
                let registry = crate::agents::AgentRegistry::with_builtins();
                let spec = match registry.get(&agent_name) {
                    Ok(s) => s.clone(),
                    Err(e) => {
                        self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: "Deny".into() });
                        let out = format!("ERROR: {e}");
                        self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(200).collect(), ok: false });
                        return out;
                    }
                };
                // не-readonly спека (test_runner с bash) — гейт по режиму, как у peer_ask:
                // DontAsk → Deny (некому подтвердить), Ask/SemiAuto → попап, Yolo → Allow
                enum SubGate { Allow, Ask(String), Deny(String) }
                let gate = if spec.readonly {
                    SubGate::Allow
                } else {
                    match self.perms.mode() {
                        Mode::DontAsk => SubGate::Deny(format!(
                            "субагент «{}» (есть bash) заблокирован в режиме DontAsk — нужно подтверждение", spec.name)),
                        Mode::Ask | Mode::SemiAuto => SubGate::Ask(format!(
                            "запустить субагента «{}» (есть bash): {prompt}", spec.name)),
                        Mode::Yolo => SubGate::Allow,
                    }
                };
                let decision_label = match &gate {
                    SubGate::Allow => format!("Allow (subagent {})", spec.name),
                    SubGate::Ask(_) => format!("Ask (subagent {})", spec.name),
                    SubGate::Deny(_) => format!("Deny (subagent {})", spec.name),
                };
                self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: decision_label });
                let out = match gate {
                    SubGate::Deny(reason) => format!("DENIED: {reason}"),
                    SubGate::Ask(question) => {
                        let allow = self.perm_answerer.as_mut().is_some_and(|f| f(&question));
                        if allow {
                            self.spawn_or_run_subagent(&spec, &prompt, is_bg)
                        } else {
                            format!("DENIED: пользователь отклонил субагента «{}»", spec.name)
                        }
                    }
                    SubGate::Allow => self.spawn_or_run_subagent(&spec, &prompt, is_bg),
                };
                let ok = !out.starts_with("DENIED") && !out.starts_with("ERROR");
                self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(200).collect(), ok });
                return out;
            }
            "swarm" => {
                // рой субагентов (v0.7): параллельный fan-out одним вызовом.
                // Правовая модель — как у task: не-readonly спеки (есть bash)
                // гейтятся по режиму (DontAsk→Deny, Ask/SemiAuto→попап, Yolo→Allow)
                let registry = crate::agents::AgentRegistry::with_builtins();
                let parsed = parse_swarm_tasks(&args, &registry);
                let specs = match parsed {
                    Ok(s) => s,
                    Err(e) => {
                        self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: "Deny".into() });
                        self.emit(AgentEvent::ToolResult { name: name.into(), preview: e.clone(), ok: false });
                        return format!("ERROR: {e}");
                    }
                };
                let any_write = specs.iter().any(|(s, _)| !s.readonly);
                enum SwarmGate { Allow, Ask(String), Deny(String) }
                let gate = if !any_write {
                    SwarmGate::Allow
                } else {
                    match self.perms.mode() {
                        Mode::DontAsk => SwarmGate::Deny(
                            "рой с не-readonly субагентами (есть bash) заблокирован в режиме DontAsk — нужно подтверждение".into()),
                        Mode::Ask | Mode::SemiAuto => SwarmGate::Ask(format!(
                            "запустить рой из {} субагентов (есть bash)", specs.len())),
                        Mode::Yolo => SwarmGate::Allow,
                    }
                };
                let decision_label = match &gate {
                    SwarmGate::Allow => format!("Allow (swarm {})", specs.len()),
                    SwarmGate::Ask(_) => "Ask (swarm)".to_string(),
                    SwarmGate::Deny(_) => "Deny (swarm)".to_string(),
                };
                self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: decision_label });
                let out = match gate {
                    SwarmGate::Deny(reason) => format!("DENIED: {reason}"),
                    SwarmGate::Ask(question) => {
                        let allow = self.perm_answerer.as_mut().is_some_and(|f| f(&question));
                        if allow {
                            self.launch_swarm(&specs)
                        } else {
                            "DENIED: пользователь отклонил рой субагентов".to_string()
                        }
                    }
                    SwarmGate::Allow => self.launch_swarm(&specs),
                };
                let ok = !out.starts_with("DENIED") && !out.starts_with("ERROR");
                self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(200).collect(), ok });
                return out;
            }
            "swarm_wait" => {
                // сбор результатов роя одним ответом: ждём завершения всех ids
                // (или таймаут) и возвращаем вывод каждой задачи
                let ids: Vec<u64> = args["ids"].as_array()
                    .map(|a| a.iter().filter_map(serde_json::Value::as_u64).collect())
                    .unwrap_or_default();
                let timeout = std::time::Duration::from_secs(
                    args["timeout"].as_u64().unwrap_or(600).min(900));
                let out = if ids.is_empty() {
                    "ERROR: swarm_wait: нужен массив ids (из ответа swarm)".to_string()
                } else {
                    collect_bg_results(&mut self.bg, &ids, timeout)
                };
                let ok = !out.starts_with("ERROR");
                self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: "Allow".into() });
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

        // автоподхват: модель вызвала скилл как инструмент (agent_sessions →
        // agent-sessions). Отдаём тело скилла с пояснением вместо голой
        // «unknown tool» — иначе модель флаила ходы на ровном месте
        // (живой кейс 23.07: skill-harvester → вызов agent_sessions).
        if let Some(out) = self.skill_invoked_as_tool(name) {
            self.emit(AgentEvent::ToolCall { name: name.into(), args: call.function.arguments.clone(), decision: "Allow (skill-as-tool)".into() });
            self.emit(AgentEvent::ToolResult { name: name.into(), preview: out.chars().take(200).collect(), ok: true });
            return out;
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
