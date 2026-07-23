//! Агентный цикл v0.3 (корень модуля): структура Agent, API run/resume, общие хелперы.
//! Разложение по rust-project-setup: события → events.rs, компактификация → compact.rs,
//! исполнение инструментов → execute.rs, детекторы цикла → detectors.rs.

use crate::api::{ApiClient, ChatResponse, Message, ToolCall};
use crate::background::BgRegistry;
use crate::config::{Config, HookConfig};
use crate::hooks_ext::{HookEngine, HookMatcher, HookEvent as ExtHookEvent};
use crate::mcp::McpRegistry;
use crate::memory::Memory;
use crate::permissions::{Decision, PermissionEngine};
use crate::prompts::{EnvContext, PromptBuilder, SkillDigest};
use crate::skills::{self, SkillSpec};
use crate::subagent::SubConfig;
use crate::todo::{TodoItem as GateTodoItem, TodoList, TodoStatus};
use crate::tools::{self, tool_specs, ToolEnv};
use crate::trace::{SpanId, TraceRegistry};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::Sender;
use std::time::Instant;

const SYSTEM_PROMPT: &str = r#"You are Тесей (Theseus) — an autonomous ML-engineering agent inside a TUI harness, created by Роман Некрасов. If asked who you are or who made you, answer exactly that.

Rules that bind the whole session:
- Greetings and direct questions: answer briefly and call finish immediately — do NOT explore the workspace or start work the user did not ask for.
- Work only inside the workspace. Read with read_file/list_files/grep; edit with write_file/edit_file; run with bash. Do NOT use bash echo/cat to read or write files when a dedicated tool exists.
- Before non-trivial work, maintain a plan via todo_write (exactly one in_progress). Finish with finish(summary) only when the task is actually done — pending todos will be rejected (TodoGate).
- Keep going until the task is completely resolved; do not guess or fake results; run scripts you write and show real output. Report outcomes faithfully, never characterize incomplete work as done.
- Minimal diffs: fix the root cause, do not refactor beyond the task. Three similar lines are better than a premature abstraction.
- If a tool call is denied or fails, adapt — do not re-attempt the exact same call.
- Long tasks: use bash is_background for long commands, check with task_output.
- ML knowledge: use concept_search/concept_explain for RL/LLM/agent concepts (/home/roman/library), library_search/library_read for papers and reports (recipes_taxonomy), digest_search/digest_read for daily news digests, hf_collections for HuggingFace collections, and ariadna_ask (local Qwen3.5-4B helper) for quick drafts, classification and simple Q&A.
- Use memory_search for relevant facts about the user/project when it matters; memory_write only for durable facts. Long-term memory lives in ~/.theseus/memory/MEMORY.md OUTSIDE the workspace — access it only via memory_search/memory_write, never read_file.
- Final responses: concise, cite files as path:line where relevant."#;

mod compact;
mod detectors;
mod events;
mod execute;

pub use events::AgentEvent;

/// Общие элементы управления агентом из TUI (v0.3)
#[derive(Clone)]
pub struct Controls {
    pub abort: Arc<AtomicBool>,
    pub plan: Arc<AtomicBool>,
    pub goal_slot: Arc<Mutex<Option<String>>>,
    /// пользовательские вставки посреди хода (урок библиотеки: push-back — норма):
    /// приоритетная очередь (Codex steering/mailbox) — Immediate (Ctrl+S в TUI)
    /// прерывает стрим немедленно, Normal (Enter) ждёт границы хода
    pub prompt_slot: Arc<Mutex<crate::scheduler::PromptQueue>>,
    /// режим разрешений с переключением в рантайме (/mode в TUI):
    /// 0=Ask (с подтверждением), 1=SemiAuto (полуавтомат), 2=Yolo (автомат),
    /// 255=не задано (режим из запуска). Читается PermissionEngine на каждое решение.
    pub mode_atomic: Arc<std::sync::atomic::AtomicU8>,
    /// запрос новой сессии из TUI (/new, /clear): агент при старте следующей
    /// задачи очищает session_history и пер-задачное состояние
    pub reset_session: Arc<AtomicBool>,
    /// число работающих фоновых задач (субагенты/пиры/bash) — для индикатора
    /// «фон: N» в шапке TUI (v0.6.6); BgRegistry инкрементит на старте и
    /// декрементит на завершении
    pub bg_running: Arc<std::sync::atomic::AtomicUsize>,
    /// контекстные заметки от локальных слэш-команд (например, пронумерованный
    /// список кандидатов /skill-search): подмешиваются к следующему промпту,
    /// не порождая отдельного хода (урок: slash-вывод иначе невидим агенту —
    /// баг «загрузи скилл 2» резолвился по чужому списку)
    pub notes_slot: Arc<Mutex<Vec<String>>>,
}

impl Default for Controls {
    fn default() -> Self {
        Controls {
            abort: Arc::new(AtomicBool::new(false)),
            plan: Arc::new(AtomicBool::new(false)),
            goal_slot: Arc::new(Mutex::new(None)),
            prompt_slot: Arc::new(Mutex::new(crate::scheduler::PromptQueue::new())),
            mode_atomic: Arc::new(std::sync::atomic::AtomicU8::new(crate::permissions::MODE_UNSET)),
            reset_session: Arc::new(AtomicBool::new(false)),
            bg_running: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            notes_slot: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

/// Слияние контекстных заметок слэш-команд с промптом пользователя (чистая
/// функция — для тестов). Заметки идут первым блоком, промпт — после; агент
/// видит то же, что пользователь на экране, без отдельного хода.
fn merge_notes_into_prompt(notes: &[String], prompt: &str) -> String {
    if notes.is_empty() {
        return prompt.to_string();
    }
    let mut out = String::new();
    for n in notes {
        out.push_str(n);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(prompt);
    out
}

pub(crate) struct GoalState {
    text: String,
    max_turns: usize,
    turns_used: usize,
    audit_sent: bool,
}

/// Исход одного хода run_turn: продолжить цикл или завершить run_with финальным ответом.
enum TurnFlow {
    Continue,
    Done(String),
}

/// Колбэк подтверждения разрешений (вопрос из попапа → да/нет)
pub type PermAnswerer = Box<dyn FnMut(&str) -> bool + Send>;

/// Потолок продления лимита ходов: лимит можно продлевать батчами не выше
/// `initial_max_turns × N` (v0.6.5). Защита от бесконечной сессии при зажатом «y».
const TURN_LIMIT_CEILING_MULT: usize = 4;

pub struct Agent {
    api: ApiClient,
    perms: PermissionEngine,
    env: ToolEnv,
    workspace: PathBuf,
    /// имя модели (для атрибутов трейсинга api_call)
    model: String,
    context_limit: usize,
    transcript_dir: PathBuf,
    session_ts: u64,
    max_turns: usize,
    pub events: Option<Sender<AgentEvent>>,
    pub perm_answerer: Option<PermAnswerer>,
    pub mcp: Option<McpRegistry>,
    sub: SubConfig,
    last_prompt: usize,
    todo_rejections: usize,
    // v0.3
    pub controls: Controls,
    goal: Option<GoalState>,
    /// единый движок хуков (V3 #2.2): PreToolUse/PostToolUse/UserPromptSubmit/
    /// PreCompact/PostCompact/SessionStart/SessionEnd/Notification/GoalSet
    hooks_ext: HookEngine,
    /// rollout-трейсинг: реестр спанов сессии (JSONL-поток в .theseus/trace-<ts>.jsonl)
    trace: TraceRegistry,
    skills: Vec<SkillSpec>,
    memory: Option<Memory>,
    bg: BgRegistry,
    web_domains: Vec<String>,
    // библиотечные детекторы (OpenDev)
    fp_window: std::collections::VecDeque<u64>,
    doom_warned: std::collections::HashSet<u64>,
    last_deny_fp: Option<u64>,
    spiral_reads: usize,
    reminder_fires: std::collections::HashMap<String, usize>,
    last_text_fp: u64,
    output_escalated: bool,
    /// L3-компактификация признана бесполезной (QA-STRESS-01): при лимите меньше
    /// базового контекста она не опускает est ниже порога и зацикливается по
    /// API-вызову на ход — после первой неэффективной L3 пропускаем её.
    /// Флаг сессионный (как doom_warned): условие «лимит < базового контекста»
    /// между вопросами не меняется.
    l3_futile: bool,
    compact_mask_pct: usize,
    compact_prune_pct: usize,
    compact_summary_pct: usize,
    /// История сессии TUI между вопросами (v0.5.7): run() продолжает её, а не
    /// начинает с чистого листа — иначе агент «не понимал» второй вопрос
    /// в контексте первого (жалоба пользователя «плохо держит контекст»).
    /// Компактификация действует и на неё (пороги как раньше).
    session_history: Vec<Message>,
}

fn est_tokens(messages: &[Message]) -> usize {
    let chars: usize = messages.iter().map(|m| {
        m.content.as_deref().unwrap_or("").len()
            + m.tool_calls.as_ref().map(|v| serde_json::to_string(v).unwrap_or_default().len()).unwrap_or(0)
    }).sum();
    chars / 4 + 1
}

/// Текущее время в секундах от эпохи (для меток сессий).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn agents_md(workspace: &Path) -> Option<String> {
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let p = workspace.join(name);
        if p.exists() {
            let mut s = std::fs::read_to_string(&p).ok()?;
            if s.len() > 32 * 1024 { s.truncate(32 * 1024); }
            return Some(s);
        }
    }
    None
}

/// Дата UTC в формате YYYY-MM-DD из секунд эпохи — без внешних крейтов
/// (civil-from-days алгоритм Говарда Хиннанта, как в memory.rs принята строковая дата).
fn utc_date(ts: u64) -> String {
    let days = (ts / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Системный промпт через PromptBuilder (crate::prompts): base — прежний SYSTEM_PROMPT
/// текстом без изменений, далее Environment, слои AGENTS.md (как раньше — только
/// workspace AGENTS.md/CLAUDE.md), дайджест скиллов и цель (при goal-режиме).
fn build_system_prompt(workspace: &Path, skills: &[SkillSpec], goal: Option<String>, date: &str) -> String {
    let mut env = EnvContext::detect(date);
    // cwd процесса может отличаться от workspace агента — workspace важнее
    env.cwd = workspace.display().to_string();
    let md = agents_md(workspace).unwrap_or_default();
    let digests: Vec<SkillDigest> = skills.iter()
        .map(|s| SkillDigest::new(s.name.as_str(), s.description.as_str()))
        .collect();
    PromptBuilder::new()
        .base(SYSTEM_PROMPT)
        .env(env)
        .agents_md("", md)
        // лимит как у прежней посимвольной обрезки agents_md (32 КиБ)
        .agents_md_limit(32 * 1024)
        .skills(&digests)
        .goal(goal)
        .build()
}

/// Движок hooks_ext из конфигурационных хуков (обратная совместимость формата):
/// matcher "*" → без фильтра по инструменту; конкретное имя → якорный regex точного
/// совпадения (старая семантика сравнения строк). События вне перечня hooks_ext
/// (UserPromptSubmit) пропускаются — их обслуживает старый hooks.rs.
fn build_hook_engine(hooks: &[HookConfig]) -> HookEngine {
    let mut specs = Vec::new();
    for h in hooks {
        let Some(event) = ExtHookEvent::from_name(&h.event) else { continue };
        let pattern = (h.matcher != "*").then(|| format!("^{}$", regex::escape(&h.matcher)));
        // экранированный regex заведомо валиден — Err теоретически недостижим
        if let Ok(m) = HookMatcher::new(event, pattern.as_deref(), &h.command,
                                        std::time::Duration::from_secs(h.timeout_secs.max(1))) {
            specs.push(m);
        }
    }
    HookEngine::from_specs(specs)
}

/// Проекция строкового статуса tools::TodoItem в TodoStatus для гейта finish.
/// Семантика прежнего todo_gate сохранена: «закрыта» только done, всё прочее
/// (pending, in_progress, cancelled, мусор) — открытая задача.
fn gate_status(status: &str) -> TodoStatus {
    match status {
        "done" => TodoStatus::Done,
        "in_progress" => TodoStatus::InProgress,
        // cancelled — закрытая (как в todo.rs и Claude Code): пункт «обсудить с
        // пользователем» модель закрывает отменой, и гейт не должен его мараковать
        // (живой кейс 18.07: TodoRejected на finish с отменённым пунктом)
        "cancelled" | "canceled" => TodoStatus::Cancelled,
        _ => TodoStatus::Pending,
    }
}

/// Гейты консолидации памяти (образец — Claude Code extractMemories):
/// консолидировать только если в сессии была реальная работа (tool-вызовы)
/// и агент сам не писал в память через memory_write (взаимное исключение).
fn should_consolidate(messages: &[Message]) -> bool {
    let mut has_tool_work = false;
    let mut wrote_memory = false;
    for m in messages {
        if m.role == "tool" {
            has_tool_work = true;
        }
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                if c.function.name == "memory_write" {
                    wrote_memory = true;
                }
            }
        }
    }
    has_tool_work && !wrote_memory
}

fn is_readonly_tool(name: &str) -> bool {
    matches!(name, "read_file" | "list_files" | "grep")
}

/// Fingerprint вызова (tool, args) — детектор doom loop (OpenDev шаг 13).
/// NB: DefaultHasher нестабилен между запусками процесса — это допустимо, т.к.
/// fingerprint используется только внутри одной сессии (детекторы повторов)
/// и никуда не персистируется. Для персистентных хэшей — compact_v2::simhash64.
fn fingerprint(name: &str, args: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    serde_json::to_string(args).unwrap_or_default().hash(&mut h);
    h.finish()
}

impl Agent {
    pub fn new(cfg: Config, perms: PermissionEngine, workspace: &Path,
               max_turns: usize, events: Option<Sender<AgentEvent>>) -> Result<Self> {
        let api = ApiClient::new(
            cfg.base_url.as_deref().unwrap(),
            cfg.api_key()?,
            &cfg.model,
            cfg.api_timeout_secs,
            cfg.extra_body.clone(),
            cfg.max_output_tokens,
        )?;
        let transcript_dir = workspace.join(".theseus");
        std::fs::create_dir_all(&transcript_dir).ok();
        let session_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?.as_secs();
        // rollout-трейсинг: JSONL-поток в .theseus/trace-<ts>.jsonl;
        // при ошибке открытия файла — реестр в памяти (трейсинг не роняет сессию)
        let trace = TraceRegistry::with_jsonl(transcript_dir.join(format!("trace-{session_ts}.jsonl")))
            .unwrap_or_else(|_| TraceRegistry::new());
        // новый движок хуков поверх того же конфига (старый hooks.rs не трогаем)
        let hooks_ext = build_hook_engine(&cfg.hooks);
        // скиллы (v0.3): конфиг + дефолтные каталоги; "~" раскрываем в $HOME
        let mut skill_dirs: Vec<PathBuf> = cfg.skill_dirs.iter()
            .map(|s| {
                if let Some(rest) = s.strip_prefix("~/") {
                    std::env::var("HOME").ok()
                        .map(|h| PathBuf::from(h).join(rest))
                        .unwrap_or_else(|| PathBuf::from(s))
                } else {
                    PathBuf::from(s)
                }
            })
            .collect();
        skill_dirs.push(workspace.join(".theseus/skills"));
        if let Some(home) = std::env::var("HOME").ok().map(PathBuf::from) {
            skill_dirs.push(home.join(".theseus/skills"));
        }
        let skill_list = skills::discover(&skill_dirs);
        // память (v0.3)
        let memory = std::env::var("HOME").ok().map(PathBuf::from)
            .map(|h| Memory::open(&h.join(".theseus")));
        let mut tool_env = ToolEnv::new(workspace);
        tool_env.sandbox = cfg.sandbox;
        Ok(Agent {
            api,
            perms,
            env: tool_env,
            workspace: workspace.to_path_buf(),
            model: cfg.model.clone(),
            context_limit: cfg.context_limit_tokens,
            transcript_dir,
            session_ts,
            max_turns,
            events,
            perm_answerer: None,
            mcp: None,
            sub: SubConfig {
                base_url: cfg.base_url.clone().unwrap(),
                api_key: cfg.api_key().unwrap_or_default().to_string(),
                model: cfg.model.clone(),
                timeout_secs: cfg.api_timeout_secs,
                extra_body: cfg.extra_body.clone(),
                max_output_tokens: cfg.max_output_tokens,
            },
            last_prompt: 0,
            todo_rejections: 0,
            controls: Controls::default(),
            goal: None,

            hooks_ext,
            trace,
            skills: skill_list,
            memory,
            bg: BgRegistry::new(),
            web_domains: cfg.web_allowed_domains.clone(),
            fp_window: std::collections::VecDeque::new(),
            doom_warned: std::collections::HashSet::new(),
            last_deny_fp: None,
            spiral_reads: 0,
            reminder_fires: std::collections::HashMap::new(),
            last_text_fp: 0,
            output_escalated: false,
            l3_futile: false,
            compact_mask_pct: cfg.compact_mask_pct,
            compact_prune_pct: cfg.compact_prune_pct,
            compact_summary_pct: cfg.compact_summary_pct,
            session_history: Vec::new(),
        })
    }

    fn emit(&self, ev: AgentEvent) {
        if let Some(tx) = &self.events { let _ = tx.send(ev.clone()); }
        // транскрипт: события в JSONL (для внешнего аудита)
        let f = self.transcript_dir.join(format!("events-{ts}.jsonl", ts = self.session_ts));
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(f) {
            let _ = writeln!(f, "{}", serde_json::json!({"ts": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
                "event": format!("{:?}", ev)}));
        }
    }

    fn emit_accounting(&self) {
        let a = &self.api.accounting;
        self.emit(AgentEvent::Accounting {
            calls: a.calls, prompt_t: a.prompt_tokens, completion_t: a.completion_tokens,
        });
    }

    /// Снимок сессии (v0.3, урок Codex rollout): полные сообщения для --resume
    fn save_session(&self, messages: &[Message]) {
        let f = self.transcript_dir.join(format!("session-{ts}.json", ts = self.session_ts));
        let doc = serde_json::json!({
            "ts": self.session_ts,
            "workspace": self.workspace,
            "messages": messages,
        });
        let _ = std::fs::write(f, serde_json::to_string_pretty(&doc).unwrap_or_default());
    }

    /// Загрузка сессии для resume
    pub fn load_session(path: &Path) -> Result<Vec<Message>> {
        let text = std::fs::read_to_string(path)?;
        let doc: serde_json::Value = serde_json::from_str(&text)?;
        let msgs: Vec<Message> = serde_json::from_value(doc["messages"].clone())?;
        Ok(msgs)
    }


    /// Запуск хуков нового движка (hooks_ext) с заметками о сбоях в событиях TUI.
    /// Исходы не блокируют: блокировка определена только для PreToolUse, который
    /// продолжает обслуживать старый hooks.rs (обратная совместимость).
    pub(crate) fn fire_ext(&self, ev: ExtHookEvent, payload: serde_json::Value) -> Vec<crate::hooks_ext::HookOutcome> {
        let outcomes = self.hooks_ext.fire(ev, &payload.to_string());
        for o in &outcomes {
            let err = o.stderr.trim();
            // как и в старом hooks.rs: молчим только при чистом успехе
            if o.is_ok() && err.is_empty() {
                continue;
            }
            let mut note = format!("[hook {}]", o.command);
            if !o.is_ok() {
                note = format!("{note} exit {}", o.exit_code);
            }
            if !err.is_empty() {
                note = format!("{note}: {err}");
            }
            self.emit(AgentEvent::HookNote(note));
        }
        outcomes
    }

    /// TodoGate: finish при незакрытых todo → отказ с напоминанием (урок Grok).
    /// Реализация — через crate::todo::TodoList::gate_check: GateReject → отказ
    /// с тем же текстом, что и прежде; после двух отказов — «falling through».
    fn todo_gate(&mut self) -> Option<String> {
        let mut list = TodoList::new();
        let items: Vec<GateTodoItem> = self.env.todos.iter().enumerate()
            .map(|(i, t)| GateTodoItem::new(&format!("t{}", i + 1), &t.content, gate_status(&t.status)))
            .collect();
        // id сгенерированы уникальными и непустыми — валидация пройти обязана
        let _ = list.set_full(items);
        match list.gate_check("finish") {
            // Allow или мягкое напоминание — finish пропускаем
            Ok(_) => None,
            Err(reject) => {
                self.todo_rejections += 1;
                if self.todo_rejections > 2 {
                    return None; // «falling through» — пропускаем после двух отказов
                }
                let msg = reject.to_string();
                self.emit(AgentEvent::TodoRejected(msg.clone()));
                Some(msg)
            }
        }
    }

    /// autoDream-lite (v0.3): консолидация прочных фактов из сессии в память
    fn consolidate_memory(&mut self, messages: &[Message]) {
        let Some(mem) = &self.memory else { return; };
        if mem.fact_count() > 200 { return; }
        // гейты по образцу Claude Code extractMemories (стоп-хук с пропусками):
        // 1) чистый разговор без инструментов — нечего запоминать (иначе MEMORY.md
        //    забивается фактами «пользователь спросил про GRPO» — живой урок);
        // 2) агент уже писал в память сам (memory_write) — взаимное исключение,
        //    повторная LLM-консолидация избыточна.
        if !should_consolidate(messages) { return; }
        let convo: String = messages.iter()
            .filter_map(|m| m.content.as_deref())
            .collect::<Vec<_>>().join("\n");
        // порог в символах, не в байтах: кириллица даёт 2+ байта на символ (ревью 2.3/4)
        if convo.chars().count() < 800 { return; }
        let prompt = vec![
            Message::system("Extract up to 5 durable facts about the USER or the PROJECT from this \
                session that are worth remembering across sessions (preferences, environment facts, \
                project decisions). One line each, no numbering, Russian ok. If nothing durable, reply EMPTY."),
            Message::user(convo.chars().take(6000).collect::<String>()),
        ];
        if let Ok(resp) = self.api.chat(&prompt, &serde_json::Value::Null) {
            if let Some(text) = resp.content {
                if !text.contains("EMPTY") {
                    let mut n = 0;
                    for line in text.lines() {
                        let fact = line.trim().trim_start_matches(['-', '*', ' ']).trim();
                        if fact.len() > 10 {
                            mem.write_fact(fact);
                            n += 1;
                        }
                    }
                    if n > 0 {
                        self.emit(AgentEvent::MemoryConsolidated(n));
                    }
                }
            }
        }
    }

    /// Параллельное исполнение read-only вызовов (v0.3, урок всех трёх + локи на запись)
    fn parallel_readonly(&mut self, calls: &[&ToolCall]) -> Vec<(String, String)> {
        let workspace = self.workspace.clone();
        // doom-loop guard и для параллельного батча (QA-TH-AGENT-001): раньше
        // read-only вызовы исполнялись мимо execute() и его fingerprint-детектора,
        // поэтому батч идентичных read_file обходил защиту от закольцовки.
        // Каждый вызов прогоняется через тот же механизм (окно 20, ≥3 идентичных,
        // общие fp_window/doom_warned с execute()): достигшие порога не исполняются,
        // а получают текст предупреждения — контракт tool messages сохраняется
        // (каждый id получает ровно один ответ).
        let mut verdicts: Vec<Option<String>> = Vec::with_capacity(calls.len());
        for c in calls {
            let name = &c.function.name;
            let args: serde_json::Value = serde_json::from_str(&c.function.arguments)
                .unwrap_or(serde_json::json!({}));
            let fp = fingerprint(name, &args);
            let verdict = if self.doom_warned.contains(&fp) {
                Some("DENIED (doom-loop guard): идентичный вызов уже пропускался после предупреждения — измените подход".to_string())
            } else {
                self.fp_window.push_back(fp);
                if self.fp_window.len() > 20 { self.fp_window.pop_front(); }
                let count = self.fp_window.iter().filter(|x| **x == fp).count();
                if count >= 3 {
                    self.doom_warned.insert(fp);
                    self.emit(AgentEvent::HookNote(format!(
                        "⚠ doom-loop: «{name}» ×{count} с одинаковыми аргументами (окно 20)")));
                    Some(format!("[SYSTEM WARNING: doom loop suspected — «{name}» с теми же аргументами уже встречался {count} раз в окне 20. Вызов пропущен. Измените стратегию.]"))
                } else {
                    None
                }
            };
            verdicts.push(verdict);
        }
        std::thread::scope(|s| {
            // сначала спавним ВСЕ потоки, потом джойним — иначе параллелизм теряется
            let mut handles = Vec::with_capacity(calls.len());
            for (c, verdict) in calls.iter().zip(&verdicts) {
                // doom-вердикт — исполнять нечего, поток не спавним
                if verdict.is_some() {
                    handles.push(None);
                    continue;
                }
                let name = c.function.name.clone();
                let id = c.id.clone();
                let args: serde_json::Value = serde_json::from_str(&c.function.arguments)
                    .unwrap_or(serde_json::json!({}));
                let ws = workspace.clone();
                handles.push(Some(s.spawn(move || {
                    let out = match name.as_str() {
                        "read_file" => tools::read_file_free(&ws, &args),
                        "list_files" => tools::list_files_free(&ws, &args),
                        "grep" => tools::grep_free(&ws, &args),
                        other => format!("ERROR: не read-only: {other}"),
                    };
                    (id, out)
                })));
            }
            handles.into_iter().zip(calls.iter()).zip(&verdicts).map(|((h, c), verdict)| {
                // паника в read-only потоке не должна валить агента (урок бага cap():
                // join().unwrap() пробрасывал панику и убивал весь ход).
                // id берём снаружи: при панике tool_call всё равно получает ответ,
                // иначе DeepSeek вернёт 400 «insufficient tool messages».
                match h {
                    Some(h) => match h.join() {
                        Ok(pair) => pair,
                        Err(_) => (c.id.clone(), "ERROR: паника в параллельном read-only исполнении".to_string()),
                    },
                    // doom-вердикт: текст предупреждения вместо исполнения
                    None => (c.id.clone(), verdict.clone().unwrap_or_default()),
                }
            }).collect()
        })
    }

    /// Полный сброс на новую сессию (/new, /clear из TUI): история, цель,
    /// пер-сессионные детекторы и файлы транскрипта. Вызывается лениво из
    /// run() по флагу controls.reset_session.
    fn reset_session_state(&mut self) {
        self.session_history.clear();
        self.goal = None;
        self.env.finished = None;
        self.env.clear_todos();
        // новая сессия — новая метка: events/session/trace пишутся в файлы
        // с новым ts; при совпадении секунды сдвигаем на 1, чтобы имя файла
        // гарантированно сменилось (иначе сессия «склеится» с прежней)
        let ts = now_secs();
        self.session_ts = if ts == self.session_ts { ts + 1 } else { ts };
        // трейс-реестр привязан к файлу при создании агента — перепривязываем,
        // иначе спаны новой сессии уходили бы в trace-<старый ts>.jsonl
        self.trace = TraceRegistry::with_jsonl(
            self.transcript_dir.join(format!("trace-{}.jsonl", self.session_ts)))
            .unwrap_or_else(|_| TraceRegistry::new());
        // пер-сессионные детекторы и счётчики — тоже с чистого листа
        self.fp_window.clear();
        self.doom_warned.clear();
        self.last_deny_fp = None;
        self.spiral_reads = 0;
        self.reminder_fires.clear();
        self.last_text_fp = 0;
        self.output_escalated = false;
        self.last_prompt = 0;
        self.todo_rejections = 0;
        self.l3_futile = false;
    }

    pub fn run(&mut self, user_prompt: &str) -> Result<String> {
        // запрос новой сессии из TUI (/new, /clear): история и пер-задачное
        // состояние сбрасываются, транскрипт начинается с новой метки времени
        if self.controls.reset_session.swap(false, Ordering::Relaxed) {
            self.reset_session_state();
        }
        // goal-режим: цель попадает в системный промпт секцией ## Goal (recency-эффект);
        // GoalState/hook выставляет run_with при своём разборе префикса
        let goal = user_prompt.strip_prefix("[GOAL] ").map(str::to_string);
        let sys = build_system_prompt(&self.workspace, &self.skills, goal, &utc_date(self.session_ts));
        // история сессии (v0.5.7): продолжаем прошлый разговор, системный промпт
        // обновляем на месте (env/дата/goal могли измениться)
        if self.session_history.is_empty() {
            self.session_history.push(Message::system(sys));
        } else {
            self.session_history[0] = Message::system(sys);
        }
        let mut messages = std::mem::take(&mut self.session_history);
        let out = self.run_with(&mut messages, user_prompt);
        self.session_history = messages;
        out
    }

    /// resume (v0.3): продолжить из снимка сессии; снимок становится новой историей сессии
    pub fn run_resume(&mut self, mut messages: Vec<Message>, user_prompt: &str) -> Result<String> {
        if messages.is_empty() || messages[0].role != "system" {
            messages.insert(0, Message::system(
                build_system_prompt(&self.workspace, &self.skills, None, &utc_date(self.session_ts))));
        }
        let out = self.run_with(&mut messages, user_prompt);
        // новый ход — обратно в историю сессии (баг: раньше resume терял его)
        self.session_history = messages;
        out
    }

    /// Сборка сообщений прерванного преемпцией хода: частичный ответ ассистента
    /// и заглушка tool на каждый несостоявшийся вызов. Контракт DeepSeek: у
    /// каждого tool_call в истории обязан быть ответ tool, иначе следующий ход
    /// падает с 400 «insufficient tool messages» — преемпция ломала сессию.
    /// Стрим мог оборваться в фазе thinking (ни контента, ни вызовов) — тогда
    /// assistant-реплику НЕ добавляем вовсе: DeepSeek требует «content or
    /// tool_calls must be set» (живой тест 19.07 поймал этот 400 на ходе 2).
    fn preempted_turn_messages(resp: &ChatResponse) -> Vec<Message> {
        let content = resp.content.clone().filter(|c| !c.is_empty());
        if content.is_none() && resp.tool_calls.is_empty() {
            return Vec::new();
        }
        let mut out = vec![Message::assistant(content,
            if resp.tool_calls.is_empty() { None } else { Some(resp.tool_calls.clone()) })];
        for c in &resp.tool_calls {
            out.push(Message::tool(c.id.clone(),
                "[interrupted: preempted by user before execution]"));
        }
        out
    }

    /// Слить пользовательские вставки из очереди в историю (библиотека: push-back —
    /// норма). Immediate (Ctrl+S) — «вставка посреди хода», Normal (Enter) — «из
    /// очереди»: обе вливаются как user-реплики, стрим НЕ прерывается на Normal.
    fn drain_prompt_slot(&mut self, messages: &mut Vec<Message>) {
        for d in self.controls.prompt_slot.lock().unwrap().drain() {
            let (tag, hint) = if d.priority == crate::scheduler::Priority::Immediate {
                ("(вставка посреди хода)", "[user interjection mid-turn]")
            } else {
                ("(из очереди)", "[user queued message while you were working]")
            };
            self.emit(AgentEvent::UserMsg(format!("{tag} {}", d.text)));
            messages.push(Message::user(format!("{hint} {}", d.text)));
        }
    }

    /// Основной цикл агента (общий для run и run_resume)
    fn run_with(&mut self, messages: &mut Vec<Message>, user_prompt: &str) -> Result<String> {
        self.todo_rejections = 0;
        // счётчик фоновых задач в разделяемый атомик (индикатор «фон: N» в TUI)
        self.bg.set_counter(self.controls.bg_running.clone());
        // сброс пер-задачного состояния: env.finished и детекторы живут в Agent,
        // который в TUI переиспользуется между вопросами. Без сброса второй вопрос
        // мгновенно «завершался» устаревшим env.finished от первой задачи
        // (баг из живой сессии 1784400830: второй вопрос получил summary первого).
        self.env.finished = None;
        self.spiral_reads = 0;
        self.last_text_fp = 0;
        self.reminder_fires.clear();
        // сброс флага отмены: Esc прервал ПРОШЛУЮ задачу, новая стартует чистой
        // (баг 19.07: после Esc следующая задача мгновенно «прервана пользователем»)
        self.controls.abort.store(false, Ordering::Relaxed);
        let mut user_prompt = user_prompt.to_string();
        // /goal из TUI: "[GOAL] текст"
        if let Some(rest) = user_prompt.strip_prefix("[GOAL] ") {
            self.goal = Some(GoalState { text: rest.to_string(), max_turns: 10, turns_used: 0, audit_sent: false });
            self.emit(AgentEvent::GoalSet(rest.to_string()));
            self.fire_ext(ExtHookEvent::GoalSet, serde_json::json!({"goal": rest}));
            user_prompt = format!("The user set a GOAL for this session: {rest}. Work toward it; it will be audited.");
        }
        // скиллы попали в системный промпт при сборке (build_system_prompt, секция Available skills)
        // контекстные заметки локальных команд (список /skill-search и т.п.) —
        // подмешиваем к промпту, чтобы агент видел то же, что и пользователь
        let notes: Vec<String> = self.controls.notes_slot.lock().unwrap().drain(..).collect();
        if !notes.is_empty() {
            user_prompt = merge_notes_into_prompt(&notes, &user_prompt);
        }
        messages.push(Message::user(user_prompt.clone()));
        self.emit(AgentEvent::UserMsg(user_prompt.clone()));
        // Единый движок hooks_ext (V3 #2.2): SessionStart и UserPromptSubmit —
        // exit 2 хука блокирует промпт (семантика старого hooks.rs)
        self.fire_ext(ExtHookEvent::SessionStart, serde_json::json!({"prompt": user_prompt}));
        let submit = self.fire_ext(ExtHookEvent::UserPromptSubmit, serde_json::json!({"prompt": user_prompt}));
        let reason = crate::hooks_ext::block_reason(&submit);
        if !reason.is_empty() {
            self.emit(AgentEvent::HookNote(format!("⛔ хук заблокировал промпт: {reason}")));
            self.emit(AgentEvent::Error(format!("промпт заблокирован хуком: {reason}")));
            return Ok(format!("промпт заблокирован хуком: {reason}"));
        }

        let mut tools = tool_specs();
        if let Some(mcp) = &self.mcp {
            if let Some(arr) = tools.as_array_mut() {
                for (nsname, _srv, _tool, schema) in &mcp.tools {
                    arr.push(serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": nsname,
                            "description": schema["description"].as_str().unwrap_or("MCP tool"),
                            "parameters": schema.get("inputSchema").cloned()
                                .unwrap_or(serde_json::json!({"type": "object"})),
                        }
                    }));
                }
            }
        }
        let t0 = Instant::now();
        // лимит ходов с продлением для эксперта (v0.6.5, баг пользователя 20.07 —
        // сессия умерла на 40-м ходу посреди разведки): при достижении лимита
        // спрашиваем через тот же perm-попап — продлить на batch до потолка ×4;
        // headless (нет answerer'а) — прежнее поведение: останов с ошибкой
        let initial_max = self.max_turns;
        let ceiling = initial_max.saturating_mul(TURN_LIMIT_CEILING_MULT);
        let mut turn = 0;
        loop {
            turn += 1;
            // корневой спан хода: закрывается на всех путях, включая ошибки
            // (ручной scopeguard: close_span до диспетчеризации flow, Err тоже фиксируется)
            let turn_span = self.trace.open_span("turn", None);
            self.trace.attr(turn_span, "n", &turn.to_string());
            let flow = self.run_turn(messages, &tools, turn, t0, turn_span);
            if let Err(e) = &flow {
                self.trace.attr(turn_span, "error", &format!("{e:#}"));
            }
            self.trace.close_span(turn_span);
            match flow? {
                TurnFlow::Continue => {}
                TurnFlow::Done(out) => return Ok(out),
            }
            if turn >= self.max_turns {
                if self.max_turns >= ceiling {
                    break; // потолок выбран полностью — выход с ошибкой лимита
                }
                let batch = initial_max;
                let question = format!(
                    "⏱ достигнут лимит {} ходов. Продолжить ещё {} (потолок {})?",
                    self.max_turns, batch, ceiling);
                let allow = self.perm_answerer.as_mut().is_some_and(|f| f(&question));
                if !allow {
                    break;
                }
                self.max_turns = (self.max_turns + batch).min(ceiling);
                self.emit(AgentEvent::HookNote(format!(
                    "▶ лимит ходов продлён до {} (потолок {})", self.max_turns, ceiling)));
            }
        }
        self.emit_accounting();
        self.save_session(messages);
        let err = format!("достигнут лимит ходов ({}) на ходе {}", self.max_turns, turn);
        self.emit(AgentEvent::Error(err.clone()));
        Ok(err)
    }

    /// Тело одного хода цикла run_with. Спан хода открыт/закрыт снаружи;
    /// вложенные спаны (api_call, tool_exec, compact) родительствуются к нему.
    fn run_turn(&mut self, messages: &mut Vec<Message>, tools: &serde_json::Value,
                turn: usize, t0: Instant, turn_span: SpanId) -> Result<TurnFlow> {
        // отмена пользователем (v0.3)
        if self.controls.abort.load(Ordering::Relaxed) {
            self.emit(AgentEvent::Error("прервано пользователем (Esc)".into()));
            self.save_session(messages);
            return Ok(TurnFlow::Done("прервано пользователем".into()));
        }
        // /goal из слота TUI
        if let Some(text) = self.controls.goal_slot.lock().unwrap().take() {
            self.goal = Some(GoalState { text: text.clone(), max_turns: 10, turns_used: 0, audit_sent: false });
            self.emit(AgentEvent::GoalSet(text.clone()));
            self.fire_ext(ExtHookEvent::GoalSet, serde_json::json!({"goal": text}));
            messages.push(Message::user(format!("The user set a GOAL: {text}. Work toward it; it will be audited.")));
        }
        // пользовательские вставки посреди хода (библиотека: push-back — норма, SWE-chat 44%)
        self.drain_prompt_slot(messages);
        self.maybe_compact(messages, Some(turn_span))?;
        let est = est_tokens(messages).max(self.last_prompt);
        self.emit(AgentEvent::Status {
            turns: turn,
            est_tokens: est,
            mode: format!("{:?}{}", self.perms.mode(),
                if self.controls.plan.load(Ordering::Relaxed) { "+plan" } else { "" }),
        });
        // schema gating (OpenDev #3): в plan-режиме write-инструменты НЕВИДИМЫ, не заблокированы
        let mut turn_tools = tools.clone();
        if self.controls.plan.load(Ordering::Relaxed) {
            if let Some(arr) = turn_tools.as_array_mut() {
                arr.retain(|t| !matches!(t["function"]["name"].as_str(),
                    Some("write_file") | Some("edit_file") | Some("bash")));
            }
        }
        let events = self.events.clone();
        // преемпция (урок Codex mailbox): стоп стрима при abort (Esc) ИЛИ
        // Immediate-вставке (Ctrl+S) в очереди. Normal (Enter) стрим НЕ рвёт —
        // вливается на границе хода через drain_prompt_slot.
        let controls = self.controls.clone();
        let should_stop = move || {
            controls.abort.load(Ordering::Relaxed)
                || matches!(controls.prompt_slot.lock().unwrap().peek(),
                            Some(p) if p.priority == crate::scheduler::Priority::Immediate)
        };
        // спан API-вызова: модель всегда, токены — при успехе, ошибка — при сбое
        let api_span = self.trace.open_span("api_call", Some(turn_span));
        self.trace.attr(api_span, "model", &self.model);
        let resp = match self.api.chat_stream(messages, &turn_tools, &mut |chunk| {
            if let Some(tx) = &events {
                let _ = tx.send(AgentEvent::AgentTextDelta(chunk.to_string()));
            }
        }, &should_stop) {
            Ok(r) => {
                self.trace.attr(api_span, "tokens",
                    &format!("{}+{}", r.prompt_tokens, r.completion_tokens));
                self.trace.close_span(api_span);
                r
            }
            Err(e) => {
                self.trace.attr(api_span, "error", &format!("{e:#}"));
                self.trace.close_span(api_span);
                // on-error триггер (урок Grok): context-length ошибка → L3 компактификация и resubmit
                let etext = format!("{e:#}").to_lowercase();
                if etext.contains("context") || etext.contains("length")
                    || etext.contains("token") || etext.contains("too long") {
                    self.emit(AgentEvent::HookNote(format!(
                        "⤓ on-error триггер: L3 компактификация и повтор запроса ({e:#})")));
                    // Pre/PostCompact — через hooks_ext (этот путь идёт мимо maybe_compact)
                    self.fire_ext(ExtHookEvent::PreCompact,
                        serde_json::json!({"trigger": "on-error", "level": "L3"}));
                    let compacted = self.llm_compact(messages, Some(turn_span));
                    self.fire_ext(ExtHookEvent::PostCompact,
                        serde_json::json!({"trigger": "on-error", "level": "L3", "ok": compacted.is_ok()}));
                    compacted?;
                    return Ok(TurnFlow::Continue);
                }
                return Err(e);
            }
        };
        self.last_prompt = resp.prompt_tokens as usize;
        // одноразовая эскалация max_output (урок Claude: 8k→64k один раз, без мета-сообщений)
        if resp.finish_reason.as_deref() == Some("length") && !self.output_escalated {
            self.output_escalated = true;
            let new_max = self.api.max_output() * 2;
            self.api.set_max_output(new_max);
            self.emit(AgentEvent::HookNote(format!(
                "⇑ max_output ×2 → {new_max} (finish_reason=length, повтор запроса)")));
            return Ok(TurnFlow::Continue);
        }
        if resp.aborted {
            // досрочный разрыв: фиксируем частичный ответ и вливаем вставки следующим ходом
            let why = if self.controls.abort.load(Ordering::Relaxed) {
                "⏸ стрим прерван пользователем (Esc)"
            } else {
                "⏸ стрим прерван преемпцией (вставка Ctrl+S)"
            };
            self.emit(AgentEvent::HookNote(why.into()));
            // контракт tool messages: у каждого tool_call обязан быть ответ tool,
            // иначе DeepSeek вернёт 400 «insufficient tool messages» на следующем
            // ходе — прерванные вызовы закрываем заглушками (баг пользователя)
            messages.extend(Self::preempted_turn_messages(&resp));
            self.drain_prompt_slot(messages);
            return Ok(TurnFlow::Continue);
        }
        if resp.reasoning_len > 0 {
            self.emit(AgentEvent::Reasoning(resp.reasoning_len));
        }
        if let Some(text) = &resp.content {
            self.emit(AgentEvent::AgentText(text.clone()));
            // doom-text (усиление детекторов): повторный идентичный текст модели подряд
            if text.len() > 100 {
                let fp = fingerprint("assistant_text", &serde_json::json!(text));
                if fp == self.last_text_fp {
                    let fires = self.reminder_fires.entry("doom_text".into()).or_insert(0);
                    if *fires < 1 {
                        *fires += 1;
                        messages.push(Message::user(
                            "REMINDER: ваш предыдущий ответ идентичен текущему — вы повторяетесь без прогресса. Измените подход или завершите задачу."));
                        self.emit(AgentEvent::HookNote("⚠ doom-text: идентичный текст модели два хода подряд".into()));
                    }
                }
                self.last_text_fp = fp;
            }
        }
        messages.push(Message::assistant(resp.content.clone(),
            if resp.tool_calls.is_empty() { None } else { Some(resp.tool_calls.clone()) }));

        if resp.tool_calls.is_empty() {
            messages.push(Message::user(
                "If the user's request is already fully answered (a greeting or a direct answer counts as complete), \
                 call finish(summary) now. Only continue with tool calls if real work remains."));
            return Ok(TurnFlow::Continue);
        }

        // v0.3: разделение на параллельные read-only и последовательные
        let (ro_calls, serial_calls): (Vec<&ToolCall>, Vec<&ToolCall>) = resp.tool_calls.iter()
            .partition(|c| is_readonly_tool(&c.function.name));
        let mut ro_results: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        if !ro_calls.is_empty() {
            let allowed: Vec<&ToolCall> = ro_calls.iter().copied()
                .filter(|c| matches!(self.decide(&c.function.name,
                    &serde_json::from_str(&c.function.arguments).unwrap_or(serde_json::json!({}))), Decision::Allow))
                .collect();
            for (id, out) in self.parallel_readonly(&allowed) {
                ro_results.insert(id, out);
            }
        }

        let mut finished = None;
        // Напоминания, которые надо вставить ПОСЛЕ всех tool-ответов хода:
        // user-сообщение между assistant(tool_calls) и tool-ответами ломает
        // контракт DeepSeek (400 «insufficient tool messages») — баг стресса.
        let mut deferred_notes: Vec<String> = Vec::new();
        for call in &resp.tool_calls {
            // спан исполнения инструмента: имя всегда, решение — по итогу пути
            let tool_span = self.trace.open_span("tool_exec", Some(turn_span));
            self.trace.attr(tool_span, "name", &call.function.name);
            // exploration spiral (OpenDev #6): 5+ подряд read-only → напоминание
            if is_readonly_tool(&call.function.name) {
                self.spiral_reads += 1;
                if self.spiral_reads == 5 {
                    let fires = self.reminder_fires.entry("spiral".into()).or_insert(0);
                    if *fires < 2 {
                        *fires += 1;
                        deferred_notes.push(
                            "REMINDER: много чтений подряд без продвижения — переходите к действию (анализ/план/правки) или к finish.".into());
                        self.emit(AgentEvent::HookNote("⚠ exploration spiral: 5 read-only подряд — напоминание".into()));
                    }
                }
            } else {
                self.spiral_reads = 0;
            }
            if call.function.name == "finish" {
                if let Some(reject) = self.todo_gate() {
                    self.trace.attr(tool_span, "decision", "TodoGate");
                    self.trace.close_span(tool_span);
                    messages.push(Message::tool(&call.id, reject));
                    continue;
                }
            }
            let out = if let Some(pre) = ro_results.remove(&call.id) {
                self.trace.attr(tool_span, "decision", "Allow (parallel)");
                self.emit(AgentEvent::ToolCall { name: call.function.name.clone(), args: call.function.arguments.clone(), decision: "Allow (parallel)".into() });
                let ok = !pre.starts_with("ERROR");
                self.emit(AgentEvent::ToolResult { name: call.function.name.clone(), preview: pre.chars().take(200).collect(), ok });
                pre
            } else {
                let out = self.execute(call);
                // точное решение остаётся внутри execute (событие ToolCall);
                // для спана выводим его по префиксу результата
                let dec = if out.starts_with("DENIED") { "Deny" }
                    else if out.starts_with("BLOCKED") { "Blocked (hook)" }
                    else { "Allow" };
                self.trace.attr(tool_span, "decision", dec);
                out
            };
            if call.function.name == "finish" {
                finished = Some(out.clone());
            }
            messages.push(Message::tool(&call.id, out));
            self.trace.close_span(tool_span);
        }
        // отложенные напоминания — строго после всех tool-ответов хода
        for note in deferred_notes {
            messages.push(Message::user(note));
        }
        let _ = serial_calls;
        if let Some(summary) = finished.or_else(|| self.env.finished.clone()) {
            // goal-аудит (v0.3): первый finish при активной цели → continuation
            let mut audit_text: Option<String> = None;
            if let Some(g) = self.goal.as_mut() {
                g.turns_used += 1;
                if !g.audit_sent && g.turns_used <= g.max_turns {
                    g.audit_sent = true;
                    audit_text = Some(g.text.clone());
                } else {
                    self.goal = None;
                }
            }
            if let Some(text) = audit_text {
                self.emit(AgentEvent::GoalSet(format!("аудит цели: {text}")));
                messages.push(Message::user(format!(
                    "GOAL AUDIT: the goal was: «{text}». If it is FULLY achieved with proof (files, outputs, numbers), \
                     call finish again with the proof. Otherwise keep working toward it.")));
                return Ok(TurnFlow::Continue);
            }
            self.emit(AgentEvent::Finished(summary.clone()));
            self.consolidate_memory(messages);
            self.fire_ext(ExtHookEvent::SessionEnd, serde_json::json!({"summary": summary}));
            self.emit_accounting();
            self.save_session(messages);
            return Ok(TurnFlow::Done(format!("{summary}\n(ходов: {turn}, время: {:.0}s, API: {} вызовов, токены: {}+{})",
                t0.elapsed().as_secs(),
                self.api.accounting.calls,
                self.api.accounting.prompt_tokens,
                self.api.accounting.completion_tokens)));
        }
        Ok(TurnFlow::Continue)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    /// Заметки слэш-команд идут первым блоком, промпт следом; без замет — as-is.
    #[test]
    fn merge_notes_prepends_context() {
        let notes = vec!["[context: список: 1. a; 2. b]".to_string()];
        let merged = merge_notes_into_prompt(&notes, "загрузи скилл 2");
        assert!(merged.starts_with("[context: список: 1. a; 2. b]"));
        assert!(merged.ends_with("загрузи скилл 2"));
        assert!(merged.len() > notes[0].len() + 1);
        let bare = merge_notes_into_prompt(&[], "просто вопрос");
        assert_eq!(bare, "просто вопрос", "без замет промпт не должен меняться");
    }

    /// Автоподхват скилла, вызванного как инструмент (живой кейс 23.07):
    /// «agent_sessions» → тело скилла agent-sessions вместо unknown tool.
    #[test]
    fn skill_invoked_as_tool_loads_body() {
        let ws = temp_ws("skill_tool");
        let dir = ws.join("agent-sessions");
        std::fs::create_dir_all(&dir).expect("создать каталог скилла");
        std::fs::write(dir.join("SKILL.md"),
            "---\nname: agent-sessions\ndescription: про сессии\n---\n# Тело скилла сессий\n")
            .expect("записать SKILL.md");
        let mut agent = offline_agent(&ws);
        agent.skills = skills::discover(std::slice::from_ref(&ws));
        // подчёркивания нормализуются в дефисы: скилл найден, тело загружено
        let out = agent.skill_invoked_as_tool("agent_sessions").expect("скилл подхвачен");
        assert!(out.contains("а не инструмент"), "{out}");
        assert!(out.contains("agent-sessions"), "{out}");
        assert!(out.contains("Тело скилла сессий"), "{out}");
        // реальный инструмент не тенится даже при совпадении имён
        assert!(agent.skill_invoked_as_tool("read_file").is_none());
        // неизвестное имя не подхватывается
        assert!(agent.skill_invoked_as_tool("no_such_thing").is_none());
    }

    /// Уникальный временный workspace (тесты бегут параллельно).
    fn temp_ws(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, AtomicOrdering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "theseus_agent_test_{}_{}_{seq}_{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("создать временный каталог теста");
        dir
    }

    fn skill_spec(name: &str, desc: &str) -> SkillSpec {
        SkillSpec { name: name.into(), description: desc.into(), path: PathBuf::from("/tmp/SKILL.md") }
    }

    #[test]
    fn utc_date_known_timestamps() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(86_399), "1970-01-01");
        assert_eq!(utc_date(86_400), "1970-01-02");
        // 1e9 секунд эпохи = 2001-09-09 01:46:40 UTC
        assert_eq!(utc_date(1_000_000_000), "2001-09-09");
        // 2023-11-14 22:13:20 UTC
        assert_eq!(utc_date(1_700_000_000), "2023-11-14");
    }

    #[test]
    fn system_prompt_full_sections_and_order() {
        let ws = temp_ws("full");
        std::fs::write(ws.join("AGENTS.md"), "ПРАВИЛА ВОРКСПЕЙСА 123").expect("записать AGENTS.md");
        let skills = [skill_spec("demo-skill", "тестовый скилл")];
        let out = build_system_prompt(&ws, &skills, Some("доделать задачу".into()), "2026-07-18");
        // base — прежний SYSTEM_PROMPT текстом без изменений
        assert!(out.contains("You are Тесей (Theseus) — an autonomous ML-engineering agent inside a TUI harness, created by Роман Некрасов."));
        assert!(out.contains("## Environment"), "секция окружения: {out}");
        assert!(out.contains("- Date: 2026-07-18"));
        assert!(out.contains(&format!("- CWD: {}", ws.display())));
        assert!(out.contains("## AGENTS.md"), "секция AGENTS.md: {out}");
        assert!(out.contains("ПРАВИЛА ВОРКСПЕЙСА 123"));
        assert!(out.contains("## Available skills"), "дайджест скиллов: {out}");
        assert!(out.contains("- demo-skill: тестовый скилл"));
        assert!(out.contains("## Goal"), "секция цели: {out}");
        assert!(out.contains("доделать задачу"));
        // порядок секций фиксирован
        let order = [
            out.find("ML-engineering agent").expect("base"),
            out.find("## Environment").expect("env"),
            out.find("## AGENTS.md").expect("agents"),
            out.find("## Available skills").expect("skills"),
            out.find("## Goal").expect("goal"),
        ];
        assert!(order.windows(2).all(|w| w[0] < w[1]), "порядок секций нарушен: {order:?}");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn system_prompt_minimal_skips_empty_sections() {
        let ws = temp_ws("minimal");
        let out = build_system_prompt(&ws, &[], None, "2026-07-18");
        assert!(out.contains("ML-engineering agent"), "base всегда есть: {out}");
        assert!(out.contains("## Environment"), "окружение есть всегда: {out}");
        assert!(!out.contains("## AGENTS.md"), "нет файла — нет секции: {out}");
        assert!(!out.contains("## Available skills"), "пустой список скиллов — нет секции: {out}");
        assert!(!out.contains("## Goal"), "без цели — нет секции: {out}");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn todo_gate_reject_text_matches_legacy() {
        let mut list = TodoList::new();
        list.set_full(vec![
            GateTodoItem::new("t1", "задача раз", TodoStatus::Done),
            GateTodoItem::new("t2", "задача два", TodoStatus::Pending),
            GateTodoItem::new("t3", "задача три", TodoStatus::InProgress),
        ]).expect("валидные id");
        let Err(reject) = list.gate_check("finish") else {
            panic!("незакрытые задачи обязаны давать GateReject");
        };
        // текст побайтово совпадает с прежним форматом todo_gate из mod.rs
        assert_eq!(
            reject.to_string(),
            "TodoGate: finish отклонён — незакрытые задачи: задача два; задача три. \
             Закройте их или обновите todo_write."
        );
        assert_eq!(reject.pending, ["задача два", "задача три"]);
    }

    #[test]
    fn todo_gate_allows_finish_when_all_closed() {
        let mut list = TodoList::new();
        list.set_full(vec![GateTodoItem::new("t1", "готово", TodoStatus::Done)])
            .expect("валидные id");
        assert!(matches!(list.gate_check("finish"), Ok(crate::todo::GateVerdict::Allow)));
        // пустой список тоже пропускает finish
        assert!(matches!(TodoList::new().gate_check("finish"), Ok(crate::todo::GateVerdict::Allow)));
    }

    #[test]
    fn gate_status_projection_keeps_legacy_semantics() {
        // закрытые: done и cancelled (последнее — с 18.07, как в todo.rs/Claude Code)
        assert_eq!(gate_status("done"), TodoStatus::Done);
        assert!(TodoStatus::Done.is_closed());
        assert_eq!(gate_status("in_progress"), TodoStatus::InProgress);
        assert_eq!(gate_status("pending"), TodoStatus::Pending);
        assert_eq!(gate_status("cancelled"), TodoStatus::Cancelled, "cancelled — закрытая");
        assert!(gate_status("cancelled").is_closed());
        assert_eq!(gate_status("мусор"), TodoStatus::Pending, "незнакомый статус — открытая");
    }

    #[test]
    fn should_consolidate_gates() {
        fn call(name: &str) -> crate::api::ToolCall {
            crate::api::ToolCall {
                id: "c1".into(),
                kind: "function".into(),
                function: crate::api::ToolFunction { name: name.into(), arguments: "{}".into() },
            }
        }
        // чистый разговор без инструментов — не консолидируем (анти-мусор в MEMORY.md)
        let chat = vec![
            Message::system("s"),
            Message::user("привет"),
            Message::assistant(Some("привет!".into()), None),
        ];
        assert!(!should_consolidate(&chat));
        // была реальная работа — консолидируем
        let work = vec![
            Message::system("s"),
            Message::user("прочитай файл"),
            Message::assistant(None, Some(vec![call("read_file")])),
            Message::tool("c1", "содержимое"),
        ];
        assert!(should_consolidate(&work));
        // агент сам писал в память — повторная консолидация избыточна (как у Claude Code)
        let wrote = vec![
            Message::system("s"),
            Message::assistant(None, Some(vec![call("memory_write")])),
            Message::tool("c1", "OK"),
        ];
        assert!(!should_consolidate(&wrote));
    }

    #[test]
    fn hook_engine_built_from_config_compat() {        let cfgs = vec![
            HookConfig { event: "SessionStart".into(), matcher: "*".into(),
                         command: "echo start".into(), timeout_secs: 5 },
            HookConfig { event: "PreCompact".into(), matcher: "*".into(),
                         command: "echo pre".into(), timeout_secs: 0 },
            HookConfig { event: "PreToolUse".into(), matcher: "bash".into(),
                         command: "echo tool".into(), timeout_secs: 5 },
            // единый движок (V3 #2.2): UserPromptSubmit теперь тоже включается
            HookConfig { event: "UserPromptSubmit".into(), matcher: "*".into(),
                         command: "echo submit".into(), timeout_secs: 5 },
        ];
        let engine = build_hook_engine(&cfgs);
        let m = engine.matchers();
        assert_eq!(m.len(), 4, "все четыре события включены в единый движок");
        assert_eq!(m[0].event, ExtHookEvent::SessionStart);
        assert_eq!(m[1].event, ExtHookEvent::PreCompact);
        assert_eq!(m[3].event, ExtHookEvent::UserPromptSubmit);
        assert!(m[0].tool_pattern.is_none(), "* — без фильтра по инструменту");
        // matcher "bash" → точное совпадение, не подстрока
        assert!(m[2].matches_tool(Some("bash")));
        assert!(!m[2].matches_tool(Some("bashful")));
        assert!(!m[2].matches_tool(None));
        // timeout_secs.max(1) — как в старом hooks.rs
        assert_eq!(m[1].timeout, std::time::Duration::from_secs(1));
    }

    #[test]
    fn trace_jsonl_fallback_to_memory_on_bad_path() {
        let bad = std::env::temp_dir().join("theseus_no_such_dir_xyz_123/trace.jsonl");
        assert!(TraceRegistry::with_jsonl(&bad).is_err());
        // тот же фолбэк, что в Agent::new: реестр в памяти, спаны работают
        let mut reg = TraceRegistry::with_jsonl(&bad).unwrap_or_else(|_| TraceRegistry::new());
        let id = reg.open_span("turn", None);
        assert!(reg.attr(id, "n", "1"));
        assert!(reg.close_span(id));
        assert_eq!(reg.span_count(), 1);
        assert!(reg.leaked_ids().is_empty());
    }

    /// Агент для unit-тестов без сети: base_url — заглушка (API не вызывается),
    /// режим Yolo (read-only инструменты разрешены без подтверждений).
    fn offline_agent(ws: &Path) -> Agent {
        let cfg = Config {
            model: "mock-model".into(),
            base_url: Some("http://127.0.0.1:9".into()),
            api_key: Some("test-key".into()),
            context_limit_tokens: 131_072,
            max_output_tokens: 4_096,
            api_timeout_secs: 30,
            extra_body: serde_json::json!({}),
            permission: crate::config::PermissionConfig::default(),
            mcp_servers: vec![],
            permission_rules: vec![],
            hooks: vec![],
            skill_dirs: vec![],
            web_allowed_domains: vec![],
            sandbox: false,
            compact_mask_pct: 70,
            compact_prune_pct: 80,
            compact_summary_pct: 95,
        };
        let perms = PermissionEngine::new(crate::permissions::Mode::Yolo, cfg.permission.clone(), ws);
        Agent::new(cfg, perms, ws, 4, None).expect("агент создаётся")
    }

    fn tool_call(id: &str, name: &str, arguments: &str) -> crate::api::ToolCall {
        crate::api::ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: crate::api::ToolFunction { name: name.into(), arguments: arguments.into() },
        }
    }

    /// QA-TH-AGENT-001: батч идентичных read-only вызовов не должен обходить
    /// doom-loop guard. Раньше parallel_readonly() исполнял вызовы мимо execute()
    /// с его fingerprint-детектором — 5 одинаковых read_file проходили все.
    /// Теперь fingerprint каждого вызова батча идёт через то же окно (20, ≥3):
    /// doom срабатывает на 3-м, а контракт tool messages (ответ на каждый id) цел.
    #[test]
    fn parallel_readonly_doom_guard_stops_identical_batch() {
        let ws = temp_ws("ro_doom");
        std::fs::write(ws.join("a.txt"), "содержимое файла\n").expect("записать файл");
        let mut agent = offline_agent(&ws);
        let owned: Vec<crate::api::ToolCall> = (1..=5)
            .map(|i| tool_call(&format!("c{i}"), "read_file", r#"{"path":"a.txt"}"#))
            .collect();
        let refs: Vec<&crate::api::ToolCall> = owned.iter().collect();
        let out = agent.parallel_readonly(&refs);
        // контракт tool messages: каждый id получил ровно один ответ, порядок сохранён
        assert_eq!(out.len(), 5, "ответ на каждый вызов батча");
        for (i, (id, _)) in out.iter().enumerate() {
            assert_eq!(id, &format!("c{}", i + 1), "порядок ответов соответствует вызовам");
        }
        // первые два исполняются честно
        assert!(out[0].1.contains("содержимое файла"), "1-й: {}", out[0].1);
        assert!(out[1].1.contains("содержимое файла"), "2-й: {}", out[1].1);
        // doom срабатывает на 3-м (окно 20, ≥3 идентичных)
        assert!(out[2].1.contains("doom loop suspected"), "3-й: {}", out[2].1);
        // после предупреждения идентичные вызовы отклоняются
        assert!(out[3].1.starts_with("DENIED (doom-loop guard)"), "4-й: {}", out[3].1);
        assert!(out[4].1.starts_with("DENIED (doom-loop guard)"), "5-й: {}", out[4].1);
        // окно общее с execute(): серийный повтор тех же аргументов сразу DENIED
        let again = tool_call("c6", "read_file", r#"{"path":"a.txt"}"#);
        assert!(agent.execute(&again).starts_with("DENIED (doom-loop guard)"),
            "execute() видит fingerprint из параллельного батча");
        // а вызов с ДРУГИМИ аргументами по-прежнему исполняется
        let other = tool_call("c7", "read_file", r#"{"path":"b.txt"}"#);
        assert!(!agent.execute(&other).starts_with("DENIED"),
            "неидентичный вызов не задет гардом");
        std::fs::remove_dir_all(&ws).ok();
    }

    /// /new и /clear должны создавать НОВУЮ сессию, а не только чистить лог:
    /// новый session_ts, новый trace-файл на диске, чистые история и детекторы.
    /// Раньше трейс-реестр оставался привязан к файлу прежней сессии.
    #[test]
    fn reset_session_state_rotates_transcripts_and_detectors() {
        let ws = temp_ws("reset");
        let mut agent = offline_agent(&ws);
        let old_ts = agent.session_ts;
        // «поработали» в старой сессии: история и детекторы наполнены
        agent.session_history.push(Message::user("привет"));
        agent.fp_window.push_back(42);
        agent.doom_warned.insert(42);
        agent.spiral_reads = 2;
        agent.last_prompt = 7;
        agent.todo_rejections = 3;
        agent.last_text_fp = 9;
        agent.output_escalated = true;
        agent.l3_futile = true;
        agent.env.todos.push(crate::tools::TodoItem {
            content: "задача".into(), status: "pending".into() });

        agent.reset_session_state();

        assert_ne!(agent.session_ts, old_ts, "новая метка времени сессии");
        assert!(agent.session_history.is_empty(), "история очищена");
        assert!(agent.fp_window.is_empty() && agent.doom_warned.is_empty(),
            "fingerprint-детекторы сброшены");
        assert_eq!(agent.spiral_reads, 0);
        assert_eq!(agent.last_prompt, 0);
        assert_eq!(agent.todo_rejections, 0);
        assert_eq!(agent.last_text_fp, 0);
        assert!(!agent.output_escalated && !agent.l3_futile);
        assert!(agent.env.todos.is_empty(), "туду-лист очищен");
        // файлы транскрипта новой сессии: trace-<новый ts>.jsonl создан
        let dir = ws.join(".theseus");
        assert!(dir.join(format!("trace-{}.jsonl", agent.session_ts)).exists(),
            "трейс перепривязан к файлу новой сессии");
        assert!(dir.join(format!("trace-{old_ts}.jsonl")).exists(),
            "старый трейс-файл не удалён (аудит прежней сессии)");
        // события пишутся уже в events-файл новой сессии
        agent.emit(AgentEvent::Status { turns: 1, est_tokens: 10, mode: "test".into() });
        assert!(dir.join(format!("events-{}.jsonl", agent.session_ts)).exists(),
            "events-файл новой сессии создан");
        std::fs::remove_dir_all(&ws).ok();
    }

    /// Два режима вставки посреди хода (запрос пользователя): Enter ставит в
    /// очередь (Normal), Ctrl+S прерывает (Immediate). drain вливает обе как
    /// user-реплики с разными пометками; Immediate обслуживается первой.
    #[test]
    fn drain_prompt_slot_tags_immediate_vs_normal() {
        let ws = temp_ws("drain_modes");
        let mut agent = offline_agent(&ws);
        use crate::scheduler::{Priority, PromptSource, QueuedPrompt};
        // специально пушим Normal первой: Immediate всё равно выйдет раньше
        agent.controls.prompt_slot.lock().unwrap().push(QueuedPrompt::new(
            "дополни после хода", Priority::Normal, PromptSource::User));
        agent.controls.prompt_slot.lock().unwrap().push(QueuedPrompt::new(
            "стоп, срочно", Priority::Immediate, PromptSource::User));

        let mut messages = Vec::new();
        agent.drain_prompt_slot(&mut messages);

        assert_eq!(messages.len(), 2, "обе вставки влиты: {messages:?}");
        assert!(agent.controls.prompt_slot.lock().unwrap().is_empty(), "очередь пуста");
        assert_eq!(messages[0].role, "user");
        assert!(messages[0].content.as_deref().unwrap_or("")
            .contains("[user interjection mid-turn] стоп, срочно"),
            "Immediate первая и с пометкой interjection: {:?}", messages[0].content);
        assert!(messages[1].content.as_deref().unwrap_or("")
            .contains("[user queued message while you were working] дополни после хода"),
            "Normal вторая и с пометкой queued: {:?}", messages[1].content);
        std::fs::remove_dir_all(&ws).ok();
    }

    /// Контракт tool messages при преемпции: на каждый прерванный tool_call в
    /// истории есть ответ tool — иначе DeepSeek 400 «insufficient tool messages».
    #[test]
    fn preempted_turn_messages_keep_tool_contract() {
        let resp = ChatResponse {
            content: Some("частичный ответ".into()),
            tool_calls: vec![
                tool_call("c1", "read_file", r#"{"path":"a"}"#),
                tool_call("c2", "bash", r#"{"command":"ls"}"#),
            ],
            ..Default::default()
        };
        let msgs = Agent::preempted_turn_messages(&resp);
        assert_eq!(msgs.len(), 3, "assistant + 2 tool-заглушки: {msgs:?}");
        assert_eq!(msgs[0].role, "assistant");
        assert_eq!(msgs[0].tool_calls.as_ref().map(Vec::len), Some(2));
        assert_eq!(msgs[1].role, "tool");
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("c2"));
        assert!(msgs[1].content.as_deref().unwrap_or("").contains("interrupted"));
        // обрыв в фазе thinking (ни контента, ни вызовов) — assistant-реплики нет
        // вовсе: DeepSeek отвергает пустое assistant (400 «content or tool_calls
        // must be set», живой тест 19.07)
        assert!(Agent::preempted_turn_messages(&ChatResponse::default()).is_empty());
        let empty_text = ChatResponse { content: Some(String::new()), ..Default::default() };
        assert!(Agent::preempted_turn_messages(&empty_text).is_empty(),
            "пустая строка контента — тоже «ничего»");
    }
}
