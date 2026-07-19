//! Диагностика здоровья харнесса (урок codex doctor / kimi doctor): config, API, sandbox,
//! MCP, workspace, скиллы, память, правила разрешений. Формат: OK/WARN/FAIL + сводка.

use crate::config::Config;
use crate::sandbox;
use anyhow::Result;
use std::path::Path;

enum Verdict {
    Ok(String),
    Warn(String),
    Fail(String),
}

impl Verdict {
    fn icon(&self) -> &'static str {
        match self {
            Verdict::Ok(_) => "✅",
            Verdict::Warn(_) => "⚠️ ",
            Verdict::Fail(_) => "❌",
        }
    }
    fn text(&self) -> &str {
        match self {
            Verdict::Ok(t) | Verdict::Warn(t) | Verdict::Fail(t) => t,
        }
    }
}

pub fn run(cfg: &Config, workspace: &Path, fix: bool) -> Result<i32> {
    let mut checks: Vec<(String, Verdict)> = vec![];
    let mut fails = 0;

    // 1. API-ключ
    match cfg.api_key() {
        Ok(_) => checks.push(("api_key".into(), Verdict::Ok("задан (env/конфиг)".into()))),
        Err(_) => {
            checks.push(("api_key".into(), Verdict::Fail("нет: задайте DEEPSEEK_API_KEY".into())));
            fails += 1;
        }
    }

    // 2. Доступность API (GET /models — бесплатный запрос)
    match check_api(cfg) {
        Ok(n) => checks.push(("api".into(), Verdict::Ok(format!("{} доступен, моделей: {n}", cfg.base_url.as_deref().unwrap_or("?"))))),
        Err(e) => {
            checks.push(("api".into(), Verdict::Fail(format!("недоступен: {e}"))));
            fails += 1;
        }
    }

    // 3. Ядерный sandbox
    let st = sandbox::status();
    checks.push(("sandbox".into(), match st {
        sandbox::SandboxStatus::Available => Verdict::Ok("landlock доступен и применяется".into()),
        sandbox::SandboxStatus::Unavailable => Verdict::Warn("landlock недоступен — bash без ядерной изоляции".into()),
    }));

    // 4. Workspace
    match check_workspace(workspace, fix) {
        Ok(msg) => checks.push(("workspace".into(), Verdict::Ok(msg))),
        Err(e) => {
            checks.push(("workspace".into(), Verdict::Fail(format!("{e}"))));
            fails += 1;
        }
    }

    // 5. Sandbox-флаг
    if cfg.sandbox {
        checks.push(("sandbox flag".into(), Verdict::Ok("sandbox=true в конфиге".into())));
    } else {
        checks.push(("sandbox flag".into(), Verdict::Warn("sandbox=false — ядерная изоляция выключена".into())));
    }

    // 6. Правила разрешений компилируются
    match check_rules(cfg) {
        Ok(n) => checks.push(("permission rules".into(), Verdict::Ok(format!("deny-паттернов: {n}, все regex валидны")))),
        Err(e) => {
            checks.push(("permission rules".into(), Verdict::Fail(format!("битый regex: {e}"))));
            fails += 1;
        }
    }

    // 7. Web
    if cfg.web_allowed_domains.is_empty() {
        checks.push(("web".into(), Verdict::Warn("web_allowed_domains пуст — web_fetch/web_search выключены".into())));
    } else {
        checks.push(("web".into(), Verdict::Ok(format!("доменов в allow-list: {}", cfg.web_allowed_domains.len()))));
    }

    // 8. MCP-серверы
    if cfg.mcp_servers.is_empty() {
        checks.push(("mcp".into(), Verdict::Warn("MCP-серверы не настроены".into())));
    } else {
        let reg = crate::mcp::McpRegistry::connect_all(&cfg.mcp_servers, &mut |_| {});
        checks.push(("mcp".into(), if reg.is_empty() {
            Verdict::Fail(format!("{} серверов в конфиге, ни один не поднялся", cfg.mcp_servers.len()))
        } else {
            Verdict::Ok(format!("поднято инструментов: {}", reg.tools.len()))
        }));
    }

    // 9. Скиллы
    let skills = crate::skills::discover(&skill_dirs(workspace, cfg));
    checks.push(("skills".into(), Verdict::Ok(format!("обнаружено: {}", skills.len()))));

    // 10. Память
    match std::env::var("HOME").ok().map(std::path::PathBuf::from) {
        Some(h) => {
            let mem = crate::memory::Memory::open(&h.join(".theseus"));
            checks.push(("memory".into(), Verdict::Ok(format!("MEMORY.md, фактов: {}", mem.fact_count()))));
        }
        None => checks.push(("memory".into(), Verdict::Warn("HOME не задан — память недоступна".into()))),
    }

    // 11. Пороги компактификации
    if cfg.compact_mask_pct >= cfg.compact_prune_pct || cfg.compact_prune_pct >= cfg.compact_summary_pct {
        checks.push(("compaction".into(), Verdict::Fail(format!(
            "пороги не по возрастанию: {}% / {}% / {}%",
            cfg.compact_mask_pct, cfg.compact_prune_pct, cfg.compact_summary_pct))));
        fails += 1;
    } else {
        checks.push(("compaction".into(), Verdict::Ok(format!(
            "уровни: {}% маск / {}% прунинг / {}% саммари",
            cfg.compact_mask_pct, cfg.compact_prune_pct, cfg.compact_summary_pct))));
    }

    // Вывод
    println!("theseus doctor\n");
    for (name, v) in &checks {
        println!("  {} {:<18} {}", v.icon(), name, v.text());
    }
    println!();
    if fails == 0 {
        println!("Итог: здоров ({} проверок)", checks.len());
        Ok(0)
    } else {
        println!("Итог: {fails} проблем(ы) из {} проверок", checks.len());
        Ok(1)
    }
}

fn check_api(cfg: &Config) -> Result<usize> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let url = format!("{}/models", cfg.base_url.as_deref().unwrap_or("").trim_end_matches('/'));
    let resp = client.get(&url)
        .header("Authorization", format!("Bearer {}", cfg.api_key()?))
        .send()?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    let v: serde_json::Value = serde_json::from_str(&resp.text()?)?;
    Ok(v["data"].as_array().map(Vec::len).unwrap_or(0))
}

fn check_workspace(workspace: &Path, fix: bool) -> Result<String> {
    if !workspace.exists() {
        anyhow::bail!("не существует: {}", workspace.display());
    }
    let d = workspace.join(".theseus");
    if !d.exists() {
        if fix {
            std::fs::create_dir_all(&d)?;
            return Ok(format!("{} (создан .theseus)", workspace.display()));
        }
        anyhow::bail!("нет .theseus (запустите с --fix для создания)");
    }
    // проверка записи
    let probe = d.join(".doctor_probe");
    std::fs::write(&probe, "ok")?;
    let _ = std::fs::remove_file(&probe);
    Ok(format!("{} (запись ok)", workspace.display()))
}

fn check_rules(cfg: &Config) -> Result<usize> {
    for p in &cfg.permission.bash_deny_patterns {
        regex::Regex::new(p).map_err(|e| anyhow::anyhow!("{p}: {e}"))?;
    }
    for r in &cfg.permission_rules {
        if !matches!(r.decision.as_str(), "allow" | "ask" | "deny") {
            anyhow::bail!("неизвестный decision «{}» в правиле «{}»", r.decision, r.pattern);
        }
    }
    Ok(cfg.permission.bash_deny_patterns.len() + cfg.permission_rules.len())
}

fn skill_dirs(workspace: &Path, cfg: &Config) -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> = cfg.skill_dirs.iter().map(std::path::PathBuf::from).collect();
    dirs.push(workspace.join(".theseus/skills"));
    if let Some(h) = std::env::var("HOME").ok().map(std::path::PathBuf::from) {
        dirs.push(h.join(".theseus/skills"));
    }
    dirs
}
