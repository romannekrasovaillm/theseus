//! Реестр типов субагентов (образец: subagents из Claude Code и agents из Codex).
//!
//! Субагент — изолированный прогон агент-цикла с собственным системным промптом,
//! суженным тулсетом и отдельным бюджетом. Глубина делегирования — 1: субагент
//! не получает инструмент `task` и не может породить следующий уровень
//! (урок обзора тройки: рекурсивное делегирование размывает и контекст, и бюджет).
//!
//! Модуль задаёт три вещи:
//!
//! 1. [`AgentSpec`] — декларативная спецификация типа субагента: имя, назначение,
//!    системный промпт, разрешённые инструменты, потолок ходов и флаг readonly.
//!    Readonly-гарантия реализуется на уровне тулсета: спека с `readonly = true`
//!    не может включать пишущие инструменты ([`WRITE_TOOLS`]) — это проверяется
//!    в [`AgentSpec::validate`] ещё до запуска.
//! 2. [`AgentRegistry`] — реестр спек с диагностикой опечаток: запрос неизвестного
//!    типа возвращает ошибку с подсказкой ближайших имён (префикс/подстрока плюс Левенштейн).
//! 3. [`AgentBudget`] + [`BudgetGuard`] — бюджет прогона (ходы/токены/секунды)
//!    и его страж; [`AgentResult`] — компактный итог для родительского контекста.
//!
//! Встроенные типы ([`builtin_specs`]): `explore` — поиск по коду, `plan` —
//! архитектурный план, `code_review` — ревью diff'а (все readonly), `test_runner` —
//! прогон тестов (нужен `bash`, поэтому не readonly, но промпт ограничивает его
//! командами, не меняющими исходники).
//!
//! ```
//! use theseus::agents::AgentRegistry;
//!
//! let registry = AgentRegistry::with_builtins();
//! let spec = registry.get("explore")?;
//! assert!(spec.readonly);
//! # Ok::<(), theseus::agents::AgentError>(())
//! ```

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};

/// Все инструменты, известные харнессу (зеркало `crate::tools::tool_specs`).
///
/// Спека субагента может ссылаться только на имена из этого набора —
/// проверяется в [`AgentSpec::validate`]. Список держится синхронным
/// с `src/tools.rs`; рассинхрон всплывёт в тесте `builtin_specs_are_valid`.
pub const KNOWN_TOOLS: &[&str] = &[
    "read_file", "write_file", "edit_file", "list_files", "grep", "bash",
    "task_output", "task_stop", "skill", "memory_write", "memory_search",
    "web_fetch", "web_search", "exit_plan_mode", "todo_write", "task", "finish",
];

/// Инструменты, способные менять состояние: файлы, память, todo-список,
/// фоновые задачи, порождение агентов, произвольные команды.
/// Readonly-спека не может включать ни одного из них.
pub const WRITE_TOOLS: &[&str] = &[
    "write_file", "edit_file", "bash", "memory_write", "todo_write", "task_stop", "task",
];

/// Сколько подсказок имён максимум прилагается к ошибке неизвестного агента.
const MAX_SUGGESTIONS: usize = 3;

/// Системный промпт субагента `explore`.
const EXPLORE_PROMPT: &str = "You are a fast, read-only codebase exploration agent. \
Answer the user's question by searching and reading files; you cannot modify anything. \
Prefer grep/list_files to narrow down before read_file. \
Reply with a concise factual answer with file:line references.";

/// Системный промпт субагента `plan`.
const PLAN_PROMPT: &str = "You are a software architect producing an implementation plan. \
Read the relevant code, then output a step-by-step plan: concrete files, functions and \
commands, each step verifiable. Call out trade-offs and risks briefly. \
You are read-only: no edits, no shell.";

/// Системный промпт субагента `code_review`.
const CODE_REVIEW_PROMPT: &str = "You are a strict code reviewer. You receive a diff \
(or files to inspect) and report findings ordered by severity: bugs, regressions, \
security issues, style deviations. Cite file:line for every finding. \
You are read-only. If the diff is clean, say so plainly instead of inventing nits.";

/// Системный промпт субагента `test_runner`.
const TEST_RUNNER_PROMPT: &str = "You run the project's checks and report results faithfully. \
Use bash only for commands that do not modify sources: build, test, lint, status. \
Never edit files to make tests pass. On failure report the exact failing command \
and the relevant excerpt of its output; on success say what was verified.";

/// Какой именно лимит бюджета исчерпан.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BudgetKind {
    /// Ходы агент-цикла (запросы к модели).
    Turns,
    /// Суммарные токены (prompt + completion).
    Tokens,
    /// Настенное время прогона, секунды.
    WallClock,
}

impl BudgetKind {
    /// Стабильное имя лимита для логов и сообщений об ошибках.
    pub fn as_str(&self) -> &'static str {
        match self {
            BudgetKind::Turns => "turns",
            BudgetKind::Tokens => "tokens",
            BudgetKind::WallClock => "wall_clock",
        }
    }
}

impl fmt::Display for BudgetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Ошибки реестра агентов и бюджета.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    /// Запрошен неизвестный тип агента; прилагаются подсказки и полный список.
    UnknownAgent {
        /// Имя, которое запросили.
        name: String,
        /// Ближайшие по имени типы (может быть пусто).
        suggestions: Vec<String>,
        /// Все зарегистрированные типы.
        available: Vec<String>,
    },
    /// Спека не прошла валидацию; собраны ВСЕ проблемы, а не первая.
    InvalidSpec {
        /// Имя спеки (может быть пустым — это тоже проблема).
        name: String,
        /// Человекочитаемый список нарушений.
        problems: Vec<String>,
    },
    /// Тип с таким именем уже зарегистрирован.
    DuplicateName {
        /// Дублирующееся имя.
        name: String,
    },
    /// Бюджет прогона исчерпан: какой лимит, его значение и факт на момент превышения.
    BudgetExceeded {
        /// Какой лимит сработал.
        kind: BudgetKind,
        /// Значение лимита.
        limit: u64,
        /// Фактическое использование на момент превышения.
        used: u64,
    },
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::UnknownAgent { name, suggestions, available } => {
                if suggestions.is_empty() {
                    write!(f, "неизвестный тип агента «{name}»; доступные типы: {}", available.join(", "))
                } else {
                    write!(f, "неизвестный тип агента «{name}»; возможно, вы имели в виду: {}?", suggestions.join(", "))
                }
            }
            AgentError::InvalidSpec { name, problems } => {
                write!(f, "невалидная спецификация агента «{name}»: {}", problems.join("; "))
            }
            AgentError::DuplicateName { name } => {
                write!(f, "тип агента «{name}» уже зарегистрирован")
            }
            AgentError::BudgetExceeded { kind, limit, used } => {
                write!(f, "бюджет агента исчерпан ({kind}): лимит {limit}, использовано {used}")
            }
        }
    }
}

impl std::error::Error for AgentError {}

/// Декларативная спецификация типа субагента.
///
/// Аналог frontmatter `name/description/tools` из Claude Code subagents,
/// плюс потолок ходов и readonly-флаг. Сериализуется — спеки можно
/// перечитывать из конфигурации (TOML/JSON) без смены кода.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    /// Уникальное имя типа: строчный snake_case (`explore`, `code_review`).
    pub name: String,
    /// Одна строка о назначении — показывается родительскому агенту при выборе типа.
    pub purpose: String,
    /// Системный промпт изолированного прогона.
    pub system_prompt: String,
    /// Разрешённые инструменты; каждый обязан быть из [`KNOWN_TOOLS`].
    pub allowed_tools: Vec<String>,
    /// Потолок ходов агент-цикла (>= 1).
    pub max_turns: u32,
    /// true — спека не может включать пишущие инструменты ([`WRITE_TOOLS`]).
    pub readonly: bool,
}

impl AgentSpec {
    /// Собирает спеку из полей; валидация — отдельным шагом ([`AgentSpec::validate`]).
    pub fn new(
        name: impl Into<String>,
        purpose: impl Into<String>,
        system_prompt: impl Into<String>,
        allowed_tools: &[&str],
        max_turns: u32,
        readonly: bool,
    ) -> Self {
        Self {
            name: name.into(),
            purpose: purpose.into(),
            system_prompt: system_prompt.into(),
            allowed_tools: allowed_tools.iter().map(ToString::to_string).collect(),
            max_turns,
            readonly,
        }
    }

    /// Полная проверка инвариантов; возвращает ВСЕ найденные проблемы разом.
    ///
    /// Проверяется: непустое snake_case имя, непустые `purpose` и `system_prompt`,
    /// `max_turns >= 1`, непустой тулсет без дубликатов, все инструменты —
    /// из [`KNOWN_TOOLS`], а у readonly-спеки — вне [`WRITE_TOOLS`].
    pub fn validate(&self) -> Result<(), AgentError> {
        let mut problems = Vec::new();
        let name = self.name.trim();
        if name.is_empty() {
            problems.push("пустое имя".to_string());
        } else if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            problems.push(format!("имя «{name}» должно быть строчным snake_case"));
        }
        if self.purpose.trim().is_empty() {
            problems.push("пустое назначение (purpose)".to_string());
        }
        if self.system_prompt.trim().is_empty() {
            problems.push("пустой системный промпт".to_string());
        }
        if self.max_turns == 0 {
            problems.push("max_turns должен быть >= 1".to_string());
        }
        if self.allowed_tools.is_empty() {
            problems.push("спека без инструментов бесполезна".to_string());
        }
        let mut seen = std::collections::BTreeSet::new();
        for tool in &self.allowed_tools {
            if !seen.insert(tool.as_str()) {
                problems.push(format!("инструмент «{tool}» указан дважды"));
            }
            if !KNOWN_TOOLS.contains(&tool.as_str()) {
                problems.push(format!(
                    "неизвестный инструмент «{tool}»; известные: {}",
                    KNOWN_TOOLS.join(", ")
                ));
            } else if self.readonly && WRITE_TOOLS.contains(&tool.as_str()) {
                problems.push(format!(
                    "readonly-спека не может содержать пишущий инструмент «{tool}»"
                ));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(AgentError::InvalidSpec {
                name: self.name.clone(),
                problems,
            })
        }
    }

    /// Разрешён ли инструмент этой спеке.
    pub fn allows_tool(&self, tool: &str) -> bool {
        self.allowed_tools.iter().any(|t| t == tool)
    }
}

/// Встроенные типы субагентов по образцу тройки харнессов.
///
/// `explore`, `plan`, `code_review` — readonly (урок обзора: read-only гарантия
/// на уровне тулсета, а не на честном слове промпта). `test_runner` получает
/// `bash`, поэтому readonly быть не может, но его промпт ограничивает команды
/// не-мутирующими (build/test/lint/status).
pub fn builtin_specs() -> Vec<AgentSpec> {
    vec![
        AgentSpec::new(
            "explore",
            "Быстрый поиск по коду: ответы о кодовой базе со ссылками file:line (readonly).",
            EXPLORE_PROMPT,
            &["read_file", "list_files", "grep"],
            12, true,
        ),
        AgentSpec::new(
            "plan",
            "Архитектурный план изменений: проверяемые шаги по файлам и командам (readonly).",
            PLAN_PROMPT,
            &["read_file", "list_files", "grep", "web_fetch", "web_search"],
            25, true,
        ),
        AgentSpec::new(
            "code_review",
            "Ревью diff'а: находки по убыванию severity со ссылками на строки (readonly).",
            CODE_REVIEW_PROMPT,
            &["read_file", "list_files", "grep"],
            15, true,
        ),
        AgentSpec::new(
            "test_runner",
            "Прогон сборки/тестов/линтов и честный отчёт (bash без права правки исходников).",
            TEST_RUNNER_PROMPT,
            &["bash", "read_file", "list_files", "grep", "task_output", "task_stop"],
            30, false,
        ),
    ]
}

/// Реестр типов субагентов: имена уникальны, спеки валидны.
///
/// Итерация по именам упорядочена (BTreeMap) — стабильные списки в подсказках.
#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    specs: BTreeMap<String, AgentSpec>,
}

impl AgentRegistry {
    /// Пустой реестр; наполнение — через [`AgentRegistry::register`].
    pub fn new() -> Self {
        Self {
            specs: BTreeMap::new(),
        }
    }

    /// Реестр с четырьмя встроенными типами из [`builtin_specs`].
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        for spec in builtin_specs() {
            debug_assert!(spec.validate().is_ok(), "builtin-спека «{}» невалидна", spec.name);
            registry.specs.insert(spec.name.clone(), spec);
        }
        registry
    }

    /// Валидирует и регистрирует спеку. Ошибки: невалидная спека или дубликат имени.
    pub fn register(&mut self, spec: AgentSpec) -> Result<(), AgentError> {
        spec.validate()?;
        if self.specs.contains_key(&spec.name) {
            return Err(AgentError::DuplicateName { name: spec.name });
        }
        self.specs.insert(spec.name.clone(), spec);
        Ok(())
    }

    /// Точный поиск типа (регистр и обрамляющие пробелы игнорируются).
    ///
    /// При промахе — [`AgentError::UnknownAgent`] с подсказкой ближайших имён
    /// (префикс/подстрока, затем расстояние Левенштейна) либо с полным списком
    /// доступных типов, если близких нет.
    pub fn get(&self, name: &str) -> Result<&AgentSpec, AgentError> {
        let key = name.trim().to_lowercase();
        if let Some(spec) = self.specs.get(key.as_str()) {
            return Ok(spec);
        }
        let available: Vec<String> = self.specs.keys().cloned().collect();
        let suggestions = suggest_names(&key, &available, MAX_SUGGESTIONS);
        Err(AgentError::UnknownAgent { name: name.to_string(), suggestions, available })
    }

    /// Необязательный вариант поиска — без диагностики опечаток.
    pub fn find(&self, name: &str) -> Option<&AgentSpec> {
        let key = name.trim().to_lowercase();
        self.specs.get(key.as_str())
    }

    /// Имена всех зарегистрированных типов, отсортированные.
    pub fn names(&self) -> Vec<&str> {
        self.specs.keys().map(String::as_str).collect()
    }

    /// Число зарегистрированных типов.
    pub fn len(&self) -> usize {
        self.specs.len()
    }

    /// Пуст ли реестр.
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }
}

/// Бюджет прогона субагента: три независимых лимита.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBudget {
    /// Максимум ходов агент-цикла.
    pub max_turns: u32,
    /// Максимум суммарных токенов (prompt + completion).
    pub max_tokens: u64,
    /// Максимум настенного времени, секунды.
    pub max_sec: u64,
}

impl AgentBudget {
    /// Явные лимиты.
    pub const fn new(max_turns: u32, max_tokens: u64, max_sec: u64) -> Self {
        Self {
            max_turns,
            max_tokens,
            max_sec,
        }
    }

    /// Бюджет под конкретную спеку: ходы — из `spec.max_turns`, остальное — параметрами.
    pub fn for_spec(spec: &AgentSpec, max_tokens: u64, max_sec: u64) -> Self {
        Self::new(spec.max_turns, max_tokens, max_sec)
    }

    /// Практически безлимитный бюджет (для главного агента и отладки).
    pub const fn unlimited() -> Self {
        Self::new(u32::MAX, u64::MAX, u64::MAX)
    }
}

/// Бюджет по умолчанию для встроенного типа: ходы — из спеки, токены и настенное
/// время — под «вес» задачи. plan/code_review читают много кода (каждый ход
/// тащит накопленную историю — токены растут квадратично по ходам), test_runner
/// гоняет длинные прогоны тестов. Живой прогон 20.07: plan с лимитом 200k
/// оборвался на 10-м ходу (235k), не выдав результата.
pub fn default_budget(spec: &AgentSpec) -> AgentBudget {
    let (tokens, sec) = match spec.name.as_str() {
        "plan" => (500_000, 900),
        "code_review" => (300_000, 600),
        "test_runner" => (400_000, 900),
        _ => (250_000, 600),
    };
    AgentBudget::for_spec(spec, tokens, sec)
}

impl Default for AgentBudget {
    /// Умеренные лимиты по умолчанию: 20 ходов, 200k токенов, 10 минут.
    fn default() -> Self {
        Self::new(20, 200_000, 600)
    }
}

/// Страж бюджета: накапливает фактическое использование и сверяет с лимитами.
///
/// Разделение обязанностей: [`BudgetGuard::consume`] вызывается на каждом ходу
/// и проверяет счётные лимиты (ходы, токены); [`BudgetGuard::check`] не меняет
/// состояние и проверяет все три лимита, включая настенные часы — его зовут
/// между ходами и перед стартом очередного обращения к модели.
///
/// Учёт обновляется даже при превышении: факт нужен для честного [`AgentResult`].
#[derive(Debug, Clone)]
pub struct BudgetGuard {
    budget: AgentBudget,
    used_turns: u32,
    used_tokens: u64,
    started: Instant,
}

impl BudgetGuard {
    /// Страж с отсчётом времени от текущего момента.
    pub fn new(budget: AgentBudget) -> Self {
        Self::with_start(budget, Instant::now())
    }

    /// Страж с явной точкой отсчёта (восстановление из сохранённого состояния, тесты).
    pub fn with_start(budget: AgentBudget, started: Instant) -> Self {
        Self {
            budget,
            used_turns: 0,
            used_tokens: 0,
            started,
        }
    }

    /// Лимиты, под которыми работает страж.
    pub fn budget(&self) -> AgentBudget {
        self.budget
    }

    /// Сколько ходов уже учтено.
    pub fn used_turns(&self) -> u32 {
        self.used_turns
    }

    /// Сколько токенов уже учтено.
    pub fn used_tokens(&self) -> u64 {
        self.used_tokens
    }

    /// Время с точки отсчёта.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Учитывает один ход агента (`tokens` — сумма prompt+completion за ход)
    /// и проверяет счётные лимиты. Превышение — строго больше лимита.
    pub fn consume(&mut self, tokens: u64) -> Result<(), AgentError> {
        self.used_turns = self.used_turns.saturating_add(1);
        self.used_tokens = self.used_tokens.saturating_add(tokens);
        self.check_counts()
    }

    /// Проверяет все три лимита, включая настенные часы. Состояние не меняется.
    pub fn check(&self) -> Result<(), AgentError> {
        self.check_counts()?;
        let elapsed = self.elapsed().as_secs();
        if elapsed > self.budget.max_sec {
            return Err(AgentError::BudgetExceeded {
                kind: BudgetKind::WallClock,
                limit: self.budget.max_sec,
                used: elapsed,
            });
        }
        Ok(())
    }

    /// Счётные лимиты (ходы, токены) — общая часть `consume` и `check`.
    fn check_counts(&self) -> Result<(), AgentError> {
        if self.used_turns > self.budget.max_turns {
            return Err(AgentError::BudgetExceeded {
                kind: BudgetKind::Turns,
                limit: u64::from(self.budget.max_turns),
                used: u64::from(self.used_turns),
            });
        }
        if self.used_tokens > self.budget.max_tokens {
            return Err(AgentError::BudgetExceeded {
                kind: BudgetKind::Tokens,
                limit: self.budget.max_tokens,
                used: self.used_tokens,
            });
        }
        Ok(())
    }
}

/// Компактный итог прогона субагента — то, что уходит в родительский контекст.
///
/// Полный транскрипт субагента родителю не нужен (урок тройки: изоляция
/// контекста); достаточно выжимки и признака усечения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentResult {
    /// Выжимка результата (финальный ответ субагента, сжатый при необходимости).
    pub summary: String,
    /// Фактически израсходовано ходов.
    pub turns: u32,
    /// Фактически израсходовано токенов.
    pub tokens: u64,
    /// true — прогон оборван по бюджету, результат может быть неполным.
    pub truncated: bool,
}

impl AgentResult {
    /// Явная сборка итога.
    pub fn new(summary: impl Into<String>, turns: u32, tokens: u64, truncated: bool) -> Self {
        Self {
            summary: summary.into(),
            turns,
            tokens,
            truncated,
        }
    }

    /// Итог из состояния стража: фактические ходы и токены переносятся как есть.
    pub fn from_guard(summary: impl Into<String>, guard: &BudgetGuard, truncated: bool) -> Self {
        Self::new(summary, guard.used_turns(), guard.used_tokens(), truncated)
    }
}

/// Классическое расстояние Левенштейна (посимвольное, unicode-safe).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    // ДП двумя строками: prev — предыдущая строка матрицы, cur — текущая.
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0_usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// До `limit` ближайших имён: префикс/подстрока считается расстоянием 1,
/// дальше — по Левенштейну; порог — треть длины запроса (минимум 1, максимум 3).
fn suggest_names(name: &str, candidates: &[String], limit: usize) -> Vec<String> {
    let needle = name.to_lowercase();
    let threshold = (needle.chars().count() / 3).clamp(1, 3);
    let mut scored: Vec<(usize, &String)> = candidates
        .iter()
        .map(|cand| {
            let lower = cand.to_lowercase();
            let dist = levenshtein(&needle, &lower);
            let dist = if lower.contains(&needle) || needle.contains(&lower) {
                dist.min(1)
            } else {
                dist
            };
            (dist, cand)
        })
        .filter(|(dist, _)| *dist <= threshold)
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    scored.truncate(limit);
    scored.into_iter().map(|(_, cand)| cand.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_by_name(name: &str) -> AgentSpec {
        builtin_specs().into_iter().find(|s| s.name == name).unwrap()
    }

    #[test]
    fn builtins_have_expected_names() {
        let registry = AgentRegistry::with_builtins();
        assert_eq!(registry.len(), 4);
        assert_eq!(registry.names(), ["code_review", "explore", "plan", "test_runner"]);
    }

    #[test]
    fn builtin_specs_are_valid() {
        for spec in builtin_specs() {
            spec.validate().unwrap();
            assert!(!spec.system_prompt.is_empty());
            assert!(spec.max_turns >= 1);
        }
    }

    #[test]
    fn builtin_tools_are_known() {
        for spec in builtin_specs() {
            for tool in &spec.allowed_tools {
                assert!(
                    KNOWN_TOOLS.contains(&tool.as_str()),
                    "спека «{}» ссылается на неизвестный инструмент «{tool}»",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn readonly_specs_exclude_write_tools() {
        for name in ["explore", "plan", "code_review"] {
            let spec = spec_by_name(name);
            assert!(spec.readonly, "{name} должен быть readonly");
            for tool in &spec.allowed_tools {
                assert!(
                    !WRITE_TOOLS.contains(&tool.as_str()),
                    "readonly-спека «{name}» содержит пишущий инструмент «{tool}»"
                );
            }
        }
    }

    #[test]
    fn test_runner_is_shell_but_source_safe() {
        let spec = spec_by_name("test_runner");
        // bash сам по себе пишущий инструмент, поэтому readonly быть не может;
        // ограничение «только не-мутирующие команды» зашито в промпт.
        assert!(!spec.readonly);
        assert!(spec.allows_tool("bash"));
        assert!(!spec.allows_tool("edit_file"));
        assert!(spec.system_prompt.contains("do not modify sources"));
    }

    /// Бюджеты взвешены по типу (урок живого прогона 20.07: plan на 200k
    /// оборвался на 10-м ходу без результата — токены растут квадратично,
    /// каждый ход тащит накопленную историю).
    #[test]
    fn default_budget_scales_by_spec_weight() {
        let by_name = |n: &str| builtin_specs().into_iter().find(|s| s.name == n).unwrap();
        let plan = default_budget(&by_name("plan"));
        assert_eq!(plan.max_turns, 25, "ходы — из спеки");
        assert_eq!(plan.max_tokens, 500_000);
        assert_eq!(default_budget(&by_name("explore")).max_tokens, 250_000);
        assert_eq!(default_budget(&by_name("code_review")).max_tokens, 300_000);
        assert_eq!(default_budget(&by_name("test_runner")).max_sec, 900);
        // монотонность: «тяжёлые» типы получают не меньше «лёгкого» explore
        assert!(plan.max_tokens > default_budget(&by_name("explore")).max_tokens);
    }

    #[test]
    fn known_and_write_tools_are_consistent() {
        let unique: std::collections::BTreeSet<&&str> = KNOWN_TOOLS.iter().collect();
        assert_eq!(unique.len(), KNOWN_TOOLS.len(), "дубликаты в KNOWN_TOOLS");
        for tool in WRITE_TOOLS {
            assert!(KNOWN_TOOLS.contains(tool), "«{tool}» из WRITE_TOOLS неизвестен");
        }
    }

    #[test]
    fn validate_collects_all_problems_at_once() {
        let bad = AgentSpec::new(
            "Bad Name!", "  ", "",
            &["read_file", "unknown_tool", "write_file", "read_file"],
            0, true,
        );
        let err = bad.validate().unwrap_err();
        match err {
            AgentError::InvalidSpec { problems, .. } => {
                // имя, purpose, промпт, max_turns, неизвестный тул, пишущий тул, дубликат.
                assert!(problems.len() >= 7, "мало проблем: {problems:?}");
                let joined = problems.join("\n");
                assert!(joined.contains("unknown_tool"));
                assert!(joined.contains("write_file"));
                assert!(joined.contains("дважды"));
            }
            other => panic!("ожидался InvalidSpec, получен {other:?}"),
        }
        // Пустой тулсет — тоже ошибка валидации.
        let quiet = AgentSpec::new("quiet", "цель", "промпт", &[], 3, true);
        assert!(matches!(quiet.validate(), Err(AgentError::InvalidSpec { .. })));
    }

    #[test]
    fn registry_rejects_duplicate_and_invalid() {
        let mut registry = AgentRegistry::new();
        registry.register(AgentSpec::new("helper", "цель", "промпт", &["grep"], 3, true)).unwrap();
        let dup = registry.register(AgentSpec::new("helper", "иная", "иной", &["grep"], 3, true));
        assert_eq!(dup.unwrap_err(), AgentError::DuplicateName { name: "helper".to_string() });
        let invalid = registry.register(AgentSpec::new("", "цель", "промпт", &["grep"], 3, true));
        assert!(matches!(invalid.unwrap_err(), AgentError::InvalidSpec { .. }));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn get_normalizes_case_and_whitespace() {
        let registry = AgentRegistry::with_builtins();
        let spec = registry.get("  EXPLORE \n").unwrap();
        assert_eq!(spec.name, "explore");
        assert!(registry.find("Plan").is_some());
        assert!(registry.find("missing").is_none());
    }

    #[test]
    fn unknown_agent_suggests_by_prefix() {
        let registry = AgentRegistry::with_builtins();
        let err = registry.get("explor").unwrap_err();
        match err {
            AgentError::UnknownAgent { suggestions, .. } => {
                assert_eq!(suggestions, ["explore"]);
            }
            other => panic!("ожидался UnknownAgent, получен {other:?}"),
        }
    }

    #[test]
    fn unknown_agent_suggests_by_levenshtein() {
        let registry = AgentRegistry::with_builtins();
        // Подстрока («review») тоже считается близкой.
        for (typo, want) in [("code_revie", "code_review"), ("test_runer", "test_runner"), ("review", "code_review")] {
            let err = registry.get(typo).unwrap_err();
            match err {
                AgentError::UnknownAgent { suggestions, .. } => {
                    assert!(
                        suggestions.iter().any(|s| s == want),
                        "для «{typo}» ожидалась подсказка «{want}», получено {suggestions:?}"
                    );
                }
                other => panic!("ожидался UnknownAgent, получен {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_agent_lists_available_when_nothing_close() {
        let registry = AgentRegistry::with_builtins();
        let err = registry.get("zzzzzz").unwrap_err();
        let text = err.to_string();
        match err {
            AgentError::UnknownAgent { suggestions, available, .. } => {
                assert!(suggestions.is_empty());
                assert_eq!(available.len(), 4);
                assert!(text.contains("доступные типы"));
                assert!(text.contains("explore"));
            }
            other => panic!("ожидался UnknownAgent, получен {other:?}"),
        }
        // Ветка с подсказками формулируется иначе.
        let text = registry.get("explor").unwrap_err().to_string();
        assert!(text.contains("возможно, вы имели в виду: explore"));
    }

    #[test]
    fn suggestions_are_capped_at_max() {
        let candidates: Vec<String> =
            ["code_review", "explore", "plan", "test_runner"].iter().map(ToString::to_string).collect();
        let found = suggest_names("e", &candidates, MAX_SUGGESTIONS);
        assert_eq!(found.len(), MAX_SUGGESTIONS);
        assert!(found.iter().any(|s| s == "explore"));
        assert!(suggest_names("zzz", &candidates, MAX_SUGGESTIONS).is_empty());
    }

    #[test]
    fn levenshtein_known_distances() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("explore", "explore"), 0);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("flaw", "lawn"), 2);
        // Unicode: считаем символы, а не байты.
        assert_eq!(levenshtein("план", "плак"), 1);
    }

    #[test]
    fn budget_consume_tracks_usage() {
        let mut guard = BudgetGuard::new(AgentBudget::new(3, 100, 3600));
        guard.consume(40).unwrap();
        guard.consume(60).unwrap();
        assert_eq!(guard.used_turns(), 2);
        assert_eq!(guard.used_tokens(), 100);
        guard.check().unwrap();
        // Ровно на границе лимита — ещё не превышение.
        guard.consume(0).unwrap();
        assert_eq!(guard.used_turns(), 3);
        guard.check().unwrap();
    }

    #[test]
    fn budget_turn_limit_exceeded() {
        let mut guard = BudgetGuard::new(AgentBudget::new(1, 1000, 3600));
        guard.consume(10).unwrap();
        let err = guard.consume(10).unwrap_err();
        assert_eq!(err, AgentError::BudgetExceeded { kind: BudgetKind::Turns, limit: 1, used: 2 });
        // Учёт отражает факт, даже когда лимит пробит.
        assert_eq!(guard.used_turns(), 2);
        assert!(guard.check().is_err());
    }

    #[test]
    fn budget_token_limit_exceeded() {
        let mut guard = BudgetGuard::new(AgentBudget::new(10, 100, 3600));
        guard.consume(60).unwrap();
        let err = guard.consume(50).unwrap_err();
        assert_eq!(err, AgentError::BudgetExceeded { kind: BudgetKind::Tokens, limit: 100, used: 110 });
        assert!(err.to_string().contains("tokens"));
    }

    #[test]
    fn budget_wallclock_limit_exceeded() {
        let past = Instant::now().checked_sub(Duration::from_secs(10)).unwrap();
        let mut guard = BudgetGuard::with_start(AgentBudget::new(100, 1000, 5), past);
        // consume намеренно не следит за часами — это работа check().
        guard.consume(0).unwrap();
        let err = guard.check().unwrap_err();
        match err {
            AgentError::BudgetExceeded { kind, limit, used } => {
                assert_eq!(kind, BudgetKind::WallClock);
                assert_eq!(limit, 5);
                assert!(used >= 10);
            }
            other => panic!("ожидался BudgetExceeded, получен {other:?}"),
        }
        // Свежий страж в пределах тех же лимитов — чист.
        BudgetGuard::new(AgentBudget::new(100, 1000, 5)).check().unwrap();
    }

    #[test]
    fn budget_defaults_and_for_spec() {
        let budget = AgentBudget::default();
        assert_eq!(budget.max_turns, 20);
        assert_eq!(budget.max_tokens, 200_000);
        assert_eq!(budget.max_sec, 600);
        let spec = spec_by_name("explore");
        let from_spec = AgentBudget::for_spec(&spec, 50_000, 120);
        assert_eq!(from_spec.max_turns, spec.max_turns);
        assert_eq!(from_spec.max_tokens, 50_000);
        let unlimited = AgentBudget::unlimited();
        assert_eq!(unlimited.max_turns, u32::MAX);
    }

    #[test]
    fn agent_result_from_guard() {
        let mut guard = BudgetGuard::new(AgentBudget::default());
        guard.consume(30).unwrap();
        guard.consume(40).unwrap();
        let result = AgentResult::from_guard("итог прогона", &guard, true);
        assert_eq!(result.summary, "итог прогона");
        assert_eq!(result.turns, 2);
        assert_eq!(result.tokens, 70);
        assert!(result.truncated);
        let direct = AgentResult::new("ok", 1, 10, false);
        assert!(!direct.truncated);
    }

    #[test]
    fn serde_json_roundtrip() {
        let spec = spec_by_name("code_review");
        let json = serde_json::to_string_pretty(&spec).unwrap();
        let back: AgentSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, back);

        let budget = AgentBudget::new(7, 1234, 56);
        let json = serde_json::to_string(&budget).unwrap();
        let back: AgentBudget = serde_json::from_str(&json).unwrap();
        assert_eq!(budget, back);

        let result = AgentResult::new("готово", 3, 999, true);
        let json = serde_json::to_string(&result).unwrap();
        let back: AgentResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, back);

        // Неизвестные поля в JSON отвергаются — конфиг не молчит об опечатках.
        let bad = serde_json::from_str::<AgentSpec>(
            r#"{"name":"x","purpose":"y","system_prompt":"z","allowed_tools":[],"max_turns":1,"readonly":true,"typo":1}"#,
        );
        assert!(bad.is_err());
    }

    #[test]
    fn toml_roundtrip() {
        let spec = spec_by_name("test_runner");
        let text = toml::to_string(&spec).unwrap();
        let back: AgentSpec = toml::from_str(&text).unwrap();
        assert_eq!(spec, back);
    }
}
