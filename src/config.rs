//! Конфигурация харнесса: слоёный конфиг (`config_layers`) + env + CLI-оверрайды.

use crate::config_layers::{self, ConfigIssue, Severity};
use crate::models;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use toml::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive] // добавление полей не ломает внешних потребителей (V3 #8)
pub struct McpServerConfig {
    pub name: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// HTTP-транспорт (v0.3.1): если задан — используется вместо stdio
    #[serde(default)]
    pub url: Option<String>,
    /// имя env-переменной с токеном для Authorization: Bearer (v0.3.1)
    #[serde(default)]
    pub env_key: Option<String>,
    /// политика elicitation: accept | decline (default decline) (v0.3.1)
    #[serde(default)]
    pub elicit: Option<String>,
}

/// Правило разрешения (v0.3, урок Claude Code/Codex rules)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    /// allow | ask | deny
    pub decision: String,
    /// паттерн: "Bash(prefix)" | "Read(prefix)" | "Write(prefix)" | "Tool"
    pub pattern: String,
    #[serde(default)]
    pub reason: String,
}

/// Хук (v0.3): shell-команда на событие
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive] // добавление полей не ломает внешних потребителей (V3 #8)
pub struct HookConfig {
    /// PreToolUse | PostToolUse | UserPromptSubmit | SessionStart | SessionEnd
    /// PreCompact | PostCompact | Notification | GoalSet
    pub event: String,
    /// имя инструмента или "*" (для ToolUse-событий)
    #[serde(default = "star")]
    pub matcher: String,
    /// shell-команда: JSON события на stdin; exit 2 = блок, stderr → причина
    pub command: String,
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
}

fn star() -> String { "*".into() }
fn default_hook_timeout() -> u64 { 10 }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    /// Оценочный лимит контекста в токенах (chars/4-эвристика)
    #[serde(default = "default_context_limit")]
    pub context_limit_tokens: usize,
    /// Потолок max_tokens на один ответ
    #[serde(default = "default_max_output")]
    pub max_output_tokens: usize,
    /// Таймаут одного API-вызова, сек
    #[serde(default = "default_timeout")]
    pub api_timeout_secs: u64,
    /// thinking-режим провайдера (deepseek: {"thinking":{"type":"enabled"}})
    #[serde(default)]
    pub extra_body: serde_json::Value,
    #[serde(default)]
    pub permission: PermissionConfig,
    /// MCP stdio-серверы (v0.2)
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// Правила разрешений (v0.3)
    #[serde(default)]
    pub permission_rules: Vec<PermissionRule>,
    /// Хуки (v0.3)
    #[serde(default)]
    pub hooks: Vec<HookConfig>,
    /// Каталоги поиска скиллов (v0.3); default: .theseus/skills, ~/.theseus/skills
    #[serde(default)]
    pub skill_dirs: Vec<String>,
    /// web_fetch: разрешённые домены (пусто = выключен) (v0.3)
    #[serde(default)]
    pub web_allowed_domains: Vec<String>,
    /// ядерный sandbox (landlock) для bash (v0.3.1); default: включён
    #[serde(default = "default_true")]
    pub sandbox: bool,
    /// Пороги трёхуровневой компактификации, % окна (v0.3.2, по OpenDev ACC)
    #[serde(default = "pct70")]
    pub compact_mask_pct: usize,
    #[serde(default = "pct80")]
    pub compact_prune_pct: usize,
    #[serde(default = "pct95")]
    pub compact_summary_pct: usize,
}

fn default_true() -> bool { true }
fn pct70() -> usize { 70 }
fn pct80() -> usize { 80 }
fn pct95() -> usize { 95 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionConfig {
    /// hard-deny regex-паттерны для bash (проверяются всегда, даже в yolo)
    #[serde(default = "default_deny_patterns")]
    pub bash_deny_patterns: Vec<String>,
    /// дополнительные auto-allow префиксы bash-команд
    #[serde(default)]
    pub bash_allow_prefixes: Vec<String>,
}

impl Default for PermissionConfig {
    fn default() -> Self {
        PermissionConfig {
            bash_deny_patterns: default_deny_patterns(),
            bash_allow_prefixes: vec![],
        }
    }
}

fn default_model() -> String { "deepseek-v4-pro".into() }
fn default_context_limit() -> usize { 120_000 }
fn default_max_output() -> usize { 8_192 }
fn default_timeout() -> u64 { 600 }
fn default_deny_patterns() -> Vec<String> {
    vec![
        r"rm\s+-[a-z]*r[a-z]*f[a-z]*\s+(/(\s|$|\*)|~|\$HOME)".into(),
        r">\s*/dev/sd[a-z]".into(),
        r"mkfs\.".into(),
        r":\(\)\s*\{".into(), // fork bomb
    ]
}

/// extra_body по умолчанию для режима «вообще без конфиг-файлов»: thinking включён
/// (прежнее поведение fallback-ветки Config::load).
fn default_extra_body() -> serde_json::Value {
    serde_json::json!({"thinking": {"type": "enabled"}})
}

impl Config {
    /// Загрузить конфигурацию слоями (приоритет от низшего к высшему):
    /// `defaults < ~/.config/theseus/config.toml < ./.theseus/config.toml < CLI`.
    pub fn load(cli_base_url: Option<&str>, cli_model: Option<&str>) -> Result<Self> {
        // Глобальный слой — прежнее расположение. Workspace-слой (новый,
        // опциональный) ищется от текущего каталога процесса: сигнатуру
        // load(base_url, model) сохраняем, каталог workspace сюда не передаётся.
        let global = Self::config_path().unwrap_or_default();
        let workspace = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".theseus/config.toml");
        Self::load_from_paths(&global, &workspace, cli_base_url, cli_model)
    }

    /// Загрузка с явными путями слоёв (вынесена ради тестируемости).
    fn load_from_paths(
        global_path: &Path,
        workspace_path: &Path,
        cli_base_url: Option<&str>,
        cli_model: Option<&str>,
    ) -> Result<Self> {
        // CLI-оверрайды — самый приоритетный слой.
        let mut cli: Vec<(&str, &str)> = Vec::new();
        if let Some(u) = cli_base_url { cli.push(("base_url", u)); }
        if let Some(m) = cli_model { cli.push(("model", m)); }

        let files_existed = global_path.exists() || workspace_path.exists();
        let (merged, issues) = config_layers::load_layered(global_path, workspace_path, &cli)
            .context("загрузка слоёв конфигурации")?;

        // Валидация config_layers: Warn → stderr, Error → отказ в загрузке.
        // Замечания-артефакты двойственности схем (legacy-формы текущего
        // Config) пропускаем: их корректность обеспечивает маппинг ниже.
        let mut errors: Vec<String> = Vec::new();
        for issue in &issues {
            if is_legacy_artifact(issue) { continue; }
            match issue.severity {
                Severity::Warn => eprintln!("config: {issue}"),
                Severity::Error => errors.push(format!("{issue}")),
            }
        }
        if !errors.is_empty() {
            let joined = errors.join("\n");
            anyhow::bail!("некорректная конфигурация:\n{joined}");
        }

        // Defaults-слой config_layers описывает чужую схему (qwen3-coder,
        // compact_l*, sandbox-таблица): вычитаем его значения, чтобы дефолты
        // theseus задали serde-дефолты полей Config.
        let mut effective = merged;
        strip_foreign_defaults(&mut effective, &config_layers::ConfigLayer::defaults().toml_table);

        // Обе схемы (legacy и codex-стиль) → форма структуры Config.
        normalize_sandbox(&mut effective);
        normalize_mcp_servers(&mut effective);
        normalize_compact_levels(&mut effective);
        let explicit_context_limit = effective.get("context_limit_tokens").is_some();

        let mut cfg: Config = Config::deserialize(effective)
            .context("маппинг смердженной конфигурации в Config")?;
        if !files_existed && cfg.extra_body.is_null() {
            // Прежнее поведение ветки «конфига нет»: thinking включён.
            cfg.extra_body = default_extra_body();
        }

        // env-оверрайды (прежняя семантика: заполняют только пустые места).
        if cfg.api_key.is_none() {
            cfg.api_key = std::env::var("DEEPSEEK_API_KEY").ok()
                .or_else(|| std::env::var("THESEUS_API_KEY").ok());
        }
        if cfg.base_url.is_none() {
            cfg.base_url = std::env::var("THESEUS_BASE_URL").ok();
        }
        // Реестр моделей: без явного base_url URL и лимит контекста берутся
        // из models; неизвестная модель — ошибка с подсказкой ближайших.
        if cfg.base_url.is_none() {
            let info = models::find_model(&cfg.model)
                .ok_or_else(|| unknown_model_err(&cfg.model))?;
            let provider = models::find_provider(&info.provider)
                .with_context(|| format!("нет провайдера «{}» для модели {}", info.provider, info.id))?;
            if let Some(note) = &provider.risk_note {
                eprintln!("config: {note}");
            }
            cfg.base_url = Some(provider.effective_base_url());
            if !explicit_context_limit {
                cfg.context_limit_tokens = info.context_limit;
            }
        }
        Ok(cfg)
    }

    fn config_path() -> Option<PathBuf> {
        std::env::var("HOME").ok().map(PathBuf::from)
            .map(|h| h.join(".config/theseus/config.toml"))
    }

    pub fn api_key(&self) -> Result<&str> {
        self.api_key.as_deref()
            .context("нет API-ключа: задайте DEEPSEEK_API_KEY или api_key в config.toml")
    }
}

/// Ошибка «неизвестная модель» с подсказкой ближайших (зеркалит models::resolve).
fn unknown_model_err(id: &str) -> anyhow::Error {
    let near = models::nearest_models(id, 3);
    let hint = if near.is_empty() {
        let all: Vec<String> = models::builtin_models().into_iter().map(|m| m.id).collect();
        let joined = all.join(", ");
        format!("зарегистрированные модели: {joined}")
    } else {
        let names: Vec<&str> = near.iter().map(|(name, _)| name.as_str()).collect();
        let joined = names.join(", ");
        format!("похожие модели: {joined}")
    };
    anyhow::anyhow!("неизвестная модель «{id}»; {hint}")
}

/// Замечание валидатора — артефакт двойственности схем?
///
/// `config_layers` валидирует codex-стиль (mcp_servers — таблица таблиц,
/// sandbox — таблица, permission.mode), а текущая схема Config — legacy-формы
/// ([[mcp_servers]] массивом, `sandbox = bool`, permission.bash_*). Такие
/// замечания не эскалируем: маппинг ниже обе формы поддерживает.
fn is_legacy_artifact(issue: &ConfigIssue) -> bool {
    // Поля Config, отсутствующие в KNOWN_KEYS валидатора, — не «неизвестные».
    const CONFIG_ONLY_KEYS: &[&str] = &[
        "api_key", "max_output_tokens", "api_timeout_secs", "extra_body",
        "skill_dirs", "compact_mask_pct", "compact_prune_pct", "compact_summary_pct",
    ];
    match issue.path.as_str() {
        "mcp_servers" | "sandbox" => true,
        "permission.bash_deny_patterns" | "permission.bash_allow_prefixes" => true,
        p => CONFIG_ONLY_KEYS.contains(&p),
    }
}

/// Вычесть из `v` значения, равные defaults-слою config_layers (рекурсивно).
///
/// Ключ исчезает, только если его значение совпадает с defaults — тогда про
/// него решают serde-дефолты Config. Явно заданное пользователем значение
/// сохраняется (известная оговорка: значение, совпадающее с чужим defaults,
/// например `model = "qwen3-coder"`, будет трактовано как незаданное).
fn strip_foreign_defaults(v: &mut Value, defaults: &Value) {
    let (Some(table), Some(defs)) = (v.as_table_mut(), defaults.as_table()) else { return };
    for (key, dv) in defs {
        let Some(mut existing) = table.get(key).cloned() else { continue };
        if existing == *dv {
            table.remove(key);
        } else if existing.is_table() && dv.is_table() {
            strip_foreign_defaults(&mut existing, dv);
            if existing.as_table().is_some_and(toml::Table::is_empty) {
                table.remove(key);
            } else {
                table.insert(key.clone(), existing);
            }
        }
    }
}

/// sandbox: legacy-форма — булево (`sandbox = false`) — остаётся как есть;
/// табличная ([sandbox] enabled = ...) сводится к полю `enabled`. Прочие
/// типы оставляем десериализации — она честно упадёт с ошибкой типа.
fn normalize_sandbox(v: &mut Value) {
    let Some(table) = v.as_table_mut() else { return };
    let Some(Value::Table(t)) = table.get("sandbox").cloned() else { return };
    if let Some(Value::Boolean(b)) = t.get("enabled") {
        table.insert("sandbox".into(), Value::Boolean(*b));
    }
}

/// mcp_servers: legacy-форма — массив ([[mcp_servers]] с полем name) —
/// остаётся; табличная ([mcp_servers.name]) разворачивается в массив с полем
/// name (порядок — по имени, детерминированно). Нетабличные записи оставляем
/// как есть: десериализация в Vec<McpServerConfig> честно упадёт.
fn normalize_mcp_servers(v: &mut Value) {
    let Some(table) = v.as_table_mut() else { return };
    let Some(Value::Table(t)) = table.get("mcp_servers").cloned() else { return };
    if t.values().any(|s| !s.is_table()) { return; }
    let mut entries: Vec<(String, toml::Table)> = t
        .iter()
        .filter_map(|(name, val)| val.as_table().map(|tb| (name.clone(), tb.clone())))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let arr: Vec<Value> = entries
        .into_iter()
        .map(|(name, mut def)| {
            def.insert("name".into(), Value::String(name));
            Value::Table(def)
        })
        .collect();
    table.insert("mcp_servers".into(), Value::Array(arr));
}

/// Число из TOML: Float как есть, Integer — с приведением (как в config_layers).
fn as_f64(v: &Value) -> Option<f64> {
    v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
}

/// Пороги компакта новой схемы (compact_l1..3, доли окна) → поля Config
/// (compact_*_pct, проценты). Legacy-поля compact_*_pct, заданные явно,
/// приоритетнее. Диапазон (0.0, 1.0) уже гарантирован валидатором.
fn normalize_compact_levels(v: &mut Value) {
    let Some(table) = v.as_table_mut() else { return };
    for (level, pct) in [
        ("compact_l1", "compact_mask_pct"),
        ("compact_l2", "compact_prune_pct"),
        ("compact_l3", "compact_summary_pct"),
    ] {
        let Some(f) = table.get(level).and_then(as_f64) else { continue };
        if table.contains_key(pct) { continue; }
        table.insert(pct.into(), Value::Integer((f * 100.0).round() as i64));
    }
}

/// Пример генерации дефолтного конфига для пользователя
pub fn write_example_config() -> Result<PathBuf> {
    let dir = std::env::var("HOME").map(PathBuf::from)?.join(".config/theseus");
    std::fs::create_dir_all(&dir)?;
    let p = dir.join("config.toml");
    if !p.exists() {
        std::fs::write(&p, r#"# theseus config
model = "deepseek-v4-pro"
# base_url = "https://api.deepseek.com/v1"
# api_key задаётся через env DEEPSEEK_API_KEY (не храните ключ в файле)
context_limit_tokens = 120000
max_output_tokens = 8192
api_timeout_secs = 600

extra_body = { thinking = { type = "enabled" } }

[permission]
# bash_allow_prefixes = ["make", "docker ps"]
"#)?;
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Сериализация тестов, трогающих env: они исполняются в одном процессе.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Временный каталог для файловых тестов.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("theseus_config_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Ожидаемый base_url провайдера с учётом env-переопределений (зеркало кода).
    fn expected_url(provider: &str) -> String {
        std::env::var("THESEUS_BASE_URL").ok().unwrap_or_else(|| {
            models::find_provider(provider).unwrap().effective_base_url()
        })
    }

    #[test]
    fn workspace_layer_overrides_global() {
        let dir = temp_dir("ws_over_global");
        let global = dir.join("global.toml");
        let workspace = dir.join("workspace.toml");
        std::fs::write(&global, "model = \"kimi-k2\"\nmax_output_tokens = 4096\n").unwrap();
        std::fs::write(&workspace, "model = \"deepseek-chat\"\n").unwrap();
        let cfg = Config::load_from_paths(&global, &workspace, None, None).unwrap();
        // workspace перекрывает global...
        assert_eq!(cfg.model, "deepseek-chat");
        // ...а нетронутый ключ доезжает из global
        assert_eq!(cfg.max_output_tokens, 4096);
        // без явного base_url — резолюция через реестр (провайдер deepseek)
        assert_eq!(cfg.base_url.as_deref(), Some(expected_url("deepseek").as_str()));
        assert_eq!(cfg.context_limit_tokens, 131_072);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_overrides_beat_all_layers() {
        let dir = temp_dir("cli_wins");
        let global = dir.join("global.toml");
        let workspace = dir.join("workspace.toml");
        std::fs::write(&global, "model = \"kimi-k2\"\nbase_url = \"https://g.example/v1\"\n").unwrap();
        std::fs::write(&workspace, "model = \"deepseek-chat\"\nbase_url = \"https://w.example/v1\"\n").unwrap();
        let cfg = Config::load_from_paths(
            &global,
            &workspace,
            Some("http://127.0.0.1:9/v1"),
            Some("kimi-k3"),
        )
        .unwrap();
        // CLI — верхний слой: бьёт и workspace, и global.
        assert_eq!(cfg.model, "kimi-k3");
        assert_eq!(cfg.base_url.as_deref(), Some("http://127.0.0.1:9/v1"));
        // Явный base_url → реестр не вызывается, лимит — дефолт Config.
        assert_eq!(cfg.context_limit_tokens, 120_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_model_suggests_nearest() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Резолюция через реестр срабатывает только без явного base_url:
        // прячем env-оверрайд на время теста.
        let saved = std::env::var("THESEUS_BASE_URL").ok();
        std::env::remove_var("THESEUS_BASE_URL");
        let dir = temp_dir("unknown_model");
        let res = Config::load_from_paths(
            &dir.join("g.toml"),
            &dir.join("w.toml"),
            None,
            Some("deepseek-chatt"),
        );
        if let Some(v) = saved { std::env::set_var("THESEUS_BASE_URL", v); }
        let err = res.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("неизвестная модель"), "msg: {msg}");
        assert!(msg.contains("deepseek-chat"), "msg: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_compact_threshold_is_error() {
        let dir = temp_dir("bad_compact");
        let workspace = dir.join("workspace.toml");
        // Выход за диапазон (0.0, 1.0).
        std::fs::write(&workspace, "compact_l2 = 1.5\n").unwrap();
        let err = Config::load_from_paths(&dir.join("g.toml"), &workspace, None, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("compact_l2"), "msg: {msg}");
        // Нарушен порядок l1 >= l2.
        std::fs::write(&workspace, "compact_l1 = 0.9\ncompact_l2 = 0.5\n").unwrap();
        let err = Config::load_from_paths(&dir.join("g.toml"), &workspace, None, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("compact_l1"), "msg: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_resolves_url_and_context_when_no_base_url() {
        let dir = temp_dir("registry");
        let global = dir.join("global.toml");
        std::fs::write(&global, "model = \"kimi-k2\"\n").unwrap();
        let cfg = Config::load_from_paths(&global, &dir.join("w.toml"), None, None).unwrap();
        assert_eq!(cfg.base_url.as_deref(), Some(expected_url("kimi").as_str()));
        assert_eq!(cfg.context_limit_tokens, 131_072);
        // Явно заданный лимит контекста реестр не перекрывает.
        std::fs::write(&global, "model = \"kimi-k2\"\ncontext_limit_tokens = 120000\n").unwrap();
        let cfg = Config::load_from_paths(&global, &dir.join("w.toml"), None, None).unwrap();
        assert_eq!(cfg.context_limit_tokens, 120_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn defaults_preserved_without_files() {
        let dir = temp_dir("defaults");
        let cfg = Config::load_from_paths(&dir.join("g.toml"), &dir.join("w.toml"), None, None).unwrap();
        assert_eq!(cfg.model, "deepseek-v4-pro");
        assert_eq!(cfg.max_output_tokens, 8_192);
        assert_eq!(cfg.api_timeout_secs, 600);
        assert_eq!(cfg.compact_mask_pct, 70);
        assert_eq!(cfg.compact_prune_pct, 80);
        assert_eq!(cfg.compact_summary_pct, 95);
        assert!(cfg.sandbox);
        assert!(!cfg.permission.bash_deny_patterns.is_empty());
        // thinking по умолчанию (режим «вообще без конфига», как раньше)
        assert!(cfg.extra_body.is_object());
        // реестр: deepseek-v4-pro → URL провайдера и лимит 131_072
        assert_eq!(cfg.base_url.as_deref(), Some(expected_url("deepseek").as_str()));
        assert_eq!(cfg.context_limit_tokens, 131_072);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_full_config_parses_identically() {
        let dir = temp_dir("legacy");
        let global = dir.join("global.toml");
        // Форма боевого конфига: [[permission_rules]], [[hooks]], [[mcp_servers]],
        // web_allowed_domains и прочие поля текущей схемы верхнего уровня.
        std::fs::write(&global, r#"
model = "deepseek-v4-pro"
web_allowed_domains = ["duckduckgo.com", "api.duckduckgo.com", "wikipedia.org", "ru.wikipedia.org"]
context_limit_tokens = 120000
max_output_tokens = 8192
api_timeout_secs = 600
sandbox = false
skill_dirs = ["./skills"]
extra_body = { thinking = { type = "enabled" } }

[permission]
bash_allow_prefixes = ["make"]

[[permission_rules]]
decision = "deny"
pattern = "Bash(rm)"
reason = "тест"

[[hooks]]
event = "PreToolUse"
matcher = "bash"
command = "exit 0"
timeout_secs = 5

[[mcp_servers]]
name = "mock"
command = "python3"
args = ["mock_mcp.py"]

[[mcp_servers]]
name = "httpmock"
url = "http://127.0.0.1:8901/mcp"
elicit = "accept"
"#).unwrap();
        let cfg = Config::load_from_paths(&global, &dir.join("w.toml"), None, None).unwrap();
        assert_eq!(cfg.model, "deepseek-v4-pro");
        assert_eq!(cfg.web_allowed_domains.len(), 4);
        assert!(!cfg.sandbox);
        assert_eq!(cfg.skill_dirs, vec!["./skills"]);
        assert_eq!(cfg.permission.bash_allow_prefixes, vec!["make"]);
        assert!(!cfg.permission.bash_deny_patterns.is_empty());
        assert_eq!(cfg.permission_rules.len(), 1);
        assert_eq!(cfg.permission_rules[0].decision, "deny");
        assert_eq!(cfg.permission_rules[0].pattern, "Bash(rm)");
        assert_eq!(cfg.permission_rules[0].reason, "тест");
        assert_eq!(cfg.hooks.len(), 1);
        assert_eq!(cfg.hooks[0].event, "PreToolUse");
        assert_eq!(cfg.hooks[0].matcher, "bash");
        assert_eq!(cfg.hooks[0].timeout_secs, 5);
        assert_eq!(cfg.mcp_servers.len(), 2);
        assert_eq!(cfg.mcp_servers[0].name, "mock");
        assert_eq!(cfg.mcp_servers[0].command, "python3");
        assert_eq!(cfg.mcp_servers[0].args, vec!["mock_mcp.py"]);
        assert_eq!(cfg.mcp_servers[1].name, "httpmock");
        assert_eq!(cfg.mcp_servers[1].url.as_deref(), Some("http://127.0.0.1:8901/mcp"));
        assert_eq!(cfg.mcp_servers[1].elicit.as_deref(), Some("accept"));
        assert!(cfg.extra_body.is_object());
        // Явный лимит контекста сохраняется (реестр его не перекрывает).
        assert_eq!(cfg.context_limit_tokens, 120_000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
