//! Слоёная конфигурация Theseus (по образцу `codex-rs/config`).
//!
//! Приоритет слоёв (от низшего к высшему):
//! `defaults` → `global` (`~/.theseus/config.toml`) →
//! `workspace` (`./.theseus/config.toml`) → `CLI`-оверрайды.
//!
//! - [`merge`] — глубокий мердж: таблицы рекурсивно, скаляры и массивы перекрываются;
//! - [`validate`] — проверка результата: известные ключи, типы, диапазоны;
//! - [`load_layered`] — загрузка цепочки «файлы + CLI» за один вызов.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result};
use regex::Regex;
use toml::{Table, Value};

/// Известные ключи верхнего уровня конфигурации.
///
/// Любой другой ключ верхнего уровня — не ошибка, а предупреждение
/// (конфигурация может опережать версию харнесса).
pub const KNOWN_KEYS: &[&str] = &[
    "model",
    "base_url",
    "context_limit_tokens",
    "sandbox",
    "compact_l1",
    "compact_l2",
    "compact_l3",
    "permission",
    "permission_rules",
    "mcp_servers",
    "hooks",
    "web_allowed_domains",
    "notify",
];

/// Допустимые режимы `permission.mode`.
pub const PERMISSION_MODES: &[&str] = &["ask", "dontAsk", "yolo"];

/// Допустимые решения в `permission_rules[].decision`.
const RULE_DECISIONS: &[&str] = &["allow", "ask", "deny"];

/// Известные события хуков (нестандартное событие — предупреждение).
const HOOK_EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "UserPromptSubmit",
    "SessionStart",
    "SessionEnd",
];

/// Известные подключи таблицы `sandbox`.
const SANDBOX_KEYS: &[&str] = &["enabled", "allow_network", "writable_paths"];

/// Минимально допустимый лимит контекста в токенах.
const MIN_CONTEXT_LIMIT: i64 = 4096;

/// Шаблон допустимого домена (с опциональным префиксом `*.`).
const DOMAIN_RE: &str =
    r"^(\*\.)?[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?(\.[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?)*$";

/// Один слой конфигурации: имя (для диагностики) + TOML-таблица.
#[derive(Debug, Clone)]
pub struct ConfigLayer {
    /// Имя слоя (`defaults`, `global`, `workspace`, `cli`, ...).
    pub name: String,
    /// Содержимое слоя; корень всегда TOML-таблица.
    pub toml_table: Value,
}

impl ConfigLayer {
    /// Создать слой из готового значения.
    pub fn new(name: impl Into<String>, toml_table: Value) -> Self {
        Self {
            name: name.into(),
            toml_table,
        }
    }

    /// Пустой слой (пустая таблица) — заглушка для отсутствующего файла.
    pub fn empty(name: impl Into<String>) -> Self {
        Self::new(name, Value::Table(Table::new()))
    }

    /// Разобрать слой из TOML-текста. Корень обязан быть таблицей.
    pub fn parse(name: impl Into<String>, text: &str) -> Result<Self> {
        let name = name.into();
        let value = text
            .parse::<Value>()
            .with_context(|| format!("слой «{name}»: ошибка разбора TOML"))?;
        anyhow::ensure!(
            value.is_table(),
            "слой «{name}»: корень конфигурации должен быть TOML-таблицей"
        );
        Ok(Self::new(name, value))
    }

    /// Прочитать слой из файла. Отсутствующий файл — ошибка
    /// (для необязательных файлов см. [`load_layered`]).
    pub fn from_file(name: impl Into<String>, path: &Path) -> Result<Self> {
        let name = name.into();
        let text = fs::read_to_string(path)
            .with_context(|| format!("слой «{name}»: не удалось прочитать {}", path.display()))?;
        Self::parse(name, &text)
    }

    /// Слой встроенных умолчаний харнесса (низший приоритет).
    pub fn defaults() -> Self {
        let mut root = Table::new();
        root.insert("model".into(), Value::String("qwen3-coder".into()));
        root.insert("context_limit_tokens".into(), Value::Integer(128_000));
        root.insert("compact_l1".into(), Value::Float(0.60));
        root.insert("compact_l2".into(), Value::Float(0.80));
        root.insert("compact_l3".into(), Value::Float(0.90));

        let mut permission = Table::new();
        permission.insert("mode".into(), Value::String("ask".into()));
        root.insert("permission".into(), Value::Table(permission));

        let mut sandbox = Table::new();
        sandbox.insert("enabled".into(), Value::Boolean(true));
        sandbox.insert("allow_network".into(), Value::Boolean(false));
        root.insert("sandbox".into(), Value::Table(sandbox));

        Self::new("defaults", Value::Table(root))
    }
}

/// Серьёзность замечания по конфигурации.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Подозрительно, но работать можно (например, неизвестный ключ).
    Warn,
    /// Конфигурация некорректна, значение сломает работу или будет проигнорировано.
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Severity::Warn => "WARN",
            Severity::Error => "ERROR",
        };
        f.write_str(s)
    }
}

/// Одно замечание валидатора: путь к ключу + серьёзность + текст.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigIssue {
    /// Путь к проблемному ключу, например `permission.mode` или `hooks[0].event`.
    pub path: String,
    /// Серьёзность: предупреждение или ошибка.
    pub severity: Severity,
    /// Человекочитаемое описание проблемы.
    pub message: String,
}

impl ConfigIssue {
    fn warn(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            severity: Severity::Warn,
            message: message.into(),
        }
    }

    fn error(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            severity: Severity::Error,
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.severity, self.path, self.message)
    }
}

/// Глубокий мердж слоёв в одну TOML-таблицу.
///
/// Слои применяются слева направо: каждый следующий перекрывает предыдущие.
/// Таблицы сливаются рекурсивно по ключам; скаляры и массивы
/// перекрываются целиком (массивы НЕ конкатенируются).
pub fn merge(layers: &[ConfigLayer]) -> Value {
    let mut acc = Value::Table(Table::new());
    for layer in layers {
        acc = merge_values(&acc, &layer.toml_table);
    }
    acc
}

/// Рекурсивный мердж двух значений: `over` перекрывает `base`.
fn merge_values(base: &Value, over: &Value) -> Value {
    match (base, over) {
        (Value::Table(b), Value::Table(o)) => {
            let mut out = b.clone();
            for (key, value) in o {
                let merged = match out.get(key) {
                    Some(existing) => merge_values(existing, value),
                    None => value.clone(),
                };
                out.insert(key.clone(), merged);
            }
            Value::Table(out)
        }
        // Скаляры и массивы перекрываются целиком.
        _ => over.clone(),
    }
}

/// Проверить итоговую конфигурацию: известные ключи, типы, диапазоны.
///
/// Ничего не паникует: все проблемы возвращаются списком.
pub fn validate(config: &Value) -> Vec<ConfigIssue> {
    let mut issues = Vec::new();
    let Some(table) = config.as_table() else {
        issues.push(ConfigIssue::error(
            "$",
            "корень конфигурации должен быть TOML-таблицей",
        ));
        return issues;
    };

    for key in table.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            issues.push(ConfigIssue::warn(
                key.as_str(),
                format!("неизвестный ключ верхнего уровня «{key}»"),
            ));
        }
    }

    validate_model(table, &mut issues);
    validate_base_url(table, &mut issues);
    validate_context_limit(table, &mut issues);
    validate_compact(table, &mut issues);
    validate_sandbox(table, &mut issues);
    validate_permission(table, &mut issues);
    validate_permission_rules(table, &mut issues);
    validate_mcp_servers(table, &mut issues);
    validate_hooks(table, &mut issues);
    validate_web_domains(table, &mut issues);
    validate_notify(table, &mut issues);
    issues
}

/// Загрузить слоёную конфигурацию: defaults < global < workspace < CLI.
///
/// - отсутствующие файлы трактуются как пустые слои (не ошибка);
/// - битый TOML или нечитаемый файл — ошибка [`anyhow::Error`];
/// - `cli_overrides` — пары `(путь.с.точками, значение)`; значение сначала
///   разбирается как TOML-литерал (`42`, `true`, `"str"`, `[1, 2]`),
///   а при неудаче — как обычная строка.
///
/// Возвращает смердженную таблицу и список замечаний валидатора.
pub fn load_layered(
    global_path: &Path,
    workspace_path: &Path,
    cli_overrides: &[(&str, &str)],
) -> Result<(Value, Vec<ConfigIssue>)> {
    let layers = vec![
        ConfigLayer::defaults(),
        read_optional_layer("global", global_path)?,
        read_optional_layer("workspace", workspace_path)?,
        cli_layer(cli_overrides)?,
    ];
    let merged = merge(&layers);
    let issues = validate(&merged);
    Ok((merged, issues))
}

/// Прочитать необязательный файл конфигурации: нет файла — пустой слой.
fn read_optional_layer(name: &str, path: &Path) -> Result<ConfigLayer> {
    match fs::read_to_string(path) {
        Ok(text) => ConfigLayer::parse(name, &text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(ConfigLayer::empty(name)),
        Err(e) => Err(anyhow::Error::new(e)
            .context(format!("слой «{name}»: не удалось прочитать {}", path.display()))),
    }
}

/// Построить CLI-слой из пар «точечный путь → значение».
fn cli_layer(overrides: &[(&str, &str)]) -> Result<ConfigLayer> {
    let mut root = Value::Table(Table::new());
    for (path, raw) in overrides {
        let value = parse_toml_literal(raw).unwrap_or_else(|| Value::String((*raw).to_string()));
        set_dotted(&mut root, path, value)
            .with_context(|| format!("некорректный CLI-оверрайд «{path}»"))?;
    }
    Ok(ConfigLayer::new("cli", root))
}

/// Разобрать CLI-значение как TOML-литерал (`42`, `true`, `"str"`, `[1, 2]`).
///
/// `toml::Value::from_str` разбирает целый документ, а не значение,
/// поэтому литерал оборачивается в фиктивный ключ.
fn parse_toml_literal(raw: &str) -> Option<Value> {
    let mut doc = format!("__v = {raw}").parse::<Table>().ok()?;
    doc.remove("__v")
}

/// Пустая TOML-таблица как значение (для `or_insert_with` без замыкания).
fn new_table() -> Value {
    Value::Table(Table::new())
}

/// Установить значение по точечному пути (`a.b.c`), создавая таблицы.
fn set_dotted(root: &mut Value, path: &str, value: Value) -> Result<()> {
    anyhow::ensure!(!path.is_empty(), "пустой путь оверрайда");
    let mut cursor = root;
    let mut parts = path.split('.').peekable();
    while let Some(part) = parts.next() {
        anyhow::ensure!(!part.is_empty(), "пустой сегмент в пути «{path}»");
        let table = cursor
            .as_table_mut()
            .with_context(|| format!("сегмент «{part}» пути «{path}» упирается в скаляр"))?;
        if parts.peek().is_none() {
            table.insert(part.to_string(), value);
            return Ok(());
        }
        cursor = table.entry(part.to_string()).or_insert_with(new_table);
    }
    Ok(())
}

/// Число из TOML: Float как есть, Integer — с приведением.
fn as_f64(v: &Value) -> Option<f64> {
    v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
}

/// Имя типа значения для сообщений об ошибках.
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "строка",
        Value::Integer(_) => "целое число",
        Value::Float(_) => "число с плавающей точкой",
        Value::Boolean(_) => "булево",
        Value::Datetime(_) => "дата/время",
        Value::Array(_) => "массив",
        Value::Table(_) => "таблица",
    }
}

/// Замечание «неверный тип» в едином формате.
fn type_err(path: impl Into<String>, expected: &str, got: &Value) -> ConfigIssue {
    ConfigIssue::error(path, format!("ожидался тип {expected}, получено: {}", type_name(got)))
}

/// `model`: непустая строка.
fn validate_model(table: &Table, issues: &mut Vec<ConfigIssue>) {
    let Some(v) = table.get("model") else { return };
    match v.as_str() {
        Some(s) if !s.trim().is_empty() => {}
        Some(_) => issues.push(ConfigIssue::error("model", "имя модели не должно быть пустым")),
        None => issues.push(type_err("model", "строка", v)),
    }
}

/// `base_url`: строка со схемой http/https (иначе — предупреждение).
fn validate_base_url(table: &Table, issues: &mut Vec<ConfigIssue>) {
    let Some(v) = table.get("base_url") else { return };
    match v.as_str() {
        Some(u) if u.starts_with("https://") || u.starts_with("http://") => {}
        Some(_) => issues.push(ConfigIssue::warn(
            "base_url",
            "URL должен начинаться с http:// или https://",
        )),
        None => issues.push(type_err("base_url", "строка", v)),
    }
}

/// `context_limit_tokens`: целое >= [`MIN_CONTEXT_LIMIT`].
fn validate_context_limit(table: &Table, issues: &mut Vec<ConfigIssue>) {
    let Some(v) = table.get("context_limit_tokens") else { return };
    match v.as_integer() {
        Some(n) if n < MIN_CONTEXT_LIMIT => issues.push(ConfigIssue::error(
            "context_limit_tokens",
            format!("лимит контекста {n} слишком мал: минимум {MIN_CONTEXT_LIMIT}"),
        )),
        Some(_) => {}
        None => issues.push(type_err("context_limit_tokens", "целое число", v)),
    }
}

/// `compact_l1/l2/l3`: числа из (0.0, 1.0), строго возрастающие.
fn validate_compact(table: &Table, issues: &mut Vec<ConfigIssue>) {
    const KEYS: [&str; 3] = ["compact_l1", "compact_l2", "compact_l3"];
    let mut found: Vec<(&str, f64)> = Vec::new();
    for key in KEYS {
        let Some(v) = table.get(key) else { continue };
        match as_f64(v) {
            Some(f) if f > 0.0 && f < 1.0 => found.push((key, f)),
            Some(_) => issues.push(ConfigIssue::error(
                key,
                format!("порог компакта должен лежать в (0.0, 1.0), получено {v}"),
            )),
            None => issues.push(type_err(key, "число", v)),
        }
    }
    // Порядок проверяется по присутствующим значениям (пропуски допустимы).
    for pair in found.windows(2) {
        let (ka, va) = pair[0];
        let (kb, vb) = pair[1];
        if va >= vb {
            issues.push(ConfigIssue::error(
                ka,
                format!("нарушен порядок компакта: {ka} ({va}) >= {kb} ({vb})"),
            ));
        }
    }
}

/// `sandbox`: таблица с известными подключами корректных типов.
fn validate_sandbox(table: &Table, issues: &mut Vec<ConfigIssue>) {
    let Some(v) = table.get("sandbox") else { return };
    let Some(t) = v.as_table() else {
        issues.push(type_err("sandbox", "таблица", v));
        return;
    };
    for (key, value) in t {
        let path = format!("sandbox.{key}");
        match key.as_str() {
            "enabled" | "allow_network" => {
                if !value.is_bool() {
                    issues.push(type_err(path, "булево", value));
                }
            }
            "writable_paths" => {
                let ok = value
                    .as_array()
                    .is_some_and(|arr| arr.iter().all(Value::is_str));
                if !ok {
                    issues.push(type_err(path, "массив строк", value));
                }
            }
            _ => issues.push(ConfigIssue::warn(
                path,
                format!("неизвестный ключ sandbox «{key}»; известные: {}", SANDBOX_KEYS.join(", ")),
            )),
        }
    }
}

/// `permission.mode`: один из [`PERMISSION_MODES`].
fn validate_permission(table: &Table, issues: &mut Vec<ConfigIssue>) {
    let Some(v) = table.get("permission") else { return };
    let Some(t) = v.as_table() else {
        issues.push(type_err("permission", "таблица", v));
        return;
    };
    for key in t.keys() {
        if key != "mode" {
            issues.push(ConfigIssue::warn(
                format!("permission.{key}"),
                "неизвестный ключ permission",
            ));
        }
    }
    if let Some(mode) = t.get("mode") {
        match mode.as_str() {
            Some(m) if PERMISSION_MODES.contains(&m) => {}
            Some(m) => issues.push(ConfigIssue::error(
                "permission.mode",
                format!("неизвестный режим «{m}»; допустимы: {}", PERMISSION_MODES.join(", ")),
            )),
            None => issues.push(type_err("permission.mode", "строка", mode)),
        }
    }
}

/// `permission_rules`: массив таблиц `{decision, pattern}`.
fn validate_permission_rules(table: &Table, issues: &mut Vec<ConfigIssue>) {
    let Some(v) = table.get("permission_rules") else { return };
    let Some(arr) = v.as_array() else {
        issues.push(type_err("permission_rules", "массив таблиц", v));
        return;
    };
    for (i, rule) in arr.iter().enumerate() {
        let base = format!("permission_rules[{i}]");
        let Some(rt) = rule.as_table() else {
            issues.push(type_err(base, "таблица", rule));
            continue;
        };
        match rt.get("decision").and_then(Value::as_str) {
            Some(d) if RULE_DECISIONS.contains(&d) => {}
            Some(d) => issues.push(ConfigIssue::error(
                format!("{base}.decision"),
                format!("недопустимое решение «{d}»; допустимы: {}", RULE_DECISIONS.join(", ")),
            )),
            None => issues.push(ConfigIssue::error(
                format!("{base}.decision"),
                "обязательное строковое поле decision ∈ {allow, ask, deny}",
            )),
        }
        match rt.get("pattern").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => {}
            _ => issues.push(ConfigIssue::error(
                format!("{base}.pattern"),
                "обязательное непустое строковое поле pattern",
            )),
        }
    }
}

/// `mcp_servers`: таблица таблиц; каждому серверу нужен `command` или `url`.
fn validate_mcp_servers(table: &Table, issues: &mut Vec<ConfigIssue>) {
    let Some(v) = table.get("mcp_servers") else { return };
    let Some(t) = v.as_table() else {
        issues.push(type_err("mcp_servers", "таблица", v));
        return;
    };
    for (name, server) in t {
        let path = format!("mcp_servers.{name}");
        let Some(st) = server.as_table() else {
            issues.push(type_err(path, "таблица", server));
            continue;
        };
        let has_command = st
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| !c.is_empty());
        let has_url = st
            .get("url")
            .and_then(Value::as_str)
            .is_some_and(|u| !u.is_empty());
        if !has_command && !has_url {
            issues.push(ConfigIssue::error(
                path,
                "MCP-серверу нужен непустой «command» (stdio) или «url» (http)",
            ));
        }
    }
}

/// `hooks`: массив таблиц `{event, command}`; нестандартное событие — Warn.
fn validate_hooks(table: &Table, issues: &mut Vec<ConfigIssue>) {
    let Some(v) = table.get("hooks") else { return };
    let Some(arr) = v.as_array() else {
        issues.push(type_err("hooks", "массив таблиц", v));
        return;
    };
    for (i, hook) in arr.iter().enumerate() {
        let base = format!("hooks[{i}]");
        let Some(ht) = hook.as_table() else {
            issues.push(type_err(base, "таблица", hook));
            continue;
        };
        match ht.get("event").and_then(Value::as_str) {
            Some(ev) if !HOOK_EVENTS.contains(&ev) => issues.push(ConfigIssue::warn(
                format!("{base}.event"),
                format!("нестандартное событие «{ev}»; известные: {}", HOOK_EVENTS.join(", ")),
            )),
            Some(_) => {}
            None => issues.push(ConfigIssue::error(
                format!("{base}.event"),
                "обязательное строковое поле event",
            )),
        }
        match ht.get("command").and_then(Value::as_str) {
            Some(c) if !c.trim().is_empty() => {}
            _ => issues.push(ConfigIssue::error(
                format!("{base}.command"),
                "обязательное непустое строковое поле command",
            )),
        }
    }
}

/// `web_allowed_domains`: массив строк, похожих на домены (Warn при сомнении).
fn validate_web_domains(table: &Table, issues: &mut Vec<ConfigIssue>) {
    let Some(v) = table.get("web_allowed_domains") else { return };
    let Some(arr) = v.as_array() else {
        issues.push(type_err("web_allowed_domains", "массив строк", v));
        return;
    };
    // Паттерн — константа, проверенная тестом domain_regex_compiles; ошибка недостижима.
    let Ok(re) = Regex::new(DOMAIN_RE) else { return };
    for (i, item) in arr.iter().enumerate() {
        let path = format!("web_allowed_domains[{i}]");
        match item.as_str() {
            Some(d) if re.is_match(d) => {}
            Some(_) => issues.push(ConfigIssue::warn(path, "строка не похожа на доменное имя")),
            None => issues.push(type_err(path, "строка", item)),
        }
    }
}

/// `notify`: булево или таблица `{enabled: bool}`.
fn validate_notify(table: &Table, issues: &mut Vec<ConfigIssue>) {
    let Some(v) = table.get("notify") else { return };
    match v {
        Value::Boolean(_) => {}
        Value::Table(t) => match t.get("enabled") {
            Some(enabled) if !enabled.is_bool() => {
                issues.push(type_err("notify.enabled", "булево", enabled));
            }
            _ => {}
        },
        _ => issues.push(type_err("notify", "булево или таблица", v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Разобрать слой из строки (тестовый хелпер).
    fn layer(name: &str, toml_text: &str) -> ConfigLayer {
        ConfigLayer::parse(name, toml_text).unwrap()
    }

    /// Достать значение по точечному пути (`mcp_servers.fs.command`).
    fn get<'v>(v: &'v Value, path: &str) -> Option<&'v Value> {
        let mut cur = v;
        for part in path.split('.') {
            cur = cur.get(part)?;
        }
        Some(cur)
    }

    /// Есть ли замечание с данным путём и серьёзностью.
    fn has(issues: &[ConfigIssue], path: &str, sev: Severity) -> bool {
        issues.iter().any(|i| i.path == path && i.severity == sev)
    }

    /// Временный каталог для файловых тестов.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("theseus_config_layers_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn merge_priority_last_layer_wins() {
        let layers = vec![
            layer("defaults", "model = \"base\"\ncontext_limit_tokens = 4096"),
            layer("global", "model = \"gpt-global\""),
            layer("workspace", "model = \"qwen-ws\""),
            layer("cli", "model = \"cli-model\""),
        ];
        let merged = merge(&layers);
        assert_eq!(get(&merged, "model").and_then(Value::as_str), Some("cli-model"));
        // Ключ, не тронутый верхними слоями, доезжает из нижнего.
        assert_eq!(get(&merged, "context_limit_tokens").and_then(Value::as_integer), Some(4096));
    }

    #[test]
    fn merge_tables_recursively() {
        let a = layer("a", "[mcp_servers.fs]\ncommand = \"fsd\"\n[mcp_servers.web]\ncommand = \"webd\"");
        let b = layer("b", "[mcp_servers.fs]\nurl = \"http://localhost\"");
        let merged = merge(&[a, b]);
        // command сохранился из нижнего слоя, url добавился сверху.
        assert_eq!(get(&merged, "mcp_servers.fs.command").and_then(Value::as_str), Some("fsd"));
        assert_eq!(get(&merged, "mcp_servers.fs.url").and_then(Value::as_str), Some("http://localhost"));
        assert!(get(&merged, "mcp_servers.web").is_some());
    }

    #[test]
    fn merge_arrays_overwrite_not_concat() {
        let a = layer("a", "web_allowed_domains = [\"a.com\", \"b.com\"]");
        let b = layer("b", "web_allowed_domains = [\"c.com\"]");
        let merged = merge(&[a, b]);
        let arr = get(&merged, "web_allowed_domains").and_then(Value::as_array).unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].as_str(), Some("c.com"));
    }

    #[test]
    fn merge_scalar_replaces_table() {
        let a = layer("a", "[sandbox]\nenabled = true");
        let b = layer("b", "sandbox = false");
        let merged = merge(&[a, b]);
        assert_eq!(get(&merged, "sandbox").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn merge_empty_gives_empty_table() {
        let merged = merge(&[]);
        assert!(merged.as_table().is_some_and(Table::is_empty));
    }

    #[test]
    fn layer_order_matters() {
        let a = layer("a", "model = \"aaa\"");
        let b = layer("b", "model = \"bbb\"");
        let ab = merge(&[a.clone(), b.clone()]);
        let ba = merge(&[b, a]);
        assert_eq!(get(&ab, "model").and_then(Value::as_str), Some("bbb"));
        assert_eq!(get(&ba, "model").and_then(Value::as_str), Some("aaa"));
    }

    #[test]
    fn unknown_top_level_key_is_warn() {
        let cfg = layer("t", "modle = \"oops\"").toml_table;
        let issues = validate(&cfg);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Warn);
        assert_eq!(issues[0].path, "modle");
        assert!(issues[0].message.contains("modle"));
    }

    #[test]
    fn wrong_types_are_errors() {
        let cfg = layer("t", "model = 42\ncontext_limit_tokens = \"lots\"").toml_table;
        let issues = validate(&cfg);
        assert!(has(&issues, "model", Severity::Error));
        assert!(has(&issues, "context_limit_tokens", Severity::Error));
    }

    #[test]
    fn compact_thresholds_validated() {
        // Корректная тройка — без замечаний.
        let ok = layer("t", "compact_l1 = 0.5\ncompact_l2 = 0.7\ncompact_l3 = 0.9").toml_table;
        assert!(validate(&ok).is_empty());
        // Нарушен порядок l1 >= l2.
        let bad = layer("t", "compact_l1 = 0.8\ncompact_l2 = 0.7\ncompact_l3 = 0.9").toml_table;
        assert!(has(&validate(&bad), "compact_l1", Severity::Error));
        // Выход за верхнюю границу и ноль вне диапазона (0.0, 1.0).
        let high = layer("t", "compact_l3 = 1.5").toml_table;
        assert!(has(&validate(&high), "compact_l3", Severity::Error));
        let zero = layer("t", "compact_l1 = 0.0").toml_table;
        assert!(has(&validate(&zero), "compact_l1", Severity::Error));
    }

    #[test]
    fn permission_mode_whitelist() {
        for mode in PERMISSION_MODES {
            let cfg = layer("t", &format!("[permission]\nmode = \"{mode}\"")).toml_table;
            assert!(validate(&cfg).is_empty(), "режим {mode} должен быть валиден");
        }
        let bad = layer("t", "[permission]\nmode = \"yoloooo\"").toml_table;
        assert!(has(&validate(&bad), "permission.mode", Severity::Error));
    }

    #[test]
    fn context_limit_minimum() {
        let small = layer("t", "context_limit_tokens = 2048").toml_table;
        assert!(has(&validate(&small), "context_limit_tokens", Severity::Error));
        // Граничное значение — валидно.
        let border = layer("t", "context_limit_tokens = 4096").toml_table;
        assert!(validate(&border).is_empty());
    }

    #[test]
    fn permission_rules_checked() {
        let bad = layer(
            "t",
            "[[permission_rules]]\ndecision = \"maybe\"\npattern = \"Bash(*)\"\n\
             [[permission_rules]]\npattern = \"Read(*)\"",
        )
        .toml_table;
        let issues = validate(&bad);
        assert!(has(&issues, "permission_rules[0].decision", Severity::Error));
        assert!(has(&issues, "permission_rules[1].decision", Severity::Error));
        let good = layer("t", "[[permission_rules]]\ndecision = \"allow\"\npattern = \"Bash(cargo *)\"").toml_table;
        assert!(validate(&good).is_empty());
    }

    #[test]
    fn mcp_and_hooks_checked() {
        let cfg = layer("t", "[mcp_servers.broken]\n[[hooks]]\nevent = \"OnMoon\"\ncommand = \"\"").toml_table;
        let issues = validate(&cfg);
        // Сервер без command/url — ошибка.
        assert!(issues.iter().any(|i| i.path.starts_with("mcp_servers.broken") && i.severity == Severity::Error));
        // Нестандартное событие — предупреждение, пустая команда — ошибка.
        assert!(has(&issues, "hooks[0].event", Severity::Warn));
        assert!(has(&issues, "hooks[0].command", Severity::Error));
    }

    #[test]
    fn web_domains_shape() {
        assert!(Regex::new(DOMAIN_RE).is_ok());
        let cfg = layer("t", "web_allowed_domains = [\"example.com\", \"*.ok.org\", \"bad domain!\"]").toml_table;
        let issues = validate(&cfg);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Warn);
        assert!(issues[0].path.contains("[2]"));
    }

    #[test]
    fn validate_rejects_non_table_root() {
        let issues = validate(&Value::Integer(1));
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Error);
        assert_eq!(issues[0].path, "$");
    }

    #[test]
    fn parse_rejects_scalar_document() {
        assert!(ConfigLayer::parse("bad", "42").is_err());
    }

    #[test]
    fn from_file_roundtrip() {
        let dir = temp_dir("from_file");
        let path = dir.join("cfg.toml");
        std::fs::write(&path, "model = \"from-file\"").unwrap();
        let l = ConfigLayer::from_file("test", &path).unwrap();
        assert_eq!(l.name, "test");
        assert_eq!(get(&l.toml_table, "model").and_then(Value::as_str), Some("from-file"));
        // Отсутствующий файл — ошибка для from_file (в отличие от load_layered).
        assert!(ConfigLayer::from_file("x", &dir.join("absent.toml")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_layered_missing_files_uses_defaults() {
        let dir = temp_dir("missing");
        let (cfg, issues) = load_layered(&dir.join("g.toml"), &dir.join("w.toml"), &[]).unwrap();
        // Умолчания валидны и доступны.
        assert!(issues.is_empty());
        assert!(get(&cfg, "model").is_some());
        assert_eq!(get(&cfg, "permission.mode").and_then(Value::as_str), Some("ask"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_layered_full_chain() {
        let dir = temp_dir("chain");
        let global = dir.join("global.toml");
        let ws = dir.join("workspace.toml");
        std::fs::write(&global, "model = \"gm\"\nbase_url = \"http://g\"\n[mcp_servers.a]\ncommand = \"a-cmd\"").unwrap();
        std::fs::write(&ws, "model = \"wm\"\n[mcp_servers.a]\nurl = \"http://a\"").unwrap();
        let (cfg, issues) =
            load_layered(&global, &ws, &[("model", "cli-model"), ("context_limit_tokens", "65536")]).unwrap();
        assert!(issues.is_empty());
        // CLI > workspace > global > defaults.
        assert_eq!(get(&cfg, "model").and_then(Value::as_str), Some("cli-model"));
        assert_eq!(get(&cfg, "base_url").and_then(Value::as_str), Some("http://g"));
        assert_eq!(get(&cfg, "context_limit_tokens").and_then(Value::as_integer), Some(65536));
        // Глубокий мердж mcp_servers.a: command из global + url из workspace.
        assert_eq!(get(&cfg, "mcp_servers.a.command").and_then(Value::as_str), Some("a-cmd"));
        assert_eq!(get(&cfg, "mcp_servers.a.url").and_then(Value::as_str), Some("http://a"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_layered_bad_toml_is_error() {
        let dir = temp_dir("bad_toml");
        let global = dir.join("global.toml");
        std::fs::write(&global, "model = [unclosed").unwrap();
        let res = load_layered(&global, &dir.join("w.toml"), &[]);
        assert!(res.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_overrides_dotted_and_typed() {
        let dir = temp_dir("cli");
        let (cfg, issues) = load_layered(
            &dir.join("g.toml"),
            &dir.join("w.toml"),
            &[("permission.mode", "yolo"), ("compact_l1", "0.55"), ("sandbox.enabled", "true")],
        )
        .unwrap();
        assert!(issues.is_empty());
        assert_eq!(get(&cfg, "permission.mode").and_then(Value::as_str), Some("yolo"));
        assert!(get(&cfg, "sandbox.enabled").and_then(Value::as_bool).unwrap());
        let f = get(&cfg, "compact_l1").and_then(Value::as_float).unwrap();
        assert!((f - 0.55).abs() < f64::EPSILON);
        // Пустой путь оверрайда — ошибка.
        assert!(load_layered(&dir.join("g.toml"), &dir.join("w.toml"), &[("", "v")]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
