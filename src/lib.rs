//! theseus: собственный агентный TUI-харнесс.
//!
//! Библиотечная часть: вся логика агента, инструментов, прав, MCP,
//! компактификации и headless-раннеров. Бинарник (`main.rs`) — тонкая
//! обёртка: парсинг аргументов и диспетчер подкоманд.
//! (Паттерн «core as lib, cli as thin bin» — как codex-rs / grok-build.)

pub mod agent;
pub mod agents;
pub mod acp;
pub mod api;
pub mod argparse;
pub mod ariadna;
pub mod audit;
pub mod background;
pub mod compact_v2;
pub mod config;
pub mod config_layers;
pub mod cron;
pub mod diffview;
pub mod digests;
pub mod doctor;
pub mod doctor_ext;
pub mod doctor_fix;
pub mod execpolicy;
pub mod filetype;
pub mod filewatcher;
pub mod gitutil;
pub mod history;
pub mod hooks_ext;
pub mod keymap;
pub mod larkpatch;
pub mod library;
pub mod limits;
pub mod markdown;
pub mod logbook;
pub mod matchers;
pub mod mcp;
pub mod ml_concepts;
pub mod mcp_ext;
pub mod memory;
pub mod memory_v2;
pub mod mock_sse;
pub mod models;
pub mod notify;
pub mod onboarding;
pub mod patch;
pub mod peers;
pub mod permissions;
pub mod prompt_cache;
pub mod prompts;
pub mod retry;
pub mod report;
pub mod safety_scan;
pub mod sandbox;
pub mod sandbox_bwrap;
pub mod scheduler;
pub mod secrets;
pub mod semver;
pub mod session;
pub mod shell;
pub mod skills;
pub mod shell_escape;
pub mod slash;
pub mod subagent;
pub mod telemetry;
pub mod textutil;
pub mod theme;
pub mod todo;
pub mod tools;
pub mod trace;
pub mod tui;
pub mod update_check;
pub mod websearch;
pub mod workspace_map;

use agent::{Agent, AgentEvent};
use anyhow::Result;
use std::sync::mpsc::channel;

/// Маркер обрыва по лимиту ходов в финальном ответе агента — текст тот же,
/// что формирует `agent::Agent::run_with` («достигнут лимит ходов (N) на ходе M»).
const TURN_LIMIT_MARK: &str = "достигнут лимит ходов";

/// Код выхода headless-раннеров при обрыве по лимиту ходов (QA-TH-AGENT-002).
const EXIT_TURN_LIMIT: i32 = 3;

/// Ответ агента сигналит обрыв по лимиту ходов?
fn turn_limit_reached(out: &str) -> bool {
    out.contains(TURN_LIMIT_MARK)
}

/// QA-TH-AGENT-002: headless-прогон, оборвавшийся по лимиту ходов, обязан
/// сигналить ненулевым кодом выхода (для CI), а не «успешным» 0. `Agent::run`
/// сознательно остаётся `Ok` с тем же текстом (его ждёт live-тест
/// max_turns_enforced, вызывающий `agent.run()` напрямую), поэтому код
/// выставляется здесь, на уровне раннера, по маркеру в ответе.
fn exit_if_turn_limit(out: &str) {
    if turn_limit_reached(out) {
        eprintln!("агент оборван по лимиту ходов — код выхода {EXIT_TURN_LIMIT}");
        std::process::exit(EXIT_TURN_LIMIT);
    }
}

/// Headless: события в stdout, ответы на перм-вопросы по режиму.
pub fn run_headless(mut agent: Agent, prompt: &str) -> Result<()> {
    let (tx, rx) = channel::<AgentEvent>();
    agent.events = Some(tx);
    let printer = spawn_printer(rx);
    let result = agent.run(prompt);
    drop(agent); // закрыть tx
    let _ = printer.join();
    let out = result?;
    println!("\n=== {out}");
    exit_if_turn_limit(&out);
    Ok(())
}

/// Headless-продолжение сессии из снимка.
pub fn run_headless_resume(mut agent: Agent, messages: Vec<api::Message>, prompt: &str) -> Result<()> {
    let (tx, rx) = channel::<AgentEvent>();
    agent.events = Some(tx);
    let printer = spawn_printer(rx);
    let result = agent.run_resume(messages, prompt);
    drop(agent);
    let _ = printer.join();
    let out = result?;
    println!("\n=== {out}");
    exit_if_turn_limit(&out);
    Ok(())
}

/// Общий принтер событий.
pub fn spawn_printer(rx: std::sync::mpsc::Receiver<AgentEvent>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok(ev) = rx.recv() {
            print_event(ev);
        }
    })
}

/// Человекочитаемая печать одного события агента (ANSI-цвета).
pub fn print_event(ev: AgentEvent) {
    match ev {
        AgentEvent::UserMsg(t) => println!("\x1b[32m❯ {t}\x1b[0m"),
        AgentEvent::AgentText(t) => println!("{t}"),
        AgentEvent::AgentTextDelta(s) => {
            use std::io::Write;
            // Печатаем дельты без ANSI-кодов — каждый чанк со своими \x1b[...m
            // визуально разрывает многобайтовые UTF-8 символы.
            print!("{s}");
            let _ = std::io::stdout().flush();
        }
        AgentEvent::Reasoning(n) => println!("\x1b[90m(мышление: {n} символов)\x1b[0m"),
        AgentEvent::ToolCall { name, args, decision } => {
            let short: String = args.chars().take(100).collect();
            println!("\x1b[33m⚙ {name}\x1b[90m {short} [{decision}]\x1b[0m");
        }
        AgentEvent::ToolResult { preview, ok, .. } => {
            let short: String = preview.chars().take(120).collect();
            if ok { println!("\x1b[90m  ↳ {short}\x1b[0m"); }
            else { println!("\x1b[31m  ↳ {short}\x1b[0m"); }
        }
        AgentEvent::Status { turns, est_tokens, mode } =>
            eprintln!("[ход {turns} | ~{est_tokens} ток | {mode}]"),
        AgentEvent::Compact { from_msgs, to_msgs } =>
            println!("\x1b[35m⤓ компактификация: {from_msgs} → {to_msgs}\x1b[0m"),
        AgentEvent::TodoRejected(m) => println!("\x1b[31m⛔ {m}\x1b[0m"),
        AgentEvent::Finished(s) => println!("\x1b[36m✔ FINISH: {s}\x1b[0m"),
        AgentEvent::Error(e) => println!("\x1b[31m✖ {e}\x1b[0m"),
        AgentEvent::Accounting { calls, prompt_t, completion_t } =>
            eprintln!("[API: {calls} выз. | токены {prompt_t}+{completion_t}]"),
        AgentEvent::GoalSet(g) => println!("\x1b[35m🎯 GOAL: {g}\x1b[0m"),
        AgentEvent::PlanChanged(on) => println!("\x1b[34m📋 plan mode: {}\x1b[0m", if on { "ON" } else { "OFF" }),
        AgentEvent::MemoryConsolidated(n) => println!("\x1b[90m🧠 память: +{n} фактов\x1b[0m"),
        AgentEvent::HookNote(n) => println!("\x1b[90m🪝 {n}\x1b[0m"),
        AgentEvent::PermAsk { .. } => {}
    }
}

/// Мини-проверка терминала (внешний крейт не тянем).
pub mod atty {
    pub fn is_terminal() -> bool {
        // SAFETY: isatty(1) — POSIX-гарантированная функция; fd=1 (stdout)
        // — валидный файловый дескриптор; возвращает 0/1 без сайд-эффектов.
        unsafe { libc_isatty(1) == 1 }
    }
    /// # Safety
    /// `fd` должен быть валидным открытым файловым дескриптором.
    unsafe fn libc_isatty(fd: i32) -> i32 {
        extern "C" { fn isatty(fd: i32) -> i32; }
        // SAFETY: контракт передан вызывающему; isatty — POSIX, без сайд-эффектов.
        unsafe { isatty(fd) }
    }
}

#[cfg(test)]
mod tests {
    /// QA-TH-AGENT-002: маркер лимита ходов распознаётся в финальном ответе
    /// агента, обычный ответ — нет. Текст маркера побуквенно совпадает с тем,
    /// что формирует `agent::Agent::run_with` («достигнут лимит ходов (N) на ходе M»);
    /// сам `run_with` остаётся `Ok` — его текст ждёт live-тест max_turns_enforced.
    #[test]
    fn turn_limit_mark_detection() {
        assert!(super::turn_limit_reached("достигнут лимит ходов (2) на ходе 2"));
        assert!(super::turn_limit_reached("достигнут лимит ходов (40) на ходе 40"));
        assert!(!super::turn_limit_reached("готово: 42"));
        assert!(!super::turn_limit_reached(""));
    }
}
