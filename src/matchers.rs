//! Каскад нечётких матчеров для инструмента `edit_file` (по образцу Claude Code).
//!
//! Модель нередко присылает `old_string`, который не совпадает с содержимым
//! файла байт-в-байт: лишние или пропущенные пробелы, переотступ блока,
//! экранированные `\n` вместо реальных переводов строк. Вместо одного
//! `str::find` блок ищется каскадом от строгих способов к лояльным:
//!
//! 1. [`MatchKind::Exact`] — точное подстроковое совпадение;
//! 2. [`MatchKind::LineTrim`] — построчное сравнение с `trim` каждой строки
//!    с обеих сторон;
//! 3. [`MatchKind::BlockAnchor`] — совпадение по якорям: первые и последние
//!    две непустые строки блока (середина окна может расходиться);
//! 4. [`MatchKind::WhitespaceNormalized`] — сравнение после схлопывания всех
//!    пробельных последовательностей;
//! 5. [`MatchKind::IndentFlexible`] — сравнение после снятия общего отступа
//!    с обеих сторон;
//! 6. [`MatchKind::EscapeNormalized`] — поиск после разэкранирования
//!    `\n`/`\t`/`\r` в `needle`;
//! 7. [`MatchKind::TrimmedBlock`] — точный поиск блока, обрезанного по краям
//!    (`needle.trim()`);
//! 8. [`MatchKind::ContextMatch`] — окно с максимальной долей совпадающих
//!    строк (не ниже 60%);
//! 9. multi-occurrence — не отдельный матчер, а вентиль каждого уровня:
//!    если кандидатов больше одного — [`MatchError::Ambiguous`] со списком
//!    строк-кандидатов, а не молчаливая правка первого попавшегося.
//!
//! Первый уровень, давший ровно один кандидат, побеждает: строгое совпадение
//! всегда приоритетнее нечёткого. Диапазоны [`Match`] — байтовые смещения
//! в исходном тексте (всегда на границах символов UTF-8); номера строк
//! в ошибках — 1-based.
//!
//! Замечание о порядке уровней: [`MatchKind::IndentFlexible`] идёт после
//! [`MatchKind::LineTrim`], который уже поглощает чистые различия отступов,
//! поэтому в живом каскаде пятый уровень срабатывает редко; он сохранён
//! для паритета с эталонным каскадом и покрыт прямыми тестами.

#![forbid(unsafe_code)]

use std::fmt;

/// Порог контекстного совпадения: доля позиционно совпадающих строк окна.
/// Сравнение целочисленное, без потерь точности: `hits * DEN >= total * NUM`
/// (60% = 3/5).
const CONTEXT_MATCH_NUM: usize = 3;
const CONTEXT_MATCH_DEN: usize = 5;

/// Сколько непустых строк с каждого края блока берётся как якоря.
const BLOCK_ANCHOR_EDGE_LINES: usize = 2;

/// Уровень каскада, на котором найдено совпадение.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MatchKind {
    /// 1. Точное подстроковое совпадение.
    Exact,
    /// 2. Построчное сравнение с `trim` каждой строки с обеих сторон.
    LineTrim,
    /// 3. Совпадение по якорям первых/последних двух непустых строк блока.
    BlockAnchor,
    /// 4. Сравнение после схлопывания всех пробельных последовательностей.
    WhitespaceNormalized,
    /// 5. Сравнение после снятия общего отступа с обеих сторон.
    IndentFlexible,
    /// 6. Совпадение после разэкранирования `\n`/`\t`/`\r` в `needle`.
    EscapeNormalized,
    /// 7. Точное совпадение блока, обрезанного по краям (`needle.trim()`).
    TrimmedBlock,
    /// 8. Контекстное совпадение: окно с долей совпадающих строк >= 60%.
    ContextMatch,
}

impl MatchKind {
    /// Стабильное имя уровня для логов и телеметрии.
    pub fn as_str(&self) -> &'static str {
        match self {
            MatchKind::Exact => "exact",
            MatchKind::LineTrim => "line_trim",
            MatchKind::BlockAnchor => "block_anchor",
            MatchKind::WhitespaceNormalized => "whitespace_normalized",
            MatchKind::IndentFlexible => "indent_flexible",
            MatchKind::EscapeNormalized => "escape_normalized",
            MatchKind::TrimmedBlock => "trimmed_block",
            MatchKind::ContextMatch => "context_match",
        }
    }
}

impl fmt::Display for MatchKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ru = match self {
            MatchKind::Exact => "точное совпадение",
            MatchKind::LineTrim => "построчный trim",
            MatchKind::BlockAnchor => "якоря блока",
            MatchKind::WhitespaceNormalized => "нормализация пробелов",
            MatchKind::IndentFlexible => "гибкие отступы",
            MatchKind::EscapeNormalized => "разэкранирование",
            MatchKind::TrimmedBlock => "обрезанный блок",
            MatchKind::ContextMatch => "контекстное совпадение",
        };
        f.write_str(ru)
    }
}

/// Найденное совпадение блока в исходном тексте.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// Байтовое смещение начала совпадения (включительно).
    pub start: usize,
    /// Байтовое смещение конца совпадения (не включительно).
    pub end: usize,
    /// Уровень каскада, нашедший совпадение.
    pub kind: MatchKind,
}

impl Match {
    /// Срез исходного текста, занимаемый совпадением (его и заменяет правка).
    ///
    /// # Panics
    /// Паника при диапазоне вне `haystack` либо не на границе символа;
    /// диапазоны, выданные [`find_match`], этим гарантиям удовлетворяют.
    pub fn as_str<'a>(&self, haystack: &'a str) -> &'a str {
        &haystack[self.start..self.end]
    }
}

/// Ошибка сопоставления блока.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchError {
    /// Блок не найден ни на одном уровне каскада.
    ///
    /// `closest` — 1-based номер строки начала наиболее похожего окна
    /// (по доле совпадающих строк), если похожее место вообще есть.
    NotFound { closest: Option<usize> },
    /// Найдено несколько кандидатов: правка была бы неоднозначной.
    ///
    /// `lines` — отсортированные 1-based номера строк начала кандидатов
    /// (дубли строк убраны). Уточните `old_string` дополнительным контекстом.
    Ambiguous { lines: Vec<usize> },
}

impl fmt::Display for MatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MatchError::NotFound { closest } => match closest {
                Some(line) => {
                    write!(f, "блок для замены не найден; ближайшее похожее место — строка {line}")
                }
                None => f.write_str("блок для замены не найден"),
            },
            MatchError::Ambiguous { lines } => {
                let list = lines.iter().map(usize::to_string).collect::<Vec<_>>().join(", ");
                write!(
                    f,
                    "блок для замены неоднозначен: {} совпадений (строки: {list}); \
                     добавьте контекста в old_string",
                    lines.len()
                )
            }
        }
    }
}

impl std::error::Error for MatchError {}

/// Сигнатура уровня каскада: все кандидаты как байтовые диапазоны `[start, end)`.
type MatcherFn = fn(&str, &str) -> Vec<(usize, usize)>;

/// Каскад уровней от строгого к лояльному (порядок значим).
const CASCADE: [(MatchKind, MatcherFn); 8] = [
    (MatchKind::Exact, match_exact),
    (MatchKind::LineTrim, match_line_trim),
    (MatchKind::BlockAnchor, match_block_anchor),
    (MatchKind::WhitespaceNormalized, match_whitespace_normalized),
    (MatchKind::IndentFlexible, match_indent_flexible),
    (MatchKind::EscapeNormalized, match_escape_normalized),
    (MatchKind::TrimmedBlock, match_trimmed_block),
    (MatchKind::ContextMatch, match_context),
];

/// Ищет блок `needle` в тексте `haystack` каскадом матчеров.
///
/// Уровни перебираются от строгого к лояльному (см. документацию модуля);
/// возвращается первый уровень с ровно одним кандидатом.
///
/// # Errors
/// * [`MatchError::Ambiguous`] — на первом же сработавшем уровне кандидатов
///   больше одного (вентиль multi-occurrence): молчаливая правка запрещена.
/// * [`MatchError::NotFound`] — ни один уровень не нашёл кандидатов;
///   `closest` подсказывает строку наиболее похожего окна.
pub fn find_match(haystack: &str, needle: &str) -> Result<Match, MatchError> {
    // Пустой needle вырожден: «совпадает» в каждой позиции. Считаем не найденным.
    if needle.is_empty() {
        return Err(MatchError::NotFound { closest: None });
    }
    for (kind, matcher) in CASCADE {
        let candidates = matcher(haystack, needle);
        match candidates.len() {
            0 => continue,
            1 => {
                let (start, end) = candidates[0];
                return Ok(Match { start, end, kind });
            }
            _ => {
                return Err(MatchError::Ambiguous {
                    lines: candidate_lines(haystack, &candidates),
                });
            }
        }
    }
    Err(MatchError::NotFound {
        closest: closest_line(haystack, needle),
    })
}

// ---------------------------------------------------------------------------
// Уровни каскада
// ---------------------------------------------------------------------------

/// 1. Точное подстроковое совпадение.
fn match_exact(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    haystack
        .match_indices(needle)
        .map(|(i, _)| (i, i + needle.len()))
        .collect()
}

/// 2. Построчное сравнение с `trim` каждой строки с обеих сторон.
fn match_line_trim(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let needle_lines: Vec<&str> = needle.lines().collect();
    window_candidates(haystack, &needle_lines, |window, expected| {
        window
            .iter()
            .zip(expected.iter())
            .all(|(w, e)| w.trim() == e.trim())
    })
}

/// 3. Якоря: первые и последние две непустые строки блока. Середина окна
///    может расходиться произвольно — в этом и смысл уровня.
fn match_block_anchor(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let needle_lines: Vec<&str> = needle.lines().collect();
    let nonempty: Vec<(usize, &str)> = needle_lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| (i, line.trim()))
        .collect();
    if nonempty.is_empty() {
        return Vec::new();
    }
    // Индексы якорей внутри `nonempty`: две первые и две последние непустые строки.
    let tail = nonempty.len().saturating_sub(BLOCK_ANCHOR_EDGE_LINES);
    let mut picks: Vec<usize> = (0..nonempty.len().min(BLOCK_ANCHOR_EDGE_LINES)).collect();
    picks.extend(tail..nonempty.len());
    picks.sort_unstable();
    picks.dedup();
    let anchors: Vec<(usize, &str)> = picks.iter().map(|&i| nonempty[i]).collect();
    window_candidates(haystack, &needle_lines, |window, _| {
        anchors.iter().all(|(offset, text)| window[*offset].trim() == *text)
    })
}

/// 4. Сравнение последовательностей «слов» после схлопывания пробельных
///    последовательностей (число строк окна при этом сохраняется).
fn match_whitespace_normalized(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let needle_tokens: Vec<&str> = needle.split_whitespace().collect();
    if needle_tokens.is_empty() {
        return Vec::new();
    }
    let needle_lines: Vec<&str> = needle.lines().collect();
    window_candidates(haystack, &needle_lines, |window, _| {
        window
            .iter()
            .flat_map(|line| line.split_whitespace())
            .eq(needle_tokens.iter().copied())
    })
}

/// 5. Сравнение после снятия общего отступа с обеих сторон.
///
/// Внутренние отступы и хвостовые пробелы значимы; строки из одних пробелов
/// приравниваются к пустым.
fn match_indent_flexible(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let needle_lines: Vec<&str> = needle.lines().collect();
    let needle_indent = common_indent(&needle_lines);
    let dedented: Vec<&str> = needle_lines
        .iter()
        .map(|&line| strip_indent(line, needle_indent))
        .collect();
    window_candidates(haystack, &needle_lines, |window, _| {
        let window_indent = common_indent(window);
        window.iter().zip(dedented.iter()).all(|(w, e)| {
            let w = strip_indent(w, window_indent);
            if w.trim().is_empty() && e.trim().is_empty() {
                true
            } else {
                w == *e
            }
        })
    })
}

/// 6. Поиск после разэкранирования `\n`/`\t`/`\r`/`\\`/`\"` в `needle`.
///
/// Если разэкранирование ничего не изменило, уровень пропускается
/// (тот же поиск уже выполнял Exact).
fn match_escape_normalized(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let unescaped = unescape(needle);
    if unescaped == needle {
        return Vec::new();
    }
    haystack
        .match_indices(unescaped.as_str())
        .map(|(i, _)| (i, i + unescaped.len()))
        .collect()
}

/// 7. Точный поиск блока, обрезанного по краям: модель часто добавляет
///    или теряет пустые строки вокруг фрагмента.
fn match_trimmed_block(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let trimmed = needle.trim();
    if trimmed.is_empty() || trimmed == needle {
        return Vec::new();
    }
    haystack
        .match_indices(trimmed)
        .map(|(i, _)| (i, i + trimmed.len()))
        .collect()
}

/// 8. Контекстное совпадение: скользящее окно размера `needle`; побеждает окно
///    с максимальной долей позиционно совпадающих (после trim) строк, но не ниже
///    порога 60%. Равные по score окна — все кандидаты (неоднозначность разрулит
///    вентиль multi-occurrence).
fn match_context(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let needle_lines: Vec<&str> = needle.lines().collect();
    let (lines, spans) = split_with_spans(haystack);
    let n = needle_lines.len();
    if n == 0 || lines.len() < n {
        return Vec::new();
    }
    let mut best_hits = 0;
    let mut best: Vec<(usize, usize)> = Vec::new();
    for i in 0..=(lines.len() - n) {
        let hits = positional_hits(&needle_lines, &lines[i..i + n]);
        if hits == 0 {
            continue;
        }
        if hits > best_hits {
            best_hits = hits;
            best.clear();
        }
        if hits == best_hits {
            best.push((spans[i].0, spans[i + n - 1].1));
        }
    }
    if best_hits * CONTEXT_MATCH_DEN >= n * CONTEXT_MATCH_NUM {
        best
    } else {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Вспомогательные функции
// ---------------------------------------------------------------------------

/// Скользящее окно по строкам `haystack` размера `needle_lines`; возвращает
/// байтовые диапазоны окон, удовлетворяющих предикату. Диапазон окна —
/// от начала первой строки до конца последней (без терминатора `\n`).
fn window_candidates(
    haystack: &str,
    needle_lines: &[&str],
    pred: impl Fn(&[&str], &[&str]) -> bool,
) -> Vec<(usize, usize)> {
    let n = needle_lines.len();
    let (lines, spans) = split_with_spans(haystack);
    if n == 0 || lines.len() < n {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..=(lines.len() - n) {
        if pred(&lines[i..i + n], needle_lines) {
            out.push((spans[i].0, spans[i + n - 1].1));
        }
    }
    out
}

/// Разбивает текст на строки (семантика `str::lines`: терминаторы `\n`/`\r\n`
/// не входят в строки, хвостовая пустая строка не создаётся) и возвращает
/// срезы строк вместе с их байтовыми диапазонами.
fn split_with_spans(s: &str) -> (Vec<&str>, Vec<(usize, usize)>) {
    let bytes = s.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            let mut end = i;
            if end > start && bytes[end - 1] == b'\r' {
                end -= 1;
            }
            spans.push((start, end));
            start = i + 1;
        }
    }
    if start < bytes.len() {
        spans.push((start, bytes.len()));
    }
    let lines = spans.iter().map(|&(a, b)| &s[a..b]).collect();
    (lines, spans)
}

/// Число позиционно совпадающих (после trim) строк окна.
fn positional_hits(needle_lines: &[&str], window: &[&str]) -> usize {
    needle_lines
        .iter()
        .zip(window.iter())
        .filter(|(a, b)| a.trim() == b.trim())
        .count()
}

/// Минимальный общий отступ в символах среди непустых строк (0, если все пусты).
fn common_indent(lines: &[&str]) -> usize {
    lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.chars().count() - line.trim_start().chars().count())
        .min()
        .unwrap_or(0)
}

/// Снимает до `n` ведущих пробельных символов со строки.
fn strip_indent(line: &str, n: usize) -> &str {
    let mut end = 0;
    for (taken, (i, c)) in line.char_indices().enumerate() {
        if taken >= n || !c.is_whitespace() {
            break;
        }
        end = i + c.len_utf8();
    }
    &line[end..]
}

/// Разэкранирует `\n`, `\t`, `\r`, `\\`, `\"` в строке от модели.
/// Неизвестные последовательности и одиночный `\` в конце сохраняются как есть.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') | None => out.push('\\'),
            Some('"') => out.push('"'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

/// Номер строки (1-based), на которой находится байтовое смещение.
fn line_number(haystack: &str, byte_offset: usize) -> usize {
    haystack.as_bytes()[..byte_offset]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

/// Отсортированные 1-based номера строк начала кандидатов (без дублей).
fn candidate_lines(haystack: &str, candidates: &[(usize, usize)]) -> Vec<usize> {
    let mut lines: Vec<usize> = candidates
        .iter()
        .map(|(start, _)| line_number(haystack, *start))
        .collect();
    lines.sort_unstable();
    lines.dedup();
    lines
}

/// 1-based номер строки начала окна с максимальной долей совпадающих строк
/// (подсказка для [`MatchError::NotFound`]); `None`, если совпадающих строк нет.
fn closest_line(haystack: &str, needle: &str) -> Option<usize> {
    let needle_lines: Vec<&str> = needle.lines().collect();
    let (lines, spans) = split_with_spans(haystack);
    let n = needle_lines.len();
    if n == 0 || lines.len() < n {
        return None;
    }
    let mut best_hits = 0;
    let mut best_byte = None;
    for i in 0..=(lines.len() - n) {
        let hits = positional_hits(&needle_lines, &lines[i..i + n]);
        if hits > best_hits {
            best_hits = hits;
            best_byte = Some(spans[i].0);
        }
    }
    best_byte.map(|pos| line_number(haystack, pos))
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- 1. Exact -----------------------------------------------------------

    #[test]
    fn exact_finds_single_occurrence() {
        let hay = "fn main() {\n    println!(\"hi\");\n}\n";
        let needle = "println!(\"hi\");";
        let m = find_match(hay, needle).unwrap();
        assert_eq!(m.kind, MatchKind::Exact);
        assert_eq!(m.as_str(hay), needle);
        assert_eq!(m.start, hay.find(needle).unwrap());
        assert_eq!(m.end, m.start + needle.len());
    }

    #[test]
    fn exact_wins_over_fuzzy_alternatives() {
        // Есть и точное совпадение, и «рваное» (лишние пробелы): exact обязан
        // победить, не дойдя до нечётких уровней и не упав в Ambiguous.
        let hay = "a b\nx\na   b\n";
        let m = find_match(hay, "a b").unwrap();
        assert_eq!(m.kind, MatchKind::Exact);
        assert_eq!(m.start, 0);
    }

    #[test]
    fn exact_multiple_occurrences_is_ambiguous_with_line_numbers() {
        let hay = "one\nx\ntwo\nx\n";
        let err = find_match(hay, "x").unwrap_err();
        assert_eq!(err, MatchError::Ambiguous { lines: vec![2, 4] });
    }

    // --- 2. LineTrim ---------------------------------------------------------

    #[test]
    fn line_trim_matches_despite_edge_whitespace() {
        // У needle собственный отступ — точный поиск не проходит,
        // а построчный trim сравнивает успешно.
        let hay = "start\na\nb\nend\n";
        let m = find_match(hay, "  a\nb").unwrap();
        assert_eq!(m.kind, MatchKind::LineTrim);
        assert_eq!(m.as_str(hay), "a\nb");
    }

    #[test]
    fn line_trim_region_covers_whole_lines() {
        // Область совпадения — целые строки исходного текста (с их отступами).
        let hay = "q\n  a\n  b\nz\n";
        let m = find_match(hay, "a\nb").unwrap();
        assert_eq!(m.kind, MatchKind::LineTrim);
        assert_eq!(m.as_str(hay), "  a\n  b");
    }

    #[test]
    fn line_trim_ambiguous_when_several_windows_match() {
        let hay = "x\ny\nx\n";
        let err = find_match(hay, "  x  ").unwrap_err();
        assert_eq!(err, MatchError::Ambiguous { lines: vec![1, 3] });
    }

    // --- 3. BlockAnchor ------------------------------------------------------

    #[test]
    fn block_anchor_tolerates_changed_middle_lines() {
        let needle = "a1\na2\nm1\nm2\na5\na6";
        let hay = "head\na1\na2\nX1\nX2\na5\na6\ntail\n";
        let m = find_match(hay, needle).unwrap();
        assert_eq!(m.kind, MatchKind::BlockAnchor);
        assert_eq!(m.as_str(hay), "a1\na2\nX1\nX2\na5\na6");
    }

    #[test]
    fn block_anchor_fails_when_edge_anchor_breaks() {
        // Якорь на краю блока не совпал — окно отклоняется,
        // даже если остальные якоря на месте.
        let needle = "a1\na2\nm1\nm2\na5\na6";
        let hay = "a1\na2\nm1\nm2\na5\nWRONG\n";
        assert!(match_block_anchor(hay, needle).is_empty());
    }

    // --- 4. WhitespaceNormalized ---------------------------------------------

    #[test]
    fn whitespace_normalized_collapses_whitespace_runs() {
        let hay = "fn f() {\n    let   a\t=\t 1;\n}\n";
        let m = find_match(hay, "let a = 1;").unwrap();
        assert_eq!(m.kind, MatchKind::WhitespaceNormalized);
        assert_eq!(m.as_str(hay), "    let   a\t=\t 1;");
    }

    // --- 5. IndentFlexible ---------------------------------------------------
    // Уровень идёт после LineTrim, который поглощает чистые различия отступов,
    // поэтому в живом каскаде он почти недостижим — проверяем матчер напрямую.

    #[test]
    fn indent_flexible_ignores_common_reindent() {
        let hay = "if ok {\n        foo();\n          bar();\n}\n";
        let needle = "foo();\n  bar();";
        let cands = match_indent_flexible(hay, needle);
        assert_eq!(cands.len(), 1);
        let (start, end) = cands[0];
        assert_eq!(&hay[start..end], "        foo();\n          bar();");
    }

    #[test]
    fn indent_flexible_keeps_significant_trailing_whitespace() {
        // Хвостовые пробелы значимы: окно с «грязной» строкой не совпадает.
        let hay = "foo();  \nbar();\n";
        assert!(match_indent_flexible(hay, "foo();\nbar();").is_empty());
    }

    // --- 6. EscapeNormalized --------------------------------------------------

    #[test]
    fn escape_normalized_unescapes_literal_sequences() {
        // Модель прислала \n двумя символами вместо реального перевода строки.
        let hay = "line1\nline2\n";
        let m = find_match(hay, "line1\\nline2").unwrap();
        assert_eq!(m.kind, MatchKind::EscapeNormalized);
        assert_eq!(m.as_str(hay), "line1\nline2");
    }

    #[test]
    fn unescape_handles_backslash_edge_cases() {
        assert_eq!(unescape("a\\\\nb"), "a\\nb"); // \\ → \, «n» остаётся буквой
        assert_eq!(unescape("a\\qb"), "a\\qb"); // неизвестная последовательность — как есть
        assert_eq!(unescape("x\\t"), "x\t");
        assert_eq!(unescape("tail\\"), "tail\\"); // одиночный \ в конце строки
        assert_eq!(unescape("plain"), "plain");
    }

    // --- 7. TrimmedBlock -------------------------------------------------------

    #[test]
    fn trimmed_block_matches_with_surrounding_blank_lines() {
        let hay = "x\nfoo bar\ny\n";
        let m = find_match(hay, "\n\nfoo bar\n").unwrap();
        assert_eq!(m.kind, MatchKind::TrimmedBlock);
        assert_eq!(m.as_str(hay), "foo bar");
    }

    // --- 8. ContextMatch --------------------------------------------------------

    #[test]
    fn context_match_selects_highest_scoring_window() {
        let needle = "alpha\nbeta\ngamma\ndelta\nomega";
        let hay = "zz\nalpha\nbeta\ngamma\ndelta\nCHANGED\nalpha\nbeta\nXX\nYY\nZZ\n";
        let m = find_match(hay, needle).unwrap();
        assert_eq!(m.kind, MatchKind::ContextMatch);
        assert_eq!(m.as_str(hay), "alpha\nbeta\ngamma\ndelta\nCHANGED");
    }

    #[test]
    fn context_match_accepts_exactly_sixty_percent() {
        // Граница порога: 3/5 = 60% — допустимо.
        let needle = "a\nb\nc\nd\ne";
        let hay = "a\nb\nc\nx\ny\n";
        let m = find_match(hay, needle).unwrap();
        assert_eq!(m.kind, MatchKind::ContextMatch);
        assert_eq!(m.as_str(hay), "a\nb\nc\nx\ny");
    }

    #[test]
    fn context_match_below_threshold_falls_through_to_not_found() {
        // 2/5 = 40% — ниже порога, итог — NotFound с подсказкой ближайшего окна.
        let needle = "a\nb\nc\nd\ne";
        let hay = "a\nb\nx\ny\nz\n";
        let err = find_match(hay, needle).unwrap_err();
        assert_eq!(err, MatchError::NotFound { closest: Some(1) });
    }

    #[test]
    fn context_match_tie_reports_ambiguous() {
        let needle = "a\nb\nc\nd\ne";
        let hay = "a\nb\nc\nd\nX\ng1\ng2\na\nb\nc\nd\nY\n";
        let err = find_match(hay, needle).unwrap_err();
        assert_eq!(err, MatchError::Ambiguous { lines: vec![1, 8] });
    }

    // --- 9. Multi-occurrence и общие граничные случаи -----------------------------

    #[test]
    fn empty_needle_is_rejected_as_not_found() {
        assert_eq!(find_match("abc", ""), Err(MatchError::NotFound { closest: None }));
        assert_eq!(find_match("", ""), Err(MatchError::NotFound { closest: None }));
        assert_eq!(find_match("", "x"), Err(MatchError::NotFound { closest: None }));
    }

    #[test]
    fn not_found_without_overlap_has_no_closest_hint() {
        let err = find_match("foo\nbar\n", "zzz").unwrap_err();
        assert_eq!(err, MatchError::NotFound { closest: None });
    }

    #[test]
    fn ambiguous_occurrences_on_same_line_are_deduplicated() {
        let err = find_match("aa aa", "aa").unwrap_err();
        assert_eq!(err, MatchError::Ambiguous { lines: vec![1] });
    }

    #[test]
    fn fuzzy_regions_respect_utf8_boundaries() {
        // Диапазон совпадения обязан лежать на границах символов UTF-8.
        let hay = "привет\nкак дела\nмир\n";
        let m = find_match(hay, " как дела ").unwrap();
        assert_eq!(m.kind, MatchKind::LineTrim);
        assert_eq!(m.as_str(hay), "как дела");
        assert_eq!(m.start, hay.find("как дела").unwrap());
    }

    #[test]
    fn crlf_lines_match_without_carriage_return() {
        let hay = "one\r\ntwo\r\nthree\r\n";
        let m = find_match(hay, "  two").unwrap();
        assert_eq!(m.kind, MatchKind::LineTrim);
        assert_eq!(m.as_str(hay), "two");
    }

    #[test]
    fn match_kind_names_are_stable_for_logs() {
        assert_eq!(MatchKind::Exact.as_str(), "exact");
        assert_eq!(MatchKind::ContextMatch.as_str(), "context_match");
        assert_eq!(MatchKind::BlockAnchor.to_string(), "якоря блока");
    }

    #[test]
    fn match_error_display_mentions_lines_and_hint() {
        let msg = MatchError::Ambiguous { lines: vec![3, 7] }.to_string();
        assert!(msg.contains('3') && msg.contains('7'));
        let hint = MatchError::NotFound { closest: Some(12) }.to_string();
        assert!(hint.contains("12"));
        let silent = MatchError::NotFound { closest: None }.to_string();
        assert!(!silent.contains("строка"));
    }
}
