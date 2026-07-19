//! Модуль `diffview` — рендер unified-diff для показа правок `edit_file`/`patch`
//! в TUI и headless-режиме агентного харнесса.
//!
//! Модуль самодостаточен (только `std`). Построчный diff строится на классическом
//! LCS с динамическим программированием: файлы правок в харнессе маленькие, поэтому
//! O(n·m) по времени и памяти приемлем. Защита от квадратичного взрыва: если хотя
//! бы одна из сторон длиннее [`MAX_DIFF_LINES`] строк, вместо LCS используется
//! наивный replace-блок («всё удалено, всё добавлено»).
//!
//! Входные точки:
//! - [`unified_diff`] — текстовый unified-diff (заголовки `---`/`+++`, ханки `@@`);
//! - [`compute_hunks`] — тот же diff в структурном виде (подсветка и скроллинг в TUI);
//! - [`stat_summary`] — пара «(добавлено, удалено)» в духе `git diff --numstat`;
//! - [`colorize_for_terminal`] — ANSI-раскраска готового diff-текста.

/// Максимальная длина стороны (в строках), до которой строится точный LCS-diff.
///
/// Свыше порога применяется наивный replace-блок, чтобы DP-таблица O(n·m)
/// не разрасталась (при 2000×2000 она уже занимает ~16 МБ).
pub const MAX_DIFF_LINES: usize = 2000;

/// ANSI-код зелёного цвета (добавленные строки).
const ANSI_GREEN: &str = "\u{1b}[32m";
/// ANSI-код красного цвета (удалённые строки).
const ANSI_RED: &str = "\u{1b}[31m";
/// ANSI-код циана (заголовки ханков).
const ANSI_CYAN: &str = "\u{1b}[36m";
/// ANSI-код сброса цвета.
const ANSI_RESET: &str = "\u{1b}[0m";

/// Элементарная операция выравнивания одной строки старой и новой версий.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineOp {
    /// Строка совпадает в обеих версиях (контекст).
    Equal,
    /// Строка есть только в старой версии (удаление).
    Delete,
    /// Строка есть только в новой версии (добавление).
    Insert,
}

/// Одна строка внутри ханка unified-diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    /// Контекстная строка, одинаковая в обеих версиях (префикс `' '`).
    Context(String),
    /// Удалённая строка из старой версии (префикс `'-'`).
    Delete(String),
    /// Добавленная строка новой версии (префикс `'+'`).
    Insert(String),
}

impl DiffLine {
    /// Префикс строки в unified-diff: `' '`, `'-'` или `'+'`.
    pub fn prefix(&self) -> char {
        match self {
            Self::Context(_) => ' ',
            Self::Delete(_) => '-',
            Self::Insert(_) => '+',
        }
    }

    /// Текст строки без префикса.
    pub fn text(&self) -> &str {
        match self {
            Self::Context(s) | Self::Delete(s) | Self::Insert(s) => s,
        }
    }
}

/// Ханк — непрерывный фрагмент изменений вместе с контекстом вокруг них.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// Начальная строка в старой версии (1-based; при `old_count == 0` —
    /// номер строки, ПОСЛЕ которой идёт вставка, как в GNU diff).
    pub old_start: usize,
    /// Число строк старой версии в ханке (контекст + удаления).
    pub old_count: usize,
    /// Начальная строка в новой версии (1-based; при `new_count == 0` —
    /// номер строки, после которой идёт удаление).
    pub new_start: usize,
    /// Число строк новой версии в ханке (контекст + вставки).
    pub new_count: usize,
    /// Строки ханка в порядке вывода.
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// Заголовок ханка вида `@@ -1,3 +1,4 @@`.
    ///
    /// GNU-формат: счётчик `,1` опускается, нулевой счётчик пишется как `,0`.
    pub fn header(&self) -> String {
        format!(
            "@@ -{} +{} @@",
            format_range(self.old_start, self.old_count),
            format_range(self.new_start, self.new_count)
        )
    }
}

/// Диапазон в заголовке ханка: `start` при count == 1, иначе `start,count`.
fn format_range(start: usize, count: usize) -> String {
    if count == 1 {
        format!("{start}")
    } else {
        format!("{start},{count}")
    }
}

/// Разбивает текст на строки без переводов строк.
///
/// Используется семантика `str::lines`: различие только в финальном `\n`
/// дифом не отображается (сознательное упрощение для маленьких правок).
fn split_lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

/// Вычисляет последовательность операций выравнивания old → new.
///
/// Точный LCS (плоская DP-таблица (n+1)×(m+1) из u32) для файлов не длиннее
/// [`MAX_DIFF_LINES`] строк; для больших — наивный replace-блок. Идентичные
/// файлы в обоих случаях дают сплошной `Equal`, т.е. пустой diff.
fn diff_ops(old: &[&str], new: &[&str]) -> Vec<LineOp> {
    let n = old.len();
    let m = new.len();
    if n > MAX_DIFF_LINES || m > MAX_DIFF_LINES {
        return naive_ops(old, new);
    }
    // dp[i][j] — длина LCS суффиксов old[i..] и new[j..]; строка i идёт блоками width.
    let width = m + 1;
    let mut dp = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * width + j] = if old[i] == new[j] {
                dp[(i + 1) * width + j + 1] + 1
            } else {
                dp[(i + 1) * width + j].max(dp[i * width + j + 1])
            };
        }
    }
    // Восстановление одного из оптимальных выравниваний. При равенстве длин
    // предпочитаем удаление перед вставкой — удалённые строки идут в diff
    // раньше добавленных, как принято в git.
    let mut ops = Vec::with_capacity(n.max(m));
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old[i] == new[j] {
            ops.push(LineOp::Equal);
            i += 1;
            j += 1;
        } else if dp[(i + 1) * width + j] >= dp[i * width + j + 1] {
            ops.push(LineOp::Delete);
            i += 1;
        } else {
            ops.push(LineOp::Insert);
            j += 1;
        }
    }
    // Хвосты: всё оставшееся в old — удаления, всё оставшееся в new — вставки.
    ops.resize(ops.len() + (n - i), LineOp::Delete);
    ops.resize(ops.len() + (m - j), LineOp::Insert);
    ops
}

/// Наивное выравнивание для файлов свыше лимита: `Equal` для идентичных,
/// иначе сплошной блок удалений, затем сплошной блок вставок.
fn naive_ops(old: &[&str], new: &[&str]) -> Vec<LineOp> {
    if old == new {
        return vec![LineOp::Equal; old.len()];
    }
    let mut ops = vec![LineOp::Delete; old.len()];
    ops.resize(old.len() + new.len(), LineOp::Insert);
    ops
}

/// Группирует операции в ханки с `context` строк контекста вокруг изменений.
///
/// Изменения, разделённые не более чем `2 * context` общими строками,
/// сливаются в один ханк (поведение `diff -U`).
fn build_hunks(ops: &[LineOp], old: &[&str], new: &[&str], context: usize) -> Vec<Hunk> {
    let changes: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter_map(|(idx, op)| (*op != LineOp::Equal).then_some(idx))
        .collect();
    if changes.is_empty() {
        return Vec::new();
    }
    // Группировка: расстояние между соседними изменениями idx - prev - 1 —
    // это число Equal-строк между ними; слияние при gap <= 2*context,
    // т.е. при idx - prev <= 2*context + 1.
    let merge_gap = context.saturating_mul(2).saturating_add(1);
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for idx in changes {
        let extend_last = matches!(groups.last(), Some(last) if idx - last.1 <= merge_gap);
        if extend_last {
            if let Some(last) = groups.last_mut() {
                last.1 = idx;
            }
        } else {
            groups.push((idx, idx));
        }
    }
    groups
        .into_iter()
        .map(|(first, last)| {
            let start = first.saturating_sub(context);
            let end = last.saturating_add(context).saturating_add(1).min(ops.len());
            slice_hunk(ops, old, new, start, end)
        })
        .collect()
}

/// Вырезает ханк по диапазону операций `[start, end)`, вычисляя номера строк
/// и раскладывая операции в [`DiffLine`].
fn slice_hunk(ops: &[LineOp], old: &[&str], new: &[&str], start: usize, end: usize) -> Hunk {
    // Сколько строк каждой версии встретилось до начала ханка (для 1-based номеров).
    let old_before = ops[..start]
        .iter()
        .filter(|op| matches!(op, LineOp::Equal | LineOp::Delete))
        .count();
    let new_before = ops[..start]
        .iter()
        .filter(|op| matches!(op, LineOp::Equal | LineOp::Insert))
        .count();
    let hunk_ops = &ops[start..end];
    let old_count = hunk_ops
        .iter()
        .filter(|op| matches!(op, LineOp::Equal | LineOp::Delete))
        .count();
    let new_count = hunk_ops
        .iter()
        .filter(|op| matches!(op, LineOp::Equal | LineOp::Insert))
        .count();
    // При нулевом счётчике GNU diff указывает строку ПЕРЕД местом изменения.
    let old_start = if old_count == 0 { old_before } else { old_before + 1 };
    let new_start = if new_count == 0 { new_before } else { new_before + 1 };
    let mut lines = Vec::with_capacity(end - start);
    let (mut oi, mut ni) = (old_before, new_before);
    for op in hunk_ops {
        match op {
            LineOp::Equal => {
                lines.push(DiffLine::Context(old[oi].to_string()));
                oi += 1;
                ni += 1;
            }
            LineOp::Delete => {
                lines.push(DiffLine::Delete(old[oi].to_string()));
                oi += 1;
            }
            LineOp::Insert => {
                lines.push(DiffLine::Insert(new[ni].to_string()));
                ni += 1;
            }
        }
    }
    Hunk {
        old_start,
        old_count,
        new_start,
        new_count,
        lines,
    }
}

/// Строит список ханков для пары текстов с `context` строк контекста вокруг изменений.
///
/// Структурное представление того же diff, что рендерит [`unified_diff`]:
/// удобно для TUI (подсветка, сворачивание, скроллинг). Пустой вектор означает
/// «тексты идентичны».
pub fn compute_hunks(old: &str, new: &str, context: usize) -> Vec<Hunk> {
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let ops = diff_ops(&old_lines, &new_lines);
    build_hunks(&ops, &old_lines, &new_lines, context)
}

/// Рендерит unified-diff между старой и новой версиями файла `path`.
///
/// Формат: git-стиль заголовков `--- a/{path}` / `+++ b/{path}`, затем ханки
/// `@@ -start,count +start,count @@` со строками-префиксами `' '`, `'-'`, `'+'`.
/// Для идентичных текстов возвращает пустую строку (заголовки не печатаются).
pub fn unified_diff(old: &str, new: &str, path: &str, context: usize) -> String {
    let hunks = compute_hunks(old, new, context);
    if hunks.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("--- a/");
    out.push_str(path);
    out.push('\n');
    out.push_str("+++ b/");
    out.push_str(path);
    out.push('\n');
    for hunk in &hunks {
        out.push_str(&hunk.header());
        out.push('\n');
        for line in &hunk.lines {
            out.push(line.prefix());
            out.push_str(line.text());
            out.push('\n');
        }
    }
    out
}

/// Сводка в стиле `git diff --numstat`: `(добавлено, удалено)` строк.
///
/// Контекстные строки не считаются; для идентичных текстов — `(0, 0)`.
pub fn stat_summary(old: &str, new: &str) -> (usize, usize) {
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let ops = diff_ops(&old_lines, &new_lines);
    let mut added = 0usize;
    let mut deleted = 0usize;
    for op in ops {
        match op {
            LineOp::Insert => added += 1,
            LineOp::Delete => deleted += 1,
            LineOp::Equal => {}
        }
    }
    (added, deleted)
}

/// Раскрашивает готовый unified-diff ANSI-кодами для терминала: зелёные
/// `+`-строки, красные `-`-строки, циан для заголовков `@@`.
///
/// Заголовки файлов (`--- `, `+++ `) и контекстные строки выводятся без цвета.
/// Последняя строка без терминального `\n` обрабатывается так же, как остальные.
pub fn colorize_for_terminal(diff: &str) -> String {
    let mut out = String::with_capacity(diff.len() + diff.len() / 8 + 16);
    for chunk in diff.split_inclusive('\n') {
        let (text, eol) = match chunk.strip_suffix('\n') {
            Some(text) => (text, "\n"),
            None => (chunk, ""),
        };
        let color = if text.starts_with("@@") {
            Some(ANSI_CYAN)
        } else if text.starts_with("+++") || text.starts_with("---") {
            None
        } else if text.starts_with('+') {
            Some(ANSI_GREEN)
        } else if text.starts_with('-') {
            Some(ANSI_RED)
        } else {
            None
        };
        match color {
            Some(code) => {
                out.push_str(code);
                out.push_str(text);
                out.push_str(ANSI_RESET);
                out.push_str(eol);
            }
            None => out.push_str(chunk),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Генерирует текст из `n` строк вида `{prefix}0` … `{prefix}{n-1}`.
    fn make_lines(prefix: &str, n: usize) -> String {
        (0..n).map(|i| format!("{prefix}{i}\n")).collect()
    }

    #[test]
    fn identical_files_give_empty_diff() {
        let text = "line1\nline2\nline3\n";
        assert!(unified_diff(text, text, "f", 0).is_empty());
        assert!(unified_diff(text, text, "f", 3).is_empty());
        assert!(compute_hunks(text, text, 3).is_empty());
    }

    #[test]
    fn empty_vs_empty() {
        assert!(unified_diff("", "", "f", 3).is_empty());
        assert_eq!(stat_summary("", ""), (0, 0));
    }

    #[test]
    fn pure_addition_from_empty_file() {
        let diff = unified_diff("", "one\ntwo\n", "new.txt", 3);
        assert!(diff.starts_with("--- a/new.txt\n+++ b/new.txt\n"));
        assert!(diff.contains("@@ -0,0 +1,2 @@\n"));
        assert!(diff.contains("+one\n"));
        assert!(diff.contains("+two\n"));
        assert!(!diff
            .lines()
            .any(|line| line.starts_with('-') && !line.starts_with("---")));
        assert_eq!(stat_summary("", "one\ntwo\n"), (2, 0));
    }

    #[test]
    fn pure_deletion_to_empty_file() {
        let diff = unified_diff("one\ntwo\n", "", "old.txt", 3);
        assert!(diff.contains("@@ -1,2 +0,0 @@\n"));
        assert!(diff.contains("-one\n-two\n"));
        assert!(!diff
            .lines()
            .any(|line| line.starts_with('+') && !line.starts_with("+++")));
        assert_eq!(stat_summary("one\ntwo\n", ""), (0, 2));
    }

    #[test]
    fn single_line_change_exact_render() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\n";
        let diff = unified_diff(old, new, "f.txt", 3);
        let expected = "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        assert_eq!(diff, expected);
        assert_eq!(stat_summary(old, new), (1, 1));
    }

    #[test]
    fn multiline_replace_block_exact_render() {
        let old = "1\n2\n3\n4\n5\n";
        let new = "1\n2\nX\nY\nZ\n5\n";
        let diff = unified_diff(old, new, "f", 1);
        let expected = "--- a/f\n+++ b/f\n@@ -2,4 +2,5 @@\n 2\n-3\n-4\n+X\n+Y\n+Z\n 5\n";
        assert_eq!(diff, expected);
        assert_eq!(stat_summary(old, new), (3, 2));
    }

    #[test]
    fn header_omits_count_when_one() {
        let diff = unified_diff("x\n", "y\n", "f", 0);
        assert!(diff.contains("@@ -1 +1 @@\n"));
        assert!(diff.contains("-x\n+y\n"));
    }

    #[test]
    fn headers_use_git_style_paths() {
        let diff = unified_diff("a\n", "b\n", "src/main.rs", 0);
        assert!(diff.starts_with("--- a/src/main.rs\n+++ b/src/main.rs\n"));
    }

    #[test]
    fn insertion_in_middle_zero_context() {
        // Вставка после строки 2: старый счётчик 0 → номер строки ПЕРЕД вставкой.
        let diff = unified_diff("a\nb\nc\n", "a\nb\nX\nY\nc\n", "f", 0);
        assert!(diff.contains("@@ -2,0 +3,2 @@\n"));
        assert!(diff.contains("+X\n+Y\n"));
        assert_eq!(stat_summary("a\nb\nc\n", "a\nb\nX\nY\nc\n"), (2, 0));
    }

    #[test]
    fn deletion_in_middle_zero_context() {
        // Удаление строк 2..3: новый счётчик 0 → номер строки ПЕРЕД удалением.
        let diff = unified_diff("a\nX\nY\nc\n", "a\nc\n", "f", 0);
        assert!(diff.contains("@@ -2,2 +1,0 @@\n"));
        assert!(diff.contains("-X\n-Y\n"));
        assert_eq!(stat_summary("a\nX\nY\nc\n", "a\nc\n"), (0, 2));
    }

    #[test]
    fn context_zero_has_no_context_lines() {
        let diff = unified_diff("a\nb\nc\nd\ne\n", "a\nb\nX\nd\ne\n", "f", 0);
        assert!(diff.contains("@@ -3 +3 @@"));
        assert!(!diff.lines().any(|line| line.starts_with(' ')));
    }

    #[test]
    fn close_changes_merge_into_one_hunk() {
        // Правки в строках 5 и 10 (индексы 4 и 9): между ними 4 общие строки,
        // что <= 2*context (6) → один общий ханк.
        let old = make_lines("l", 20);
        let mut lines: Vec<String> = (0..20).map(|i| format!("l{i}")).collect();
        lines[4] = "CHANGED4".to_string();
        lines[9] = "CHANGED9".to_string();
        let new = format!("{}\n", lines.join("\n"));
        let hunks = compute_hunks(&old, &new, 3);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 2);
        assert_eq!(hunks[0].old_count, 12);
        assert_eq!(hunks[0].new_count, 12);
    }

    #[test]
    fn far_changes_split_into_two_hunks() {
        // Правки в строках 2 и 17 (индексы 1 и 16): при context=1 разрыв 14 > 2.
        let old = make_lines("l", 20);
        let mut lines: Vec<String> = (0..20).map(|i| format!("l{i}")).collect();
        lines[1] = "A".to_string();
        lines[16] = "B".to_string();
        let new = format!("{}\n", lines.join("\n"));
        let hunks = compute_hunks(&old, &new, 1);
        assert_eq!(hunks.len(), 2);
        assert_eq!((hunks[0].old_start, hunks[0].old_count), (1, 3));
        assert_eq!((hunks[1].old_start, hunks[1].old_count), (16, 3));
        let diff = unified_diff(&old, &new, "f", 1);
        assert_eq!(diff.lines().filter(|l| l.starts_with("@@")).count(), 2);
    }

    #[test]
    fn merge_boundary_exactly_two_contexts() {
        // Правки в индексах 1 и 6: между ними ровно 4 общие строки.
        let old = make_lines("l", 12);
        let mut lines: Vec<String> = (0..12).map(|i| format!("l{i}")).collect();
        lines[1] = "A".to_string();
        lines[6] = "B".to_string();
        let new = format!("{}\n", lines.join("\n"));
        // context=2: 4 == 2*2 → слияние в один ханк.
        assert_eq!(compute_hunks(&old, &new, 2).len(), 1);
        // context=1: 4 > 2*1 → два раздельных ханка.
        assert_eq!(compute_hunks(&old, &new, 1).len(), 2);
    }

    #[test]
    fn stat_summary_mixed_changes() {
        // b → X (1 удаление + 1 вставка) и добавлены e, f.
        let old = "a\nb\nc\nd\n";
        let new = "a\nX\nc\nd\ne\nf\n";
        assert_eq!(stat_summary(old, new), (3, 1));
    }

    #[test]
    fn stat_summary_identical_is_zero() {
        assert_eq!(stat_summary("a\nb\n", "a\nb\n"), (0, 0));
    }

    #[test]
    fn colorize_paints_plus_minus_and_hunks() {
        let diff = unified_diff("a\nb\nc\n", "a\nB\nc\n", "f", 1);
        let colored = colorize_for_terminal(&diff);
        assert!(colored.contains("\u{1b}[36m@@ -1,3 +1,3 @@\u{1b}[0m\n"));
        assert!(colored.contains("\u{1b}[31m-b\u{1b}[0m\n"));
        assert!(colored.contains("\u{1b}[32m+B\u{1b}[0m\n"));
        // Контекст остаётся без цвета.
        assert!(colored.contains(" a\n"));
        // Последняя строка без терминального \n тоже красится.
        assert_eq!(colorize_for_terminal("+x"), "\u{1b}[32m+x\u{1b}[0m");
    }

    #[test]
    fn colorize_leaves_headers_and_plain_text_untouched() {
        let diff = unified_diff("a\nb\nc\n", "a\nB\nc\n", "f", 1);
        let colored = colorize_for_terminal(&diff);
        assert!(colored.contains("--- a/f\n"));
        assert!(colored.contains("+++ b/f\n"));
        assert!(!colored.contains("\u{1b}[31m---"));
        assert!(!colored.contains("\u{1b}[32m+++"));
        let plain = "hello world\njust text\n";
        assert_eq!(colorize_for_terminal(plain), plain);
        assert_eq!(colorize_for_terminal("no newline"), "no newline");
        assert!(colorize_for_terminal("").is_empty());
    }

    #[test]
    fn fallback_over_limit_uses_replace_block() {
        let old = make_lines("old", MAX_DIFF_LINES + 500);
        let new = make_lines("new", MAX_DIFF_LINES + 500);
        let hunks = compute_hunks(&old, &new, 3);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].header(), "@@ -1,2500 +1,2500 @@");
        assert_eq!(stat_summary(&old, &new), (2500, 2500));
    }

    #[test]
    fn fallback_identical_large_files_empty() {
        let text = make_lines("same", MAX_DIFF_LINES + 1000);
        assert!(unified_diff(&text, &text, "big", 3).is_empty());
        assert_eq!(stat_summary(&text, &text), (0, 0));
    }

    #[test]
    fn trailing_newline_difference_ignored() {
        // Сознательное упрощение: diff построчный, финальный \n не учитывается.
        assert!(unified_diff("a\n", "a", "f", 3).is_empty());
        assert_eq!(stat_summary("a\n", "a"), (0, 0));
    }

    #[test]
    fn hunk_struct_fields_and_helpers() {
        let hunks = compute_hunks("a\nb\nc\n", "a\nB\nc\n", 0);
        assert_eq!(hunks.len(), 1);
        let hunk = &hunks[0];
        assert_eq!(hunk.header(), "@@ -2 +2 @@");
        assert_eq!((hunk.old_start, hunk.old_count), (2, 1));
        assert_eq!((hunk.new_start, hunk.new_count), (2, 1));
        assert_eq!(
            hunk.lines,
            vec![
                DiffLine::Delete("b".to_string()),
                DiffLine::Insert("B".to_string())
            ]
        );
        assert_eq!(hunk.lines[0].prefix(), '-');
        assert_eq!(hunk.lines[1].prefix(), '+');
        assert_eq!(hunk.lines[1].text(), "B");
        let ctx = DiffLine::Context("x".to_string());
        assert_eq!(ctx.prefix(), ' ');
        assert_eq!(ctx.text(), "x");
    }
}
