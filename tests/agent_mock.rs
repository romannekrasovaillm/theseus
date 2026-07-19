//! Регрессионные тесты агента против mock_sse (без сети, без API-ключа).
//!
//! Главный кейс: ответ модели с НЕСКОЛЬКИМИ параллельными read-only tool_calls —
//! после assistant-сообщения с tool_calls должны идти ТОЛЬКО tool-ответы
//! (по одному на каждый tool_call_id), никаких user-сообщений между ними.
//! Иначе DeepSeek отвечает HTTP 400 «insufficient tool messages following
//! tool_calls message» — баг, пойманный живым стресс-тестом
//! (tests/live_stress.rs::stress_parallel_readonly, 18.07.2026):
//! spiral-напоминание (5+ read-only подряд) вставлялось user-сообщением
//! ПОСЕРЕДИНЕ tool-ответов хода.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;

use theseus::agent::Agent;
use theseus::config::{Config, PermissionConfig};
use theseus::mock_sse::{MockLlm, Scenario};
use theseus::permissions::{Mode, PermissionEngine};

/// Уникальный временный workspace; каталог убирается в Drop.
struct TempWs(PathBuf);

impl TempWs {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "theseus_agent_mock_{}_{nanos}_{seq}_{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempWs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Конфиг агента, направленный на мок-сервер.
fn mock_config(base_url: &str) -> Config {
    Config {
        model: "mock-model".into(),
        base_url: Some(base_url.to_string()),
        api_key: Some("test-key".into()),
        context_limit_tokens: 131_072,
        max_output_tokens: 4_096,
        api_timeout_secs: 30,
        extra_body: serde_json::json!({}),
        permission: PermissionConfig::default(),
        mcp_servers: vec![],
        permission_rules: vec![],
        hooks: vec![],
        skill_dirs: vec![],
        web_allowed_domains: vec![],
        sandbox: true,
        compact_mask_pct: 70,
        compact_prune_pct: 80,
        compact_summary_pct: 95,
    }
}

fn mock_agent(base_url: &str, ws: &Path, max_turns: usize) -> Agent {
    let cfg = mock_config(base_url);
    let perms = PermissionEngine::new(Mode::Yolo, cfg.permission.clone(), ws);
    Agent::new(cfg, perms, ws, max_turns, None).unwrap()
}

/// Проверка контракта DeepSeek: каждое assistant-сообщение с tool_calls
/// должно немедленно продолжаться ровно N tool-сообщениями с теми же
/// tool_call_id (N = числу вызовов), без user/assistant между ними.
fn assert_tool_contract(messages: &[serde_json::Value]) {
    let mut i = 0;
    while i < messages.len() {
        let msg = &messages[i];
        let is_assistant_with_calls = msg["role"] == "assistant"
            && msg["tool_calls"].as_array().is_some_and(|c| !c.is_empty());
        if is_assistant_with_calls {
            let calls = msg["tool_calls"].as_array().unwrap();
            let ids: Vec<&str> = calls
                .iter()
                .map(|c| c["id"].as_str().unwrap_or(""))
                .collect();
            for (k, id) in ids.iter().enumerate() {
                let next = &messages[i + 1 + k];
                assert_eq!(
                    next["role"], "tool",
                    "после assistant(tool_calls) на позиции {i} сообщение #{k} \
                     обязано быть tool, а не {:?};\nсообщения: {messages:?}",
                    next["role"]
                );
                assert_eq!(
                    next["tool_call_id"], *id,
                    "tool-ответ #{k} после позиции {i} должен закрывать id {id:?}"
                );
            }
            i += 1 + ids.len();
        } else {
            i += 1;
        }
    }
}

/// 5 параллельных read_file в одном ответе модели → spiral-напоминание
/// (5 read-only подряд) обязано уйти ПОСЛЕ всех tool-ответов, а не между ними.
#[test]
fn parallel_readonly_keeps_tool_contract() -> Result<()> {
    let ws = TempWs::new("par_contract");
    let mut first = Scenario::new();
    for i in 1..=5 {
        std::fs::write(ws.path().join(format!("f{i}.txt")), format!("данные {i}\n"))?;
        first = first.reply_tool_call("read_file", format!(r#"{{"path":"f{i}.txt"}}"#));
    }
    let handle = MockLlm::with_scenarios(vec![
        first.finish_reason("tool_calls"),
        Scenario::new().reply_tool_call("finish", r#"{"summary":"прочитал все 5 файлов"}"#),
        Scenario::new().reply_text("ок"),
    ])
    .serve_on_ephemeral()?;

    let mut agent = mock_agent(&handle.base_url, ws.path(), 6);
    let out = agent.run("прочитай f1..f5 и заверши")?;
    eprintln!("parallel_readonly_keeps_tool_contract: {out}");

    let requests = handle.requests();
    assert!(requests.len() >= 2, "ожидалось >=2 запросов к моку: {}", requests.len());
    let messages = requests[1]["messages"].as_array().unwrap();
    assert_tool_contract(messages);
    // spiral-напоминание (5 read-only подряд) допустимо, но только ПОСЛЕ
    // всех tool-ответов хода — контракт выше это и гарантирует.
    Ok(())
}

/// Одиночный tool_call — базовый контракт тоже соблюдается.
#[test]
fn single_tool_call_keeps_contract() -> Result<()> {
    let ws = TempWs::new("single_contract");
    std::fs::write(ws.path().join("a.txt"), "x\n")?;
    let handle = MockLlm::with_scenarios(vec![
        Scenario::new().reply_tool_call("read_file", r#"{"path":"a.txt"}"#),
        Scenario::new().reply_tool_call("finish", r#"{"summary":"готово"}"#),
        Scenario::new().reply_text("ок"),
    ])
    .serve_on_ephemeral()?;

    let mut agent = mock_agent(&handle.base_url, ws.path(), 4);
    let _ = agent.run("прочитай a.txt")?;
    let requests = handle.requests();
    assert!(requests.len() >= 2);
    let messages = requests[1]["messages"].as_array().unwrap();
    assert_tool_contract(messages);
    Ok(())
}

/// Смешанный ответ: read-only (параллель) + пишущий инструмент (серийно) —
/// контракт сохраняется на составном tool_calls.
#[test]
fn mixed_readonly_and_write_keeps_contract() -> Result<()> {
    let ws = TempWs::new("mixed_contract");
    std::fs::write(ws.path().join("a.txt"), "x\n")?;
    let handle = MockLlm::with_scenarios(vec![
        Scenario::new()
            .reply_tool_call("read_file", r#"{"path":"a.txt"}"#)
            .reply_tool_call("list_files", r#"{"path":"."}"#)
            .reply_tool_call("write_file", r#"{"path":"b.txt","content":"y"}"#)
            .finish_reason("tool_calls"),
        Scenario::new().reply_tool_call("finish", r#"{"summary":"смешанный ход готов"}"#),
        Scenario::new().reply_text("ок"),
    ])
    .serve_on_ephemeral()?;

    let mut agent = mock_agent(&handle.base_url, ws.path(), 4);
    let _ = agent.run("прочитай a.txt, перечисли файлы и запиши b.txt")?;
    assert_eq!(std::fs::read_to_string(ws.path().join("b.txt"))?, "y");
    let requests = handle.requests();
    assert!(requests.len() >= 2);
    let messages = requests[1]["messages"].as_array().unwrap();
    assert_tool_contract(messages);
    Ok(())
}

/// Регрессия (живая сессия 1784400830, 18.07.2026): второй вопрос в одной
/// TUI-сессии мгновенно «завершался» устаревшим env.finished от первой задачи
/// — агент (переиспользуемый между вопросами) возвращал summary первого ответа.
/// Фикс: сброс пер-задачного состояния в run_with.
#[test]
fn second_question_in_same_session_gets_fresh_answer() -> Result<()> {
    let ws = TempWs::new("second_q");
    let handle = MockLlm::with_scenarios(vec![
        // задача 1: модель сразу финишит с «ПЕРВЫЙ»
        Scenario::new().reply_tool_call("finish", r#"{"summary":"ПЕРВЫЙ ответ"}"#),
        Scenario::new().reply_text("ок"), // consolidate_memory
        // задача 2: текст + финиш с «ВТОРОЙ»
        Scenario::new().reply_text("Промежуточный текст второго ответа."),
        Scenario::new().reply_tool_call("finish", r#"{"summary":"ВТОРОЙ ответ"}"#),
        Scenario::new().reply_text("ок"), // consolidate_memory
    ])
    .serve_on_ephemeral()?;

    let mut agent = mock_agent(&handle.base_url, ws.path(), 4);
    let out1 = agent.run("вопрос первый")?;
    assert!(out1.contains("ПЕРВЫЙ"), "первый ответ: {out1}");

    let out2 = agent.run("вопрос второй")?;
    assert!(out2.contains("ВТОРОЙ"), "второй ответ обязан быть свежим: {out2}");
    assert!(!out2.contains("ПЕРВЫЙ"), "второй ответ не должен повторять первый: {out2}");
    Ok(())
}

/// История сессии (v0.5.7): второй вопрос видит первый Q&A в контексте —
/// run() продолжает историю, а не начинает с чистого листа
/// (жалоба пользователя «плохо держит контекст между сообщениями»).
#[test]
fn session_history_carries_previous_qa_into_next_question() -> Result<()> {
    let ws = TempWs::new("history_carry");
    let handle = MockLlm::with_scenarios(vec![
        Scenario::new().reply_tool_call("finish", r#"{"summary":"ответ про Марс"}"#),
        Scenario::new().reply_text("ок"), // consolidate_memory
        Scenario::new().reply_tool_call("finish", r#"{"summary":"уточнение про Марс"}"#),
        Scenario::new().reply_text("ок"),
    ])
    .serve_on_ephemeral()?;

    let mut agent = mock_agent(&handle.base_url, ws.path(), 4);
    let _ = agent.run("расскажи про Марс")?;
    let _ = agent.run("а теперь короче")?;

    let requests = handle.requests();
    assert!(requests.len() >= 3, "запросов: {}", requests.len());
    // второй запрос (вопрос «а теперь короче») обязан содержать ПЕРВЫЙ вопрос
    let messages = requests[2]["messages"].as_array().unwrap();
    let texts: Vec<&str> = messages.iter()
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert!(texts.iter().any(|c| c.contains("расскажи про Марс")),
        "первый вопрос потерян из истории второго запроса:\n{}",
        serde_json::to_string_pretty(&requests[2]["messages"])?);
    // и финиш первого ответа (assistant) тоже в истории
    assert!(messages.iter().any(|m| m["role"] == "assistant"),
        "ответ ассистента потерян из истории второго запроса");
    // системный промпт — ровно один, первым
    let sys_count = messages.iter().filter(|m| m["role"] == "system").count();
    assert_eq!(sys_count, 1, "системных сообщений: {sys_count}");
    assert_eq!(messages[0]["role"], "system");
    Ok(())
}

/// Регрессия (баг 19.07): после Esc (abort=true) следующая задача запускалась
/// и мгновенно умирала «прервано пользователем». Флаг abort обязан
/// сбрасываться при старте каждой новой задачи.
#[test]
fn abort_flag_does_not_kill_next_task() -> Result<()> {
    let ws = TempWs::new("abort_reset");
    let handle = MockLlm::with_scenarios(vec![
        Scenario::new().reply_tool_call("finish", r#"{"summary":"задача выполнена"}"#),
        Scenario::new().reply_text("ок"),
    ])
    .serve_on_ephemeral()?;

    let mut agent = mock_agent(&handle.base_url, ws.path(), 4);
    // имитируем состояние «после Esc»: флаг abort взведён
    agent.controls.abort.store(true, std::sync::atomic::Ordering::Relaxed);
    let out = agent.run("новая задача после Esc")?;
    assert!(out.contains("задача выполнена"), "новая задача убита флагом abort: {out}");
    assert!(!out.contains("прервано пользователем"), "ложное прерывание: {out}");
    // и флаг сброшен
    assert!(!agent.controls.abort.load(std::sync::atomic::Ordering::Relaxed));
    Ok(())
}

/// /new и /clear: флаг reset_session очищает историю при следующей задаче
/// (запрос пользователя 19.07 — «добавь команды для создания новой сессии»).
#[test]
fn reset_session_clears_history_on_next_run() -> Result<()> {
    let ws = TempWs::new("new_session");
    let handle = MockLlm::with_scenarios(vec![
        Scenario::new().reply_tool_call("finish", r#"{"summary":"первый ответ"}"#),
        Scenario::new().reply_text("ок"),
        Scenario::new().reply_tool_call("finish", r#"{"summary":"второй ответ"}"#),
        Scenario::new().reply_text("ок"),
    ])
    .serve_on_ephemeral()?;

    let mut agent = mock_agent(&handle.base_url, ws.path(), 4);
    let _ = agent.run("вопрос первый")?;
    // пользователь вызвал /new — следующая задача начинает чистую сессию
    agent.controls.reset_session.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = agent.run("второй вопрос")?;

    let requests = handle.requests();
    let second = &requests[2];
    let messages = second["messages"].as_array().unwrap();
    // история второго вопроса НЕ содержит первый — только system + новый user
    let user_texts: Vec<&str> = messages.iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert!(!user_texts.iter().any(|c| c.contains("вопрос первый")),
        "история не очищена: {user_texts:?}");
    assert!(user_texts.iter().any(|c| c.contains("второй вопрос")));
    // флаг сброшен
    assert!(!agent.controls.reset_session.load(std::sync::atomic::Ordering::Relaxed));
    Ok(())
}
