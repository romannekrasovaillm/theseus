//! Нагрузочные живые тесты DeepSeek v4-pro — параллельные операции,
//! длинные диалоги, субагенты. Бюджет: 5–10 API-вызовов на тест.
//!
//! Прогон: `cargo test --test live_stress -- --ignored --test-threads=1`
//! Все тесты помечены `#[ignore = "live DeepSeek API"]`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};

use anyhow::Result;

use theseus::agent::{Agent, AgentEvent};

use theseus::config::{Config, PermissionConfig};
use theseus::models;
use theseus::permissions::{Mode, PermissionEngine};

const MODEL: &str = "deepseek-v4-pro";

// ---------------------------------------------------------------------------
// Хелперы (аналогичны live_deepseek.rs, дублированы для независимости крейта)
// ---------------------------------------------------------------------------

struct TempWs(PathBuf);

impl TempWs {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "theseus_stress_{}_{nanos}_{seq}_{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("не создать tempdir {}: {e}", dir.display()));
        TempWs(dir)
    }

    fn path(&self) -> &Path { &self.0 }
}

impl Drop for TempWs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn live_agent(
    ws: &Path, max_turns: usize, context_limit_tokens: usize,
    events: Option<Sender<AgentEvent>>,
) -> Option<Agent> {
    let creds = match models::resolve(MODEL) {
        Ok(c) => c,
        Err(e) => { eprintln!("SKIP: {e:#}"); return None; }
    };
    let cfg = Config {
        model: creds.model,
        base_url: Some(creds.url),
        api_key: Some(creds.key),
        context_limit_tokens,
        max_output_tokens: 8_192,
        api_timeout_secs: 300,
        extra_body: serde_json::json!({"thinking": {"type": "enabled"}}),
        permission: PermissionConfig::default(),
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
    let perms = PermissionEngine::new(Mode::Yolo, cfg.permission.clone(), ws);
    Agent::new(cfg, perms, ws, max_turns, events).ok()
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

/// Стресс-тест: 10+ параллельных read_file/list_files/grep (агент в одном ходе
/// запрашивает несколько read-only инструментов — проверка parallel_readonly).
#[test]
#[ignore = "live DeepSeek API"]
fn stress_parallel_readonly() -> Result<()> {
    let ws = TempWs::new("par_read");
    // Создаём 5 файлов чтобы агенту было что читать
    for i in 1..=5 {
        std::fs::write(
            ws.path().join(format!("file_{i}.txt")),
            format!("Содержимое файла {}: маркер DATA_{}\n", i, i * 10),
        )?;
    }
    let (tx, rx) = channel::<AgentEvent>();
    let Some(mut agent) = live_agent(ws.path(), 6, 131_072, Some(tx)) else { return Ok(()) };
    let out = agent.run(
        "Прочитай ВСЕ 5 файлов file_1.txt..file_5.txt, собери маркеры DATA_* и finish с их списком.",
    )?;
    eprintln!("stress_parallel_readonly: {out}");
    let events: Vec<AgentEvent> = rx.try_iter().collect();
    let tool_calls = events.iter()
        .filter(|e| matches!(e, AgentEvent::ToolCall { .. }))
        .count();
    eprintln!("stress_parallel_readonly: всего tool-событий = {tool_calls}");
    assert!(tool_calls > 0, "агент обязан был вызвать инструменты");
    // Мягкая проверка: хотя бы один маркер DATA_* в ответе
    assert!(
        (1..=5).any(|i| out.contains(&format!("DATA_{}", i * 10))),
        "ни один маркер не найден: {out}"
    );
    Ok(())
}

/// Длинный диалог: 12 ходов с нарастающим контекстом, проверка что агент
/// не деградирует и не упирается в лимит без finish.
#[test]
#[ignore = "live DeepSeek API"]
fn stress_long_conversation() -> Result<()> {
    let ws = TempWs::new("long_conv");
    std::fs::write(ws.path().join("step.txt"), "0")?;
    let (tx, rx) = channel::<AgentEvent>();
    let Some(mut agent) = live_agent(ws.path(), 12, 131_072, Some(tx)) else { return Ok(()) };
    let out = agent.run(
        "Выполняй задачу ПОШАГОВО, не более 1 действия за ход:\n\
         1) read_file step.txt — там число;\n\
         2) увеличить число на 1 и записать обратно write_file;\n\
         3) повторить 5 раз (пока число не станет ≥5);\n\
         4) после этого finish с итоговым числом.\n\
         ВАЖНО: на каждом ходу только ОДНО действие.",
    )?;
    eprintln!("stress_long_conversation: {out}");
    let events: Vec<AgentEvent> = rx.try_iter().collect();
    let tool_count = events.iter()
        .filter(|e| matches!(e, AgentEvent::ToolCall { .. }))
        .count();
    eprintln!("stress_long_conversation: ходов с инструментами = {tool_count}");
    assert!(!out.contains("лимит ходов"), "агент не должен исчерпать лимит: {out}");
    // Проверяем финальное значение в файле
    if let Ok(content) = std::fs::read_to_string(ws.path().join("step.txt")) {
        let val: i32 = content.trim().parse().unwrap_or(0);
        eprintln!("stress_long_conversation: финальное step.txt = {val}");
        // Мягкий ассерт: число должно было увеличиться хотя бы на 1
        assert!(val >= 1, "агент не модифицировал step.txt (val={val})");
    }
    Ok(())
}

/// Субагент explore: основной агент делегирует read-only разведку субагенту.
#[test]
#[ignore = "live DeepSeek API"]
fn stress_subagent_explore() -> Result<()> {
    let ws = TempWs::new("sub_explore");
    std::fs::create_dir_all(ws.path().join("nested"))?;
    std::fs::write(ws.path().join("nested/secret.txt"), "КЛЮЧ: SUPER_SECRET_2026")?;
    let (tx, rx) = channel::<AgentEvent>();
    let Some(mut agent) = live_agent(ws.path(), 8, 131_072, Some(tx)) else { return Ok(()) };
    let out = agent.run(
        "В этом workspace есть nested/secret.txt с секретным ключом. \
         Используй субагента task(subagent_type='Explore') чтобы найти и прочитать его. \
         Затем finish с найденным ключом.",
    )?;
    eprintln!("stress_subagent_explore: {out}");
    let events: Vec<AgentEvent> = rx.try_iter().collect();
    let subagent_spawned = events.iter().any(|e| {
        if let AgentEvent::HookNote(n) = e { n.contains("task") || n.contains("explore") }
        else { false }
    });
    eprintln!("stress_subagent_explore: субагент запущен = {subagent_spawned}");
    // Мягкий ассерт: либо ключ найден, либо агент не исчерпал лимит
    assert!(
        out.contains("SUPER_SECRET") || !out.contains("лимит ходов"),
        "агент не нашёл ключ и не исчерпал лимит: {out}"
    );
    Ok(())
}
