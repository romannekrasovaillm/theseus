//! Lark-грамматика формата apply-patch как часть API-контракта харнесса.
//!
//! Модуль фиксирует грамматику нашего диалекта apply-patch (образец —
//! официальная Lark-грамматика из `codex-rs/apply-patch`, но без
//! `environment_id`, `*** Move to` и `*** End of File`: их наш парсер
//! [`crate::patch`] не поддерживает) и даёт строгий валидатор на её основе.
//!
//! Зачем нужна строгая проверка, если есть ленивый парсер: модель в ответе
//! часто «почти» соблюдает формат — ставит отступ перед маркером, пишет путь
//! с пробелом, оставляет пустую секцию `@@`. Ленивый парсер часть этого
//! прощает, часть отклоняет по одной ошибке за раз. Строгий валидатор
//! собирает ВСЕ нарушения за один проход и отклоняет всё, что выходит за
//! пределы контракта, — это удобно и для валидации ответов модели, и для
//! тестирования самого парсера.
//!
//! Публичный API:
//! - [`render_grammar`] — полный текст Lark-грамматики (артефакт контракта);
//! - [`grammar_digest`] — краткая выжимка для системного промпта модели;
//! - [`validate_strict`] — строгая проверка, список нарушений с номерами строк;
//! - [`to_patch_ops`] — строгая валидация + конвертация в [`crate::patch::PatchOp`].

use anyhow::{bail, Context, Result};
use std::fmt;

use crate::patch::PatchOp;

/// Маркер начала патча.
const BEGIN_MARKER: &str = "*** Begin Patch";
/// Маркер конца патча.
const END_MARKER: &str = "*** End Patch";
/// Префикс операции добавления файла.
const ADD_PREFIX: &str = "*** Add File: ";
/// Префикс операции обновления файла.
const UPDATE_PREFIX: &str = "*** Update File: ";
/// Префикс операции удаления файла.
const DELETE_PREFIX: &str = "*** Delete File: ";
/// Префикс любой строки-маркера (по нему определяем границу секции).
const OP_PREFIX: &str = "*** ";

/// Текст Lark-грамматики (возвращается из [`render_grammar`]).
///
/// Диалект theseus: обрамление `*** Begin/End Patch`, три операции,
/// контекстные строки `@@`. Отличия от грамматики codex-rs: нет
/// `environment_id`, `change_move` (`*** Move to`) и `eof_line`
/// (`*** End of File`); зато явно зафиксированы строгие ограничения
/// (путь без пробелов, чанк не из одного контекста).
const LARK_GRAMMAR: &str = r#"// Lark-грамматика формата apply-patch харнесса theseus.
// По образцу codex-rs/apply-patch, без environment_id, "*** Move to"
// и "*** End of File". Маркеры стоят строго в начале строки; тела
// операций не содержат строк с префиксом "*** ".

start: begin_patch hunk* end_patch

begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?

hunk: add_hunk | update_hunk | delete_hunk

// Создание файла: тело — одна или более добавляемых строк с префиксом "+".
add_hunk: "*** Add File: " filename LF add_line+
add_line: "+" line_body LF

// Удаление файла: тела нет.
delete_hunk: "*** Delete File: " filename LF

// Обновление файла: один или несколько чанков. Чанк — необязательный
// якорь "@@" и непустой блок строк изменений; хотя бы одна строка
// "+" (добавить) или "-" (удалить), остальные — контекст " ".
update_hunk: "*** Update File: " filename LF update_chunk+
update_chunk: change_context? change_line+
change_context: "@@" line_body LF
change_line: ("+" | "-" | " ") line_body LF | LF

// Путь относительный, без пробельных символов.
filename: /[^\s]+/
line_body: /[^\n]*/

// Перевод строки.
LF: /\n/
"#;

/// Краткая инструкция по формату для системного промпта (возвращается из [`grammar_digest`]).
const DIGEST: &str = "Формат apply-patch (строго):\n\
*** Begin Patch\n\
*** Add File: <путь> — строки только с префиксом '+' (содержимое файла)\n\
*** Update File: <путь> — якорь '@@ <контекст>', затем строки ' ' (контекст), '-' (удалить), '+' (добавить); в чанке нужна хотя бы одна '+' или '-'\n\
*** Delete File: <путь>\n\
*** End Patch\n\
Маркеры — строго с начала строки. Пути без пробелов. Пустые секции запрещены.";

/// Одно нарушение строгой грамматики патча.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarIssue {
    /// Номер строки патча (1-based), где обнаружено нарушение.
    pub line: usize,
    /// Человекочитаемое описание нарушения (на русском).
    pub message: String,
}

impl fmt::Display for GrammarIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "строка {}: {}", self.line, self.message)
    }
}

/// Полный текст Lark-грамматики формата apply-patch.
///
/// Грамматика — артефакт API-контракта: её можно передать в Lark-парсер
/// или приложить к спецификации формата. Описывает ровно тот диалект,
/// который принимает парсер [`crate::patch`], с ограничениями строгого
/// режима [`validate_strict`] (маркеры в первой колонке, пути без пробелов,
/// непустые секции).
pub fn render_grammar() -> String {
    LARK_GRAMMAR.to_string()
}

/// Краткая инструкция по формату для системного промпта модели.
///
/// Одна строка на операцию плюс правила оформления; на порядок короче
/// полной грамматики [`render_grammar`] — подходит для встраивания
/// в системный промпт без заметного расхода контекста.
pub fn grammar_digest() -> String {
    DIGEST.to_string()
}

/// Строго проверить текст патча на соответствие грамматике.
///
/// Проверки (все — с номером строки в [`GrammarIssue`]):
/// - обрамление `*** Begin Patch` / `*** End Patch`, маркеры занимают
///   строку целиком (без пробелов по краям) и стоят в первой колонке;
/// - маркеры операций — только известные и только с `***` в начале строки;
/// - пути непустые, без пробелов и табуляций (включая хвостовые);
/// - в секции `Add File` каждая строка начинается с `+` (никаких `-`/` `);
/// - секция `@@` непуста: после якоря есть хотя бы одна строка изменений;
/// - каждый чанк `Update File` содержит хотя бы одну строку `-` или `+`
///   (чанк из одного контекста — ошибка).
///
/// Возвращает все найденные нарушения, отсортированные по номеру строки
/// (стабильно: внутри одной строки — в порядке обнаружения). Пустой список
/// означает, что патч валиден и гарантированно принимается парсером.
pub fn validate_strict(patch_text: &str) -> Vec<GrammarIssue> {
    let lines: Vec<&str> = patch_text.lines().collect();
    let mut issues = Vec::new();
    if lines.is_empty() {
        issues.push(issue(
            1,
            format!("патч пуст: ожидались строки '{BEGIN_MARKER}' … '{END_MARKER}'"),
        ));
        return issues;
    }
    // Маркер начала: строго первая строка, без пробельных символов по краям.
    if lines[0] != BEGIN_MARKER {
        if lines[0].trim() == BEGIN_MARKER {
            issues.push(issue(
                1,
                format!("маркер '{BEGIN_MARKER}' должен занимать строку целиком, без пробелов по краям"),
            ));
        } else {
            issues.push(issue(1, format!("первая строка патча должна быть '{BEGIN_MARKER}'")));
        }
    }
    // Маркер конца: последняя непустая строка (хвостовые пустые строки допустимы).
    let Some(last) = lines.iter().rposition(|l| !l.trim().is_empty()) else {
        issues.push(issue(1, format!("в патче нет маркера '{END_MARKER}'")));
        return issues;
    };
    let end_ok = lines[last] == END_MARKER;
    if !end_ok {
        if lines[last].trim() == END_MARKER {
            issues.push(issue(
                last + 1,
                format!("маркер '{END_MARKER}' должен занимать строку целиком, без пробелов по краям"),
            ));
        } else {
            issues.push(issue(
                last + 1,
                format!("последняя непустая строка патча должна быть '{END_MARKER}'"),
            ));
        }
    }
    // Тело: между маркером начала и маркером конца (либо до конца текста,
    // если маркер конца не найден, — продолжаем сбор остальных нарушений).
    let body_end = if end_ok { last } else { lines.len() };
    let term_line = if end_ok { last + 1 } else { lines.len() };
    let mut state = ScanState::TopLevel;
    let mut i = 1;
    while i < body_end {
        let line = lines[i];
        let n = i + 1;
        match std::mem::take(&mut state) {
            ScanState::TopLevel => {
                state = scan_top_level(line, n, &mut issues);
                i += 1;
            }
            ScanState::Add { path, body_lines } => {
                if line.starts_with(OP_PREFIX) {
                    if body_lines == 0 {
                        issues.push(issue(n, format!("секция Add File '{path}' не содержит строк с '+'")));
                    }
                    state = ScanState::TopLevel;
                    continue; // маркер обработаем в TopLevel на следующей итерации
                }
                if line.starts_with('+') {
                    state = ScanState::Add { path, body_lines: body_lines + 1 };
                } else {
                    issues.push(issue(n, add_line_message(&path, line)));
                    state = ScanState::Add { path, body_lines };
                }
                i += 1;
            }
            ScanState::Update(mut scan) => {
                if line.starts_with(OP_PREFIX) {
                    scan.finish(n, &mut issues);
                    state = ScanState::TopLevel;
                    continue; // маркер обработаем в TopLevel на следующей итерации
                }
                if line.strip_prefix("@@").is_some() {
                    scan.finish_chunk(&mut issues);
                    scan.begin_chunk(n);
                } else {
                    match line.chars().next() {
                        Some('+') | Some('-') => scan.push_line(n, true),
                        Some(' ') | None => scan.push_line(n, false),
                        _ => issues.push(issue(
                            n,
                            format!(
                                "в секции Update File '{}' строка должна начинаться с ' ', '-', '+' или '@@'",
                                scan.path
                            ),
                        )),
                    }
                }
                state = ScanState::Update(scan);
                i += 1;
            }
        }
    }
    // Секция, дотянувшаяся до конца тела, финализируется на строке маркера
    // конца (или на последней строке текста, если маркера конца нет).
    match state {
        ScanState::TopLevel => {}
        ScanState::Add { path, body_lines } => {
            if body_lines == 0 {
                issues.push(issue(term_line, format!("секция Add File '{path}' не содержит строк с '+'")));
            }
        }
        ScanState::Update(scan) => scan.finish(term_line, &mut issues),
    }
    issues.sort_by_key(|it| it.line);
    issues
}

/// Строгая валидация патча и конвертация в операции [`crate::patch`].
///
/// Сначала прогоняет [`validate_strict`]: при любом нарушении возвращает
/// ошибку со списком всех проблем (по одной на строку). Затем вызывает
/// ленивый парсер [`crate::patch::parse_patch`]: строгий режим является
/// подмножеством принимаемого им, поэтому валидный патч конвертируется
/// всегда. Применимость операций к файловой системе здесь не проверяется
/// (это дело апплаера [`crate::patch::apply_ops`]).
///
/// # Ошибки
/// - патч не прошёл строгую проверку грамматики (перечень нарушений
///   с номерами строк в тексте ошибки);
/// - внутренняя ошибка парсера (недостижимо для прошедших строгую
///   проверку патчей; оставлено как страховка).
pub fn to_patch_ops(patch_text: &str) -> Result<Vec<PatchOp>> {
    let issues = validate_strict(patch_text);
    if !issues.is_empty() {
        let report = issues.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n");
        let count = issues.len();
        bail!("патч отклонён строгой проверкой грамматики ({count} проблем):\n{report}");
    }
    crate::patch::parse_patch(patch_text)
        .context("патч прошёл строгую проверку, но отклонён ленивым парсером")
}

/// Сформировать нарушение с номером строки.
fn issue(line: usize, message: impl Into<String>) -> GrammarIssue {
    GrammarIssue { line, message: message.into() }
}

/// Обработать строку на верхнем уровне (между операциями), вернуть новое состояние.
fn scan_top_level(line: &str, n: usize, issues: &mut Vec<GrammarIssue>) -> ScanState {
    if line.trim().is_empty() {
        // Пустые строки между операциями допустимы (как и в ленивом парсере).
        return ScanState::TopLevel;
    }
    if let Some(rest) = line.strip_prefix(ADD_PREFIX) {
        check_path(rest, n, issues);
        return ScanState::Add { path: rest.to_string(), body_lines: 0 };
    }
    if let Some(rest) = line.strip_prefix(UPDATE_PREFIX) {
        check_path(rest, n, issues);
        return ScanState::Update(UpdateScan::new(rest));
    }
    if let Some(rest) = line.strip_prefix(DELETE_PREFIX) {
        check_path(rest, n, issues);
        return ScanState::TopLevel;
    }
    if line == BEGIN_MARKER || line == END_MARKER {
        issues.push(issue(n, format!("маркер '{line}' допустим только в обрамлении патча")));
    } else if line.starts_with("***") {
        issues.push(issue(
            n,
            format!(
                "неизвестный маркер '{line}'; ожидался '{ADD_PREFIX}', '{UPDATE_PREFIX}' или '{DELETE_PREFIX}'"
            ),
        ));
    } else if line.trim_start().starts_with("***") {
        issues.push(issue(
            n,
            format!(
                "маркер '{}' записан с отступом: маркеры должны начинаться с '***' в первой колонке",
                line.trim()
            ),
        ));
    } else {
        issues.push(issue(
            n,
            format!("строка '{line}' вне секции: ожидался маркер операции ('*** Add/Update/Delete File: <путь>')"),
        ));
    }
    ScanState::TopLevel
}

/// Диагностика для строки тела `Add File` без префикса '+'.
///
/// Частый случай — маркер операции с отступом: ему даём точное сообщение,
/// остальным строкам — общее требование префикса.
fn add_line_message(path: &str, line: &str) -> String {
    if !line.starts_with("***") && line.trim_start().starts_with("***") {
        format!(
            "маркер '{}' записан с отступом: маркеры должны начинаться с '***' в первой колонке",
            line.trim()
        )
    } else {
        format!("в секции Add File '{path}' каждая строка должна начинаться с '+'")
    }
}

/// Проверить хвост маркера операции как путь (строго: непустой, без пробелов).
fn check_path(rest: &str, n: usize, issues: &mut Vec<GrammarIssue>) {
    if rest.is_empty() {
        issues.push(issue(n, "пустой путь в маркере операции"));
    } else if let Some(bad) = rest.chars().find(|c| c.is_whitespace()) {
        let name = match bad {
            ' ' => "пробел",
            '\t' => "табуляция",
            _ => "пробельный символ",
        };
        issues.push(issue(
            n,
            format!("путь '{rest}' содержит недопустимый символ ({name}): в строгом режиме пути без пробелов и табуляций"),
        ));
    }
}

/// Состояние сканера строгой проверки.
#[derive(Default)]
enum ScanState {
    /// Между операциями: ждём маркер или пустую строку.
    #[default]
    TopLevel,
    /// Внутри `*** Add File:`.
    Add {
        /// Путь из маркера (для сообщений об ошибках).
        path: String,
        /// Число строк тела с префиксом '+'.
        body_lines: usize,
    },
    /// Внутри `*** Update File:`.
    Update(UpdateScan),
}

/// Накопитель состояния секции `*** Update File:` при строгой проверке.
struct UpdateScan {
    /// Путь из маркера операции (для сообщений об ошибках).
    path: String,
    /// Первая строка текущего чанка (1-based); 0 — чанк ещё не начат.
    chunk_line: usize,
    /// Число строк изменений в текущем чанке.
    chunk_lines: usize,
    /// Есть ли в текущем чанке хотя бы одна строка '+' или '-'.
    chunk_has_pm: bool,
    /// Всего строк изменений в секции.
    total_lines: usize,
}

impl UpdateScan {
    fn new(path: &str) -> Self {
        Self { path: path.to_string(), chunk_line: 0, chunk_lines: 0, chunk_has_pm: false, total_lines: 0 }
    }

    /// Начать новый чанк (встретилась строка `@@`).
    fn begin_chunk(&mut self, line: usize) {
        self.chunk_line = line;
        self.chunk_lines = 0;
        self.chunk_has_pm = false;
    }

    /// Учесть строку изменений; `pm` — это строка '+' или '-'.
    /// Пустая строка трактуется как контекстная с пустым содержимым
    /// (как в ленивом парсере).
    fn push_line(&mut self, line: usize, pm: bool) {
        if self.chunk_line == 0 {
            self.begin_chunk(line);
        }
        self.chunk_lines += 1;
        self.total_lines += 1;
        self.chunk_has_pm |= pm;
    }

    /// Завершить текущий чанк, зафиксировав нарушения его формы:
    /// пустая секция `@@` либо чанк без единой строки '+'/'-'.
    fn finish_chunk(&mut self, issues: &mut Vec<GrammarIssue>) {
        if self.chunk_line == 0 {
            return;
        }
        if self.chunk_lines == 0 {
            issues.push(issue(
                self.chunk_line,
                "пустая секция '@@': после якоря нет ни одной строки изменений",
            ));
        } else if !self.chunk_has_pm {
            issues.push(issue(
                self.chunk_line,
                "чанк не содержит ни одной строки '+' или '-' (только контекст)",
            ));
        }
        self.chunk_line = 0;
    }

    /// Завершить секцию Update целиком (маркер следующей операции или конца патча).
    fn finish(mut self, term_line: usize, issues: &mut Vec<GrammarIssue>) {
        if self.total_lines == 0 && self.chunk_line == 0 {
            issues.push(issue(term_line, format!("секция Update File '{}' не содержит изменений", self.path)));
        } else {
            self.finish_chunk(issues);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::parse_patch;
    use std::path::PathBuf;

    /// Эталонный валидный патч: все три операции, якорь `@@` с текстом,
    /// пустой якорь `@@`, пустая добавляемая строка.
    const VALID: &str = r#"*** Begin Patch
*** Add File: docs/new.md
+# Title
+
*** Update File: src/lib.rs
@@ fn main() {
 fn main() {
-    old();
+    new();
*** Update File: src/util.rs
-dead
+live
@@
+tail
*** Delete File: tmp/old.txt
*** End Patch"#;

    /// Номера строк всех нарушений (в порядке выдачи).
    fn lines_of(issues: &[GrammarIssue]) -> Vec<usize> {
        issues.iter().map(|it| it.line).collect()
    }

    // ---------- грамматика и дайджест ----------

    #[test]
    fn grammar_contains_all_rules_and_markers() {
        let g = render_grammar();
        for rule in [
            "start:",
            "begin_patch:",
            "end_patch:",
            "hunk:",
            "add_hunk:",
            "add_line:",
            "update_hunk:",
            "update_chunk:",
            "change_context:",
            "change_line:",
            "delete_hunk:",
            "filename:",
        ] {
            assert!(g.contains(rule), "в грамматике нет правила '{rule}'");
        }
        for marker in [BEGIN_MARKER, END_MARKER, ADD_PREFIX, UPDATE_PREFIX, DELETE_PREFIX, "@@"] {
            assert!(g.contains(marker), "в грамматике нет маркера '{marker}'");
        }
        // Грамматика — многострочный документ контракта, не однострочник.
        assert!(g.lines().count() > 20, "грамматика подозрительно короткая");
        // Повторный вызов возвращает тот же текст (нет глобального состояния).
        assert_eq!(g, render_grammar());
    }

    #[test]
    fn digest_is_compact_and_covers_format() {
        let d = grammar_digest();
        // Компактность: влезает в несколько строк и заметно короче грамматики.
        assert!(d.lines().count() <= 9, "дайджест распух: {} строк", d.lines().count());
        assert!(d.len() <= 700, "дайджест распух: {} байт", d.len());
        assert!(d.len() < render_grammar().len(), "дайджест должен быть короче грамматики");
        for token in [BEGIN_MARKER, END_MARKER, "Add File", "Update File", "Delete File", "@@", "+", "-"] {
            assert!(d.contains(token), "в дайджесте нет '{token}'");
        }
    }

    #[test]
    fn issue_display_has_line_prefix() {
        let it = GrammarIssue { line: 7, message: "бум".to_string() };
        assert_eq!(it.to_string(), "строка 7: бум");
    }

    // ---------- строгая проверка: валидные патчи ----------

    #[test]
    fn strict_accepts_valid_patch() {
        let issues = validate_strict(VALID);
        assert!(issues.is_empty(), "валидный патч отклонён: {issues:?}");
        // И ленивый парсер его принимает — договорённость strict ⊆ parse.
        assert!(parse_patch(VALID).is_ok());
    }

    #[test]
    fn strict_accepts_empty_patch_and_trailing_blank_lines() {
        assert!(validate_strict("*** Begin Patch\n*** End Patch").is_empty());
        // Хвостовые пустые строки после маркера конца допустимы.
        let with_tail = format!("{VALID}\n\n\n");
        assert!(validate_strict(&with_tail).is_empty(), "{with_tail}");
    }

    #[test]
    fn strict_accepts_crlf_and_empty_anchor() {
        // CRLF: lines() подрезает '\r', маркеры распознаются.
        let crlf = VALID.replace('\n', "\r\n");
        assert!(validate_strict(&crlf).is_empty());
        // Пустой якорь '@@' допустим (ленивый парсер даёт context = None).
        let p = "*** Begin Patch\n*** Update File: u.txt\n@@\n-x\n+y\n*** End Patch";
        assert!(validate_strict(p).is_empty());
    }

    // ---------- строгая проверка: нарушения с номерами строк ----------

    #[test]
    fn strict_flags_missing_or_indented_begin() {
        let issues = validate_strict("*** Add File: f\n+x\n*** End Patch");
        assert!(issues.iter().any(|it| it.line == 1 && it.message.contains(BEGIN_MARKER)), "{issues:?}");
        // Маркер начала с отступом — отдельная диагностика.
        let issues = validate_strict("  *** Begin Patch\n*** End Patch");
        assert_eq!(lines_of(&issues), vec![1]);
        assert!(issues[0].message.contains("без пробелов по краям"), "{issues:?}");
    }

    #[test]
    fn strict_flags_missing_end_with_last_line_number() {
        let issues = validate_strict("*** Begin Patch\n*** Add File: f\n+x");
        assert_eq!(lines_of(&issues), vec![3]);
        assert!(issues[0].message.contains(END_MARKER), "{issues:?}");
        // Совсем пустой ввод.
        let issues = validate_strict("");
        assert_eq!(lines_of(&issues), vec![1]);
    }

    #[test]
    fn strict_flags_unknown_marker() {
        // Маркер из диалекта codex, которого у нас нет.
        let issues = validate_strict("*** Begin Patch\n*** Move to: x\n*** End Patch");
        assert_eq!(lines_of(&issues), vec![2]);
        assert!(issues[0].message.contains("неизвестный маркер"), "{issues:?}");
    }

    #[test]
    fn strict_flags_indented_operation_marker() {
        let issues = validate_strict("*** Begin Patch\n  *** Add File: f\n+x\n*** End Patch");
        assert_eq!(lines_of(&issues), vec![2, 3]);
        assert!(issues[0].message.contains("с отступом"), "{issues:?}");
        // Строка '+x' после нераспознанного маркера остаётся вне секции.
        assert!(issues[1].message.contains("вне секции"), "{issues:?}");
    }

    #[test]
    fn strict_flags_path_with_whitespace() {
        // Пробел в середине, табуляция, хвостовой пробел — три нарушения.
        let p = "*** Begin Patch\n\
                 *** Add File: my file.txt\n\
                 +x\n\
                 *** Update File: a\tb.txt\n\
                 -x\n\
                 +y\n\
                 *** Delete File: c.txt \n\
                 *** End Patch";
        let issues = validate_strict(p);
        assert_eq!(lines_of(&issues), vec![2, 4, 7]);
        assert!(issues[0].message.contains("пробел"), "{issues:?}");
        assert!(issues[1].message.contains("табуляция"), "{issues:?}");
        assert!(issues[2].message.contains("пробел"), "{issues:?}");
    }

    #[test]
    fn strict_flags_empty_path() {
        let p = "*** Begin Patch\n*** Add File: \n+x\n*** End Patch";
        let issues = validate_strict(p);
        assert_eq!(lines_of(&issues), vec![2]);
        assert!(issues[0].message.contains("пустой путь"), "{issues:?}");
    }

    #[test]
    fn strict_flags_add_body_line_without_plus() {
        // '-', контекстная ' ' и пустая строки в Add — по нарушению на строку,
        // плюс финализация: секция фактически пуста (ни одной '+'-строки).
        let p = "*** Begin Patch\n*** Add File: f\n-removed\n context\n\n*** End Patch";
        let issues = validate_strict(p);
        assert_eq!(lines_of(&issues), vec![3, 4, 5, 6]);
        assert!(issues[..3].iter().all(|it| it.message.contains("должна начинаться с '+'")), "{issues:?}");
        assert!(issues[3].message.contains("не содержит строк"), "{issues:?}");
    }

    #[test]
    fn strict_flags_empty_add_section() {
        // Маркер следующей операции закрывает пустую секцию Add — ошибка на его строке.
        let p = "*** Begin Patch\n*** Add File: f\n*** Delete File: g\n*** End Patch";
        let issues = validate_strict(p);
        assert_eq!(lines_of(&issues), vec![3]);
        assert!(issues[0].message.contains("не содержит строк"), "{issues:?}");
        // Пустая секция Add перед концом патча — ошибка на строке маркера конца.
        let p = "*** Begin Patch\n*** Add File: f\n*** End Patch";
        let issues = validate_strict(p);
        assert_eq!(lines_of(&issues), vec![3]);
    }

    #[test]
    fn strict_flags_context_only_update_chunk() {
        // Чанк с якорем и одним контекстом — ошибка на строке начала чанка.
        let p = "*** Begin Patch\n*** Update File: u\n@@ fn f\n ctx\n*** End Patch";
        let issues = validate_strict(p);
        assert_eq!(lines_of(&issues), vec![3]);
        assert!(issues[0].message.contains("только контекст"), "{issues:?}");
        // То же без якоря: чанк начинается с первой контекстной строки.
        let p = "*** Begin Patch\n*** Update File: u\n only\n*** End Patch";
        let issues = validate_strict(p);
        assert_eq!(lines_of(&issues), vec![3]);
    }

    #[test]
    fn strict_flags_empty_anchor_section() {
        // Два якоря подряд: первая секция '@@' пуста, вторая валидна.
        let p = "*** Begin Patch\n*** Update File: u\n@@ a\n@@ b\n+x\n*** End Patch";
        let issues = validate_strict(p);
        assert_eq!(lines_of(&issues), vec![3]);
        assert!(issues[0].message.contains("пустая секция '@@'"), "{issues:?}");
    }

    #[test]
    fn strict_flags_bad_update_line_prefix() {
        let p = "*** Begin Patch\n*** Update File: u\n?what\n-x\n*** End Patch";
        let issues = validate_strict(p);
        assert_eq!(lines_of(&issues), vec![3]);
        assert!(issues[0].message.contains("должна начинаться с"), "{issues:?}");
    }

    #[test]
    fn strict_flags_update_without_any_changes() {
        let p = "*** Begin Patch\n*** Update File: u\n*** End Patch";
        let issues = validate_strict(p);
        assert_eq!(lines_of(&issues), vec![3]);
        assert!(issues[0].message.contains("не содержит изменений"), "{issues:?}");
    }

    #[test]
    fn strict_collects_multiple_issues_sorted_by_line() {
        // Пять нарушений шести видов в одном патче; маркер конца битый,
        // поэтому проверка конца срабатывает раньше сканирования тела —
        // итоговый список всё равно обязан быть отсортирован по строкам.
        let p = "*** Begin Patch\n\
                 *** Add File: f\n\
                 -oops\n\
                 *** Add File: g h\n\
                 +x\n\
                 *** End Patch broken\n";
        let issues = validate_strict(p);
        assert_eq!(lines_of(&issues), vec![3, 4, 4, 6, 6]);
        assert!(issues[0].message.contains("начинаться с '+'"), "{issues:?}");
        assert!(issues[1].message.contains("не содержит строк"), "{issues:?}");
        assert!(issues[2].message.contains("пробел"), "{issues:?}");
        assert!(issues[3].message.contains(END_MARKER), "{issues:?}");
        assert!(issues[4].message.contains("неизвестный маркер"), "{issues:?}");
    }

    // ---------- конвертация в PatchOp ----------

    #[test]
    fn to_patch_ops_roundtrip_matches_lazy_parser() {
        let ops = to_patch_ops(VALID).unwrap();
        // Roundtrip: результат совпадает с эталонным ленивым парсером.
        assert_eq!(ops, parse_patch(VALID).unwrap());
        assert_eq!(ops.len(), 4);
        match &ops[0] {
            PatchOp::Add { path, contents } => {
                assert_eq!(path, &PathBuf::from("docs/new.md"));
                // Строка "+" даёт пустую строку содержимого.
                assert_eq!(contents, "# Title\n\n");
            }
            other => panic!("ожидался Add, получено {other:?}"),
        }
        match &ops[1] {
            PatchOp::Update { path, chunks } => {
                assert_eq!(path, &PathBuf::from("src/lib.rs"));
                assert_eq!(chunks.len(), 1);
                assert_eq!(chunks[0].context.as_deref(), Some("fn main() {"));
                assert_eq!(chunks[0].old_lines, ["fn main() {", "    old();"]);
                assert_eq!(chunks[0].new_lines, ["fn main() {", "    new();"]);
                assert_eq!(chunks[0].patch_line, 6);
            }
            other => panic!("ожидался Update, получено {other:?}"),
        }
        match &ops[2] {
            PatchOp::Update { path, chunks } => {
                assert_eq!(path, &PathBuf::from("src/util.rs"));
                assert_eq!(chunks.len(), 2);
                assert_eq!(chunks[0].old_lines, ["dead"]);
                assert_eq!(chunks[0].new_lines, ["live"]);
                assert_eq!(chunks[0].patch_line, 11);
                // Пустой якорь '@@' — чанк без контекста, чистая вставка.
                assert_eq!(chunks[1].context, None);
                assert!(chunks[1].old_lines.is_empty());
                assert_eq!(chunks[1].new_lines, ["tail"]);
                assert_eq!(chunks[1].patch_line, 13);
            }
            other => panic!("ожидался Update, получено {other:?}"),
        }
        assert_eq!(ops[3], PatchOp::Delete { path: PathBuf::from("tmp/old.txt") });
        // Пустой патч конвертируется в пустой список операций.
        assert!(to_patch_ops("*** Begin Patch\n*** End Patch").unwrap().is_empty());
    }

    #[test]
    fn to_patch_ops_rejects_strict_violations_with_all_lines() {
        // Чанк из одного контекста ленивый парсер принимает (old == new),
        // а to_patch_ops обязан отклонить его до вызова парсера.
        let bad = "*** Begin Patch\n*** Update File: u\n only\n*** End Patch";
        assert!(parse_patch(bad).is_ok(), "контраст: ленивый парсер принимает");
        let err = to_patch_ops(bad).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("строка 3"), "{msg}");
        assert!(msg.contains("отклонён"), "{msg}");
        // Несколько нарушений — все попадают в текст ошибки: строка без '+',
        // отсюда пустая первая секция Add, пустой путь и пустая вторая секция.
        let bad = "*** Begin Patch\n*** Add File: f\n-oops\n*** Add File: \n*** End Patch";
        let msg = to_patch_ops(bad).unwrap_err().to_string();
        assert!(msg.contains("строка 3"), "{msg}");
        assert!(msg.contains("строка 4"), "{msg}");
        assert!(msg.contains("строка 5"), "{msg}");
        assert!(msg.contains("(4 проблем)"), "{msg}");
    }
}
