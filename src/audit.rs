//! Аудит цели перед `finish` — чистый модуль без зависимостей от агента.
//!
//! Логика вынесена из goal-аудита `agent/mod.rs`: там перед первым `finish`
//! при активной цели модель просто просили подтвердить достижение цели
//! «на словах». Здесь — формальный контур из трёх шагов:
//!
//! 1. [`parse_criteria`] — текст цели (рус/англ, свободная форма, списки)
//!    разбирается эвристиками в список проверяемых [`CompletionCriterion`]:
//!    «тесты зелёные» → тесты, «файл X» → существование файла, команда
//!    в кавычках/бэктиках → успешный запуск, «содержит "..."» → подстрока,
//!    всё прочее с модальными глаголами → ручная проверка.
//! 2. [`evaluate`] — по каждому критерию собирается [`Evidence`] через
//!    внешний коллектор (замыкание вызывающей стороны), затем выносится
//!    [`Verdict`] по строгой матрице.
//! 3. [`render`] — markdown-отчёт с маркерами ✅ / ❌ / ❓.
//!
//! Модуль чистый: ничего не знает про файловую систему, процессы и сеть.
//! Политика сбора доказательств (песочница, лимиты, интерактив с человеком)
//! живёт в коллекторе, который передаёт вызывающая сторона.
//!
//! Матрица вердиктов строгая: пустой список критериев — [`Verdict::Uncertain`]
//! (аудит невозможен), любой неподтверждённый критерий (включая `Manual`) —
//! [`Verdict::Fail`], все подтверждены — [`Verdict::Pass`]. «Мягкая»
//! семантика («ручной критерий не может провалить аудит») при желании
//! реализуется коллектором, который подтверждает `Manual` после показа
//! человеку. Известное ограничение эвристик: отрицания («лог НЕ содержит
//! "panic"») не различаются — такие условия формулируйте без отрицания.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::{self, Write as _};
use std::sync::OnceLock;

// === Критерий завершения ===

/// Вид проверяемого критерия завершения цели.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionKind {
    /// Тесты проходят: «тесты зелёные», "all tests pass", `cargo test`, pytest.
    TestsPass,
    /// Файл существует: «файл src/x.rs создан», "file `Cargo.toml` exists".
    FileExists,
    /// Команда завершается успешно (обычно выделена кавычками или бэктиками):
    /// «команда `make lint` отрабатывает», 'run "make build" succeeds'.
    CommandSucceeds,
    /// Текст/вывод содержит заданную подстроку: «лог содержит "OK"».
    TextContains,
    /// Автоматически не проверяется — требуется подтверждение человеком.
    Manual,
}

impl CriterionKind {
    /// Строковое имя (совпадает с serde-представлением).
    pub fn as_str(self) -> &'static str {
        match self {
            CriterionKind::TestsPass => "tests_pass",
            CriterionKind::FileExists => "file_exists",
            CriterionKind::CommandSucceeds => "command_succeeds",
            CriterionKind::TextContains => "text_contains",
            CriterionKind::Manual => "manual",
        }
    }

    /// Короткая русская метка для отчёта.
    pub fn label(self) -> &'static str {
        match self {
            CriterionKind::TestsPass => "тесты",
            CriterionKind::FileExists => "файл",
            CriterionKind::CommandSucceeds => "команда",
            CriterionKind::TextContains => "текст",
            CriterionKind::Manual => "ручная проверка",
        }
    }
}

impl fmt::Display for CriterionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Проверяемый критерий завершения цели.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionCriterion {
    /// Исходная формулировка (сегмент текста цели, без маркеров списка).
    pub text: String,
    /// Вид проверки.
    pub kind: CriterionKind,
    /// Извлечённая цель проверки: путь к файлу, команда или искомая
    /// подстрока (`None`, если извлечь не удалось или не требуется).
    pub target: Option<String>,
}

impl CompletionCriterion {
    /// Конструктор без эвристик — когда критерий известен точно.
    pub fn new(text: impl Into<String>, kind: CriterionKind, target: Option<String>) -> Self {
        Self { text: text.into(), kind, target }
    }
}

// === Доказательство и вердикт ===

/// Доказательство по одному критерию, собранное внешним коллектором.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Вид проверки, к которой относится доказательство.
    pub kind: CriterionKind,
    /// Что именно наблюдал коллектор: путь, вывод команды, числа, цитата.
    pub detail: String,
    /// Подтверждает ли наблюдение критерий.
    pub ok: bool,
}

impl Evidence {
    /// Полный конструктор.
    pub fn new(kind: CriterionKind, detail: impl Into<String>, ok: bool) -> Self {
        Self { kind, detail: detail.into(), ok }
    }

    /// Короткий конструктор подтверждения (`ok = true`).
    pub fn confirmed(kind: CriterionKind, detail: impl Into<String>) -> Self {
        Self::new(kind, detail, true)
    }

    /// Короткий конструктор опровержения (`ok = false`).
    pub fn refuted(kind: CriterionKind, detail: impl Into<String>) -> Self {
        Self::new(kind, detail, false)
    }
}

/// Итоговый вердикт аудита.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Verdict {
    /// Все критерии подтверждены.
    Pass,
    /// Есть хотя бы один неподтверждённый критерий.
    Fail,
    /// Критерии не распознаны — аудит невозможен.
    Uncertain,
}

impl Verdict {
    /// Строковое имя (совпадает с serde-представлением).
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Uncertain => "UNCERTAIN",
        }
    }

    /// Маркер для отчёта: ✅ pass, ❌ fail, ❓ uncertain.
    pub fn icon(self) -> &'static str {
        match self {
            Verdict::Pass => "✅",
            Verdict::Fail => "❌",
            Verdict::Uncertain => "❓",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Полный отчёт аудита цели.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditReport {
    /// Критерии в порядке их следования в тексте цели.
    pub criteria: Vec<CompletionCriterion>,
    /// Доказательства, параллельные `criteria` (индекс в индекс).
    pub evidence: Vec<Evidence>,
    /// Итоговый вердикт.
    pub verdict: Verdict,
    /// Индексы (0-based) критериев без подтверждения — что доделать.
    pub missing: Vec<usize>,
}

impl AuditReport {
    /// Число подтверждённых критериев.
    pub fn confirmed(&self) -> usize {
        self.evidence.iter().filter(|e| e.ok).count()
    }

    /// Удобная проверка: аудит пройден?
    pub fn is_pass(&self) -> bool {
        self.verdict == Verdict::Pass
    }
}

// === Разбор текста цели ===

/// Одна эвристика: вид критерия + скомпилированный шаблон + номер группы
/// захвата с целью проверки (0 — цели нет).
struct Rule {
    kind: CriterionKind,
    re: Regex,
    target_group: usize,
}

/// Сырые правила в порядке приоритета: для сегмента срабатывает первое
/// совпавшее. Шаблоны константные и проверены тестами, но компилируем
/// безопасно (без unwrap/expect): битый шаблон просто отбрасывается.
const RAW_RULES: &[(CriterionKind, &str, usize)] = &[
    // 1. Тесты (рус + англ). Идут первыми: «cargo test» важнее «команды».
    (CriterionKind::TestsPass, r#"(?i)(\bтест\w*\s+(зел[ёе]н\w*|проход\w*|запущ\w*|прогнан\w*)|\bвсе\s+тест\w*|\bпрогон\s+тест\w*|\bзапуск\s+тест\w*|\bтест\w*\s+должн\w*\s+(быть\s+)?(зел[ёе]н\w*|пройд\w*|проход\w*)|cargo\s+test|pytest|npm\s+test|go\s+test|\btests?\s+(pass\w*|are\s+green|must\s+pass|should\s+pass|succeed\w*|green\b)|\ball\s+tests\b|\bgreen\s+tests\b|\bunit\s+tests\b|\btest\s+suite\b)"#, 0),
    // 2. «... содержит "подстрока"» / 'output contains "DONE"' (до 2 слов между).
    (CriterionKind::TextContains, r#"(?i)(?:\bсодерж\w*|\bcontains?\b|\bупомин\w*|\bmentions?\b)(?:\s+\S+){0,2}?\s*[`"'«»]([^`"'«»]{1,200})[`"'«»]"#, 1),
    // 3. Команда: ключевое слово, затем команда в кавычках/бэктиках.
    (CriterionKind::CommandSucceeds, r#"(?i)(?:\bкоманд\w*|\bcommand\w*|\bзапуст\w*|\bвыполн\w*|\brun\b|\bsucceed\w*|\bотработ\w*|\bотрабатыва\w*).{0,60}?[`"'«»]([^`"'«»]{1,200})[`"'«»]"#, 1),
    // 4. Команда в кавычках, затем глагол результата: «`cargo build` отрабатывает».
    (CriterionKind::CommandSucceeds, r#"(?i)[`"'«»]([^`"'«»]{1,200})[`"'«»]\s*(?:отработ\w*|отрабатыва\w*|succeed\w*|проход\w*|зел[ёе]н\w*|без\s+ошибок|без\s+warnings|exits?\s+\w*\s*0\b|долж\w*\s+отработать|заверш\w*\s+(успешно|с\s+кодом\s+0))"#, 1),
    // 5. «файл X» / "file X": путь обязан содержать букву и расширение,
    //    чтобы не ловить «файл конфигурации» и версии вида «1.2».
    (CriterionKind::FileExists, r#"(?i)(?:\bфайл\w*|\bfile\b)\s*[:—-]?\s*[`"'«»]?([~\w./+-]*[A-Za-zА-Яа-яЁё_][\w./~+-]*\.\w{1,6})[`"'«»]?"#, 1),
    // 6. Путь, затем глагол существования: «out.txt создан», "main.rs exists".
    (CriterionKind::FileExists, r#"(?i)([~\w./+-]*[A-Za-zА-Яа-яЁё_][\w./~+-]*\.\w{1,6})\s+(?:долж\w*\s+)?(?:существу\w*|создан\w*|на\s+месте|находит\w*|exists?\b|created\b|updated\b|обновл\w*|present\b)"#, 1),
    // 7. Модальные глаголы без автоматической проверки → ручной критерий.
    (CriterionKind::Manual, r#"(?i)(\bдолж\w*|\bнужно\b|\bнадо\b|\bнеобходимо\b|\bтребуется\b|\bmust\b|\bshould\b|\bshall\b|\bensure\w*|\bverify\b|\bverified\b|\bvalidate\w*|\bпровер\w*|\bубеди\w*)"#, 0),
];

/// Скомпилированные правила (ленивая инициализация, один раз на процесс).
fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        RAW_RULES
            .iter()
            .filter_map(|&(kind, pattern, target_group)| {
                Regex::new(pattern).ok().map(|re| Rule { kind, re, target_group })
            })
            .collect()
    })
}

/// Делит текст цели на сегменты-кандидаты: по строкам, «;», «•» и по
/// границам предложений («. » перед заглавной буквой — точки внутри путей
/// и версий не режем, после них нет пробела с заглавной).
fn segments(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for chunk in text.split(['\n', ';', '•']) {
        split_sentences(chunk, &mut out);
    }
    out
}

/// Делит строку на предложения по «. » перед заглавной буквой.
fn split_sentences<'a>(chunk: &'a str, out: &mut Vec<&'a str>) {
    let chars: Vec<(usize, char)> = chunk.char_indices().collect();
    let mut start = 0;
    let mut k = 0;
    while k < chars.len() {
        if chars[k].1 == '.' {
            // Пропускаем пробелы после точки и смотрим первый значащий символ.
            let mut j = k + 1;
            while j < chars.len() && chars[j].1.is_whitespace() {
                j += 1;
            }
            // Режем только если был хотя бы один пробел и дальше заглавная.
            if j > k + 1 && j < chars.len() && chars[j].1.is_uppercase() {
                out.push(&chunk[start..chars[k].0 + 1]);
                start = chars[j].0;
            }
            k = j;
        } else {
            k += 1;
        }
    }
    if start < chunk.len() {
        out.push(&chunk[start..]);
    }
}

/// Срезает маркеры списка в начале сегмента: «- », «* », «1. », «2) ».
fn strip_marker(seg: &str) -> &str {
    let s = seg.trim_start();
    if let Some(rest) = s.strip_prefix("- ").or_else(|| s.strip_prefix("* ")) {
        return rest.trim_start();
    }
    // Нумерованный маркер: «12. » или «3) » (цифры ASCII — 1 байт каждая).
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let rest = &s[digits..];
        if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return r.trim_start();
        }
    }
    s
}

/// Классифицирует сегмент первым сработавшим правилом и извлекает цель.
fn classify(text: &str) -> Option<(CriterionKind, Option<String>)> {
    rules().iter().find_map(|rule| {
        rule.re.captures(text).map(|caps| {
            let target = if rule.target_group == 0 {
                None
            } else {
                caps.get(rule.target_group)
                    .map(|m| m.as_str().trim().to_string())
                    .filter(|s| !s.is_empty())
            };
            (rule.kind, target)
        })
    })
}

/// Разбирает текст цели в список проверяемых критериев.
///
/// Эвристики (по убыванию приоритета): тесты («тесты зелёные», "tests
/// pass", `cargo test`) → подстрока («содержит "OK"») → команда в
/// кавычках/бэктиках с глаголом запуска («команда `make lint`», 'run "x"
/// succeeds') → файл с путём и расширением («файл src/x.rs создан») →
/// модальный глагол («должен», "should") → [`CriterionKind::Manual`].
/// Сегменты без признаков критерия пропускаются, дубли (без учёта регистра)
/// схлопываются. Пустой результат — нормальный исход: [`evaluate`] даст
/// [`Verdict::Uncertain`].
pub fn parse_criteria(goal_text: &str) -> Vec<CompletionCriterion> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for seg in segments(goal_text) {
        let text = strip_marker(seg).trim().trim_end_matches('.').trim();
        if text.chars().count() < 4 || !text.chars().any(char::is_alphabetic) {
            continue;
        }
        let Some((kind, target)) = classify(text) else { continue };
        if !seen.insert(text.to_lowercase()) {
            continue;
        }
        out.push(CompletionCriterion { text: text.to_string(), kind, target });
    }
    out
}

// === Оценка и рендер ===

/// Собирает доказательства по всем критериям и выносит вердикт.
///
/// Коллектор вызывается по одному разу на критерий, в порядке списка;
/// модуль доверяет его доказательствам как есть. Матрица вердиктов:
/// пустой `criteria` → [`Verdict::Uncertain`] (коллектор не вызывается),
/// все `ok` → [`Verdict::Pass`], хотя бы один `!ok` → [`Verdict::Fail`],
/// а `missing` получает индексы неподтверждённых критериев.
pub fn evaluate(
    criteria: Vec<CompletionCriterion>,
    collect: &dyn Fn(&CompletionCriterion) -> Evidence,
) -> AuditReport {
    let mut evidence = Vec::with_capacity(criteria.len());
    let mut missing = Vec::new();
    for (i, c) in criteria.iter().enumerate() {
        let ev = collect(c);
        if !ev.ok {
            missing.push(i);
        }
        evidence.push(ev);
    }
    let verdict = if criteria.is_empty() {
        Verdict::Uncertain
    } else if missing.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    AuditReport { criteria, evidence, verdict, missing }
}

/// Рендерит отчёт в markdown: строка вердикта, маркированный список
/// критериев (✅ подтверждён, ❌ опровергнут, ❓ ручная проверка без
/// подтверждения или доказательство не собрано) и итог со списком
/// недостающих доказательств (индексы 1-based, для человека).
pub fn render(report: &AuditReport) -> String {
    let mut out = String::new();
    let note = match report.verdict {
        Verdict::Pass => "все критерии подтверждены",
        Verdict::Fail => "есть критерии без подтверждения",
        Verdict::Uncertain => "критерии не распознаны",
    };
    let icon = report.verdict.icon();
    let word = report.verdict.as_str();
    let _ = writeln!(out, "Вердикт аудита: {icon} {word} — {note}");
    if report.criteria.is_empty() {
        out.push_str("Критерии не распознаны: переформулируйте цель с проверяемыми условиями.\n");
        return out;
    }
    for (i, c) in report.criteria.iter().enumerate() {
        let (mark, detail) = match report.evidence.get(i) {
            Some(ev) if ev.ok => ("✅", ev.detail.as_str()),
            Some(ev) if c.kind == CriterionKind::Manual => ("❓", ev.detail.as_str()),
            Some(ev) => ("❌", ev.detail.as_str()),
            None => ("❓", "доказательство не собрано"),
        };
        let kind = c.kind.label();
        let text = c.text.as_str();
        let _ = writeln!(out, "- {mark} [{kind}] {text} — {detail}");
    }
    let ok = report.confirmed();
    let total = report.criteria.len();
    let _ = write!(out, "Подтверждено {ok}/{total}");
    if !report.missing.is_empty() {
        let idx = report
            .missing
            .iter()
            .map(|i| (i + 1).to_string())
            .collect::<Vec<_>>()
            .join(", #");
        let _ = write!(out, "; без доказательств: #{idx}");
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// Разбор текста, где ожидается ровно один критерий.
    fn parse_one(text: &str) -> CompletionCriterion {
        let v = parse_criteria(text);
        assert_eq!(v.len(), 1, "ожидался ровно один критерий: {text}");
        v.into_iter().next().expect("один критерий")
    }

    /// Виды всех распознанных критериев (для компактных проверок).
    fn kinds(criteria: &[CompletionCriterion]) -> Vec<CriterionKind> {
        criteria.iter().map(|c| c.kind).collect()
    }

    /// Мок-коллектор: всё подтверждает, кроме указанного вида.
    fn mock_collector(fail_on: CriterionKind) -> impl Fn(&CompletionCriterion) -> Evidence {
        move |c| {
            if c.kind == fail_on {
                Evidence::refuted(c.kind, "не подтверждено (мок)")
            } else {
                Evidence::confirmed(c.kind, "подтверждено (мок)")
            }
        }
    }

    // --- Парсинг: тесты ---

    #[test]
    fn parse_tests_ru() {
        let c = parse_one("Все тесты должны быть зелёными");
        assert_eq!(c.kind, CriterionKind::TestsPass);
        assert_eq!(c.target, None);
    }

    #[test]
    fn parse_tests_en() {
        let c = parse_one("Goal: fix the bug; all tests must pass");
        assert_eq!(c.kind, CriterionKind::TestsPass);
    }

    #[test]
    fn parse_tests_cargo_test() {
        let c = parse_one("cargo test passes without failures");
        assert_eq!(c.kind, CriterionKind::TestsPass);
    }

    // --- Парсинг: файлы ---

    #[test]
    fn parse_file_ru_with_path() {
        let c = parse_one("файл src/audit.rs создан");
        assert_eq!(c.kind, CriterionKind::FileExists);
        assert_eq!(c.target.as_deref(), Some("src/audit.rs"));
    }

    #[test]
    fn parse_file_en_backticks() {
        let c = parse_one("make sure file `Cargo.toml` exists");
        assert_eq!(c.kind, CriterionKind::FileExists);
        assert_eq!(c.target.as_deref(), Some("Cargo.toml"));
    }

    #[test]
    fn parse_file_without_path_falls_to_manual() {
        // Нет пути с расширением — автоматически не проверить, это Manual.
        let c = parse_one("файл должен существовать");
        assert_eq!(c.kind, CriterionKind::Manual);
        assert_eq!(c.target, None);
    }

    // --- Парсинг: команды ---

    #[test]
    fn parse_command_ru_backticks() {
        let c = parse_one("запустить команду `cargo clippy --offline`");
        assert_eq!(c.kind, CriterionKind::CommandSucceeds);
        assert_eq!(c.target.as_deref(), Some("cargo clippy --offline"));
    }

    #[test]
    fn parse_command_en_quoted() {
        let c = parse_one("run \"make build\" succeeds");
        assert_eq!(c.kind, CriterionKind::CommandSucceeds);
        assert_eq!(c.target.as_deref(), Some("make build"));
    }

    #[test]
    fn parse_command_quote_before_verb() {
        let c = parse_one("`cargo build` отрабатывает без ошибок");
        assert_eq!(c.kind, CriterionKind::CommandSucceeds);
        assert_eq!(c.target.as_deref(), Some("cargo build"));
    }

    // --- Парсинг: подстрока ---

    #[test]
    fn parse_text_contains_ru() {
        let c = parse_one("лог содержит \"BUILD OK\"");
        assert_eq!(c.kind, CriterionKind::TextContains);
        assert_eq!(c.target.as_deref(), Some("BUILD OK"));
    }

    #[test]
    fn parse_text_contains_guillemets() {
        let c = parse_one("вывод содержит «готово»");
        assert_eq!(c.kind, CriterionKind::TextContains);
        assert_eq!(c.target.as_deref(), Some("готово"));
    }

    // --- Парсинг: ручные и составные цели ---

    #[test]
    fn parse_manual_ru_and_en() {
        assert_eq!(parse_one("должен быть README на русском языке").kind, CriterionKind::Manual);
        assert_eq!(parse_one("README should be updated").kind, CriterionKind::Manual);
    }

    #[test]
    fn parse_mixed_multiline_order_and_markers() {
        let goal = "1. Тесты зелёные\n2) файл report/out.txt создан\n- команда `make lint` отрабатывает";
        let v = parse_criteria(goal);
        assert_eq!(
            kinds(&v),
            vec![
                CriterionKind::TestsPass,
                CriterionKind::FileExists,
                CriterionKind::CommandSucceeds,
            ]
        );
        // Маркеры списков срезаны из формулировок.
        assert!(!v[0].text.starts_with("1."));
        assert_eq!(v[1].target.as_deref(), Some("report/out.txt"));
        assert_eq!(v[2].target.as_deref(), Some("make lint"));
    }

    #[test]
    fn parse_sentence_split_prose() {
        // Граница предложения: «. » перед заглавной; точка в «out.txt» не режет.
        let v = parse_criteria("Создать файл out.txt. Тесты зелёные.");
        assert_eq!(kinds(&v), vec![CriterionKind::FileExists, CriterionKind::TestsPass]);
        assert_eq!(v[0].target.as_deref(), Some("out.txt"));
    }

    #[test]
    fn parse_dedup_identical() {
        let v = parse_criteria("Тесты зелёные\nтесты зелёные");
        assert_eq!(v.len(), 1, "дубли без учёта регистра схлопываются: {v:?}");
    }

    #[test]
    fn parse_garbage_and_empty() {
        assert!(parse_criteria("").is_empty());
        assert!(parse_criteria("сделать хорошо и красиво").is_empty());
        assert!(parse_criteria("---\n***").is_empty());
    }

    // --- Evaluate: матрица вердиктов ---

    #[test]
    fn evaluate_all_ok_pass() {
        let criteria = vec![
            CompletionCriterion::new("тесты зелёные", CriterionKind::TestsPass, None),
            CompletionCriterion::new("файл a.rs", CriterionKind::FileExists, Some("a.rs".into())),
        ];
        let report = evaluate(criteria, &|c: &CompletionCriterion| Evidence::confirmed(c.kind, "ок"));
        assert_eq!(report.verdict, Verdict::Pass);
        assert!(report.is_pass());
        assert!(report.missing.is_empty());
        assert_eq!(report.evidence.len(), 2);
        assert_eq!(report.confirmed(), 2);
    }

    #[test]
    fn evaluate_any_fail_gives_fail_and_missing_indices() {
        let criteria = vec![
            CompletionCriterion::new("тесты", CriterionKind::TestsPass, None),
            CompletionCriterion::new("файл", CriterionKind::FileExists, None),
            CompletionCriterion::new("команда", CriterionKind::CommandSucceeds, None),
        ];
        let report = evaluate(criteria, &mock_collector(CriterionKind::FileExists));
        assert_eq!(report.verdict, Verdict::Fail);
        assert_eq!(report.missing, vec![1]);
        assert_eq!(report.confirmed(), 2);
        assert_eq!(report.evidence[1].detail, "не подтверждено (мок)");
    }

    #[test]
    fn evaluate_all_fail_lists_every_index() {
        let criteria = vec![
            CompletionCriterion::new("a", CriterionKind::TestsPass, None),
            CompletionCriterion::new("b", CriterionKind::FileExists, None),
        ];
        let report = evaluate(criteria, &|c: &CompletionCriterion| Evidence::refuted(c.kind, "нет"));
        assert_eq!(report.verdict, Verdict::Fail);
        assert_eq!(report.missing, vec![0, 1]);
    }

    #[test]
    fn evaluate_empty_criteria_uncertain_and_collector_not_called() {
        let calls = Cell::new(0u32);
        let report = evaluate(Vec::new(), &|c: &CompletionCriterion| {
            calls.set(calls.get() + 1);
            Evidence::confirmed(c.kind, "не должно случиться")
        });
        assert_eq!(calls.get(), 0, "коллектор не должен вызываться на пустом списке");
        assert_eq!(report.verdict, Verdict::Uncertain);
        assert!(report.criteria.is_empty());
        assert!(report.evidence.is_empty());
    }

    #[test]
    fn evaluate_collector_sees_criteria_in_order() {
        let seen: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let criteria = vec![
            CompletionCriterion::new("первый", CriterionKind::TestsPass, None),
            CompletionCriterion::new("второй", CriterionKind::Manual, None),
        ];
        let report = evaluate(criteria, &|c: &CompletionCriterion| {
            seen.borrow_mut().push(c.text.clone());
            Evidence::confirmed(c.kind, "ок")
        });
        assert_eq!(seen.borrow().as_slice(), ["первый", "второй"]);
        // Критерии сохраняются в отчёте без изменений.
        assert_eq!(report.criteria[1].kind, CriterionKind::Manual);
    }

    // --- Рендер ---

    #[test]
    fn render_pass_format() {
        let criteria = vec![CompletionCriterion::new(
            "тесты зелёные",
            CriterionKind::TestsPass,
            None,
        )];
        let report = evaluate(criteria, &|c: &CompletionCriterion| {
            Evidence::confirmed(c.kind, "cargo test: 12 passed")
        });
        let md = render(&report);
        assert!(md.contains("Вердикт аудита: ✅ PASS"), "{md}");
        assert!(md.contains("- ✅ [тесты] тесты зелёные — cargo test: 12 passed"), "{md}");
        assert!(md.contains("Подтверждено 1/1"), "{md}");
        assert!(!md.contains("без доказательств"), "{md}");
    }

    #[test]
    fn render_fail_format_lists_missing() {
        let criteria = vec![
            CompletionCriterion::new("тесты зелёные", CriterionKind::TestsPass, None),
            CompletionCriterion::new("файл src/audit.rs создан", CriterionKind::FileExists, Some("src/audit.rs".into())),
        ];
        let report = evaluate(criteria, &mock_collector(CriterionKind::FileExists));
        let md = render(&report);
        assert!(md.contains("Вердикт аудита: ❌ FAIL"), "{md}");
        assert!(md.contains("- ✅ [тесты]"), "{md}");
        assert!(md.contains("- ❌ [файл] файл src/audit.rs создан — не подтверждено (мок)"), "{md}");
        assert!(md.contains("Подтверждено 1/2; без доказательств: #2"), "{md}");
    }

    #[test]
    fn render_uncertain_format() {
        let report = evaluate(Vec::new(), &|c: &CompletionCriterion| Evidence::confirmed(c.kind, ""));
        let md = render(&report);
        assert!(md.contains("Вердикт аудита: ❓ UNCERTAIN"), "{md}");
        assert!(md.contains("переформулируйте цель"), "{md}");
    }

    #[test]
    fn render_manual_unconfirmed_is_question_mark_but_fail() {
        // Строгая матрица: неподтверждённый Manual → Fail, но маркер ❓.
        let criteria = vec![CompletionCriterion::new(
            "должен быть README на русском",
            CriterionKind::Manual,
            None,
        )];
        let report = evaluate(criteria, &|c: &CompletionCriterion| {
            Evidence::refuted(c.kind, "не проверялось человеком")
        });
        assert_eq!(report.verdict, Verdict::Fail);
        let md = render(&report);
        assert!(md.contains("- ❓ [ручная проверка] должен быть README на русском"), "{md}");
        assert!(md.contains("❌ FAIL"), "{md}");
    }

    #[test]
    fn render_missing_evidence_entry() {
        // Отчёт собран вручную: доказательства короче критериев.
        let report = AuditReport {
            criteria: vec![
                CompletionCriterion::new("тесты", CriterionKind::TestsPass, None),
                CompletionCriterion::new("файл", CriterionKind::FileExists, None),
            ],
            evidence: vec![Evidence::confirmed(CriterionKind::TestsPass, "ок")],
            verdict: Verdict::Fail,
            missing: vec![1],
        };
        let md = render(&report);
        assert!(md.contains("- ❓ [файл] файл — доказательство не собрано"), "{md}");
    }

    // --- Serde ---

    #[test]
    fn serde_roundtrip_report() {
        let criteria = vec![
            CompletionCriterion::new("тесты зелёные", CriterionKind::TestsPass, None),
            CompletionCriterion::new("файл a.rs", CriterionKind::FileExists, Some("a.rs".into())),
        ];
        let report = evaluate(criteria, &mock_collector(CriterionKind::FileExists));
        let json = serde_json::to_string_pretty(&report).expect("сериализация отчёта");
        let back: AuditReport = serde_json::from_str(&json).expect("десериализация отчёта");
        assert_eq!(report, back);
        // Стабильные строковые формы (serde-представление).
        assert!(json.contains("\"tests_pass\""), "{json}");
        assert!(json.contains("\"FAIL\""), "{json}");
    }
}
