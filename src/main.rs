//! theseus — тонкий бинарник: парсинг аргументов → вызов lib.
//! Вся логика — в `theseus` lib (см. src/lib.rs); парсинг CLI — в `theseus::argparse`.

use anyhow::Result;
use theseus::agent::{Agent, Controls};
use theseus::argparse::{self, Args};
use theseus::config::Config;
use theseus::permissions::{Mode, PermissionEngine};
use theseus::{atty, doctor, mcp, run_headless, run_headless_resume, tui};

const USAGE: &str = "theseus — собственный агентный харнесс (DeepSeek V4-Pro)

ИСПОЛЬЗОВАНИЕ:
  theseus [опции] [задача]          TUI (задача опционально)
  theseus -p \"задача\" [--yolo]      headless-режим для тестов/CI
  theseus doctor [--fix]            диагностика окружения (как у тройки лидеров)

ОПЦИИ:
  -w, --workspace DIR   рабочий каталог (по умолчанию: текущий)
  -p, --prompt TEXT     headless-режим без TUI
      --yolo            авто-разрешение всех действий (кроме hard-deny)
  -m, --model NAME      модель (по умолчанию deepseek-v4-pro)
      --base-url URL    API-эндпоинт (по умолчанию https://api.deepseek.com/v1)
      --context-limit N жёсткий лимит контекста в токенах (перекрывает конфиг)
      --max-turns N     лимит ходов агента (по умолчанию 40)
      --resume FILE     продолжить сессию из файла транскрипта
      --sessions        вывести список сессий каталога .theseus и выйти
      --init            создать пример ~/.config/theseus/config.toml
  -h, --help            эта справка

В TUI: slash-команды (/help — полный список: /goal, /plan, /model, /skills,
  /memory, /sessions, /trace, /compact, /yolo, /quit), ↑/↓ — история ввода
  (~/.theseus/history).

КЛЮЧИ: env DEEPSEEK_API_KEY (обязателен). Транскрипты: <workspace>/.theseus/";

/// Код стартового режима для общего атомика `Controls.mode_atomic`
/// (THS-QA-01: DontAsk обязан мапиться в `MODE_DONTASK` — раньше попадал
/// под плечо `_` → `MODE_ASK`, и headless-режим был недостижим из оверрайда).
fn mode_code(mode: Mode) -> u8 {
    match mode {
        Mode::Ask => theseus::permissions::MODE_ASK,
        Mode::SemiAuto => theseus::permissions::MODE_SEMI,
        Mode::Yolo => theseus::permissions::MODE_YOLO,
        Mode::DontAsk => theseus::permissions::MODE_DONTASK,
    }
}

fn main() -> Result<()> {
    let args: Args = match argparse::parse(&std::env::args().collect::<Vec<_>>()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("ошибка аргументов: {e}");
            std::process::exit(2);
        }
    };
    if args.help {
        println!("{USAGE}");
        return Ok(());
    }
    if args.init {
        let p = theseus::config::write_example_config()?;
        println!("конфиг создан: {}", p.display());
        return Ok(());
    }
    let workspace = args.workspace.canonicalize().unwrap_or(args.workspace);
    if args.sessions {
        let dir = workspace.join(".theseus");
        let mut files: Vec<_> = std::fs::read_dir(&dir).into_iter().flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("session-"))
            .map(|e| e.path())
            .collect();
        files.sort();
        if files.is_empty() {
            println!("сессий нет в {}", dir.display());
        }
        for f in files {
            println!("{}", f.display());
        }
        return Ok(());
    }
    let mut cfg = Config::load(args.base_url.as_deref(), args.model.as_deref())?;
    if let Some(l) = args.context_limit { cfg.context_limit_tokens = l; }

    // doctor — диагностика до запуска агента (ключ API, sandbox, MCP, web, пороги)
    if args.doctor {
        let code = doctor::run(&cfg, &workspace, args.fix)?;
        std::process::exit(code);
    }

    let mode = if args.yolo { Mode::Yolo }
               else if args.prompt.is_some() { Mode::DontAsk }  // headless без yolo — авто-запреты
               else { Mode::Ask };
    // общий атомик режима: /mode в TUI переключает его в рантайме (и посреди хода);
    // стартует с реального режима запуска — индикатор виден сразу
    let controls = Controls::default();
    controls.mode_atomic.store(mode_code(mode), std::sync::atomic::Ordering::Relaxed);
    let perms = PermissionEngine::new(mode, cfg.permission.clone(), &workspace)
        .with_rules(cfg.permission_rules.clone())
        .with_mode_override(controls.mode_atomic.clone());
    let mut agent = Agent::new(cfg.clone(), perms, &workspace, args.max_turns, None)?;
    // MCP stdio/HTTP-серверы из конфига
    if !cfg.mcp_servers.is_empty() {
        let reg = mcp::McpRegistry::connect_all(&cfg.mcp_servers, &mut |msg| eprintln!("[mcp] {msg}"));
        if !reg.is_empty() {
            agent.mcp = Some(reg);
        }
    }

    // resume
    if let Some(path) = &args.resume {
        let messages = Agent::load_session(path)?;
        let prompt = args.prompt.clone()
            .unwrap_or_else(|| "Продолжи с того места, где остановились.".into());
        return run_headless_resume(agent, messages, &prompt);
    }

    let model_info = format!("{} @ {}", cfg.model, cfg.base_url.clone().unwrap_or_default());
    agent.controls = controls.clone();

    // тестовая вставка в prompt_slot (проверка преемпции стрима: Immediate, как Ctrl+S)
    if args.inject_after_sec > 0 && !args.inject_text.is_empty() {
        let slot = controls.prompt_slot.clone();
        let text = args.inject_text.clone();
        let sec = args.inject_after_sec;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(sec));
            slot.lock().unwrap().push(theseus::scheduler::QueuedPrompt::new(
                text, theseus::scheduler::Priority::Immediate,
                theseus::scheduler::PromptSource::User));
        });
    }

    match args.prompt {
        None => {
            // интерактивный TUI без стартовой задачи
            let broker = tui::PermBroker::new();
            tui::run_tui(agent, broker, None, controls, model_info)?;
        }
        Some(p) => {
            if atty::is_terminal() {
                // есть терминал и задача → TUI с первой задачей
                let broker = tui::PermBroker::new();
                tui::run_tui(agent, broker, Some(p), controls, model_info)?;
            } else {
                run_headless(agent, &p)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THS-QA-01: каждый режим мапится в свой код атомика — DontAsk в
    /// MODE_DONTASK, а не в MODE_ASK, как было при плече `_`.
    #[test]
    fn mode_code_maps_all_modes() {
        assert_eq!(mode_code(Mode::Ask), theseus::permissions::MODE_ASK);
        assert_eq!(mode_code(Mode::SemiAuto), theseus::permissions::MODE_SEMI);
        assert_eq!(mode_code(Mode::Yolo), theseus::permissions::MODE_YOLO);
        assert_eq!(mode_code(Mode::DontAsk), theseus::permissions::MODE_DONTASK);
    }

    /// Справка бинарника перечисляет реально поддерживаемые флаги
    /// (регрессия: --resume/--sessions/--context-limit работали, но были
    /// описаны только в расширенной справке argparse).
    #[test]
    fn usage_lists_all_public_flags() {
        for flag in ["--context-limit", "--resume", "--sessions", "--max-turns", "--yolo"] {
            assert!(USAGE.contains(flag), "в USAGE нет флага {flag}");
        }
    }
}
