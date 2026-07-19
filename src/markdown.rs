//! Модуль `markdown` — рендер подмножества Markdown в строки для терминала:
//! ANSI-цвета для TUI (ответы ассистента) и «голый» текст для headless-режима.
//!
//! Поддерживаемое подмножество (вложенность стилей не поддерживается):
//! - заголовки `#`..`####` — жирный акцентный цвет;
//! - `**жирный**`, `*курсив*` (приглушённый), `` `инлайн-код` `` (циан, атомарно,
//!   пробелы внутри не переносятся);
//! - код-фенсы ` ```lang ` — циановый фон на всю ширину, контент выводится
//!   дословно (без инлайн-разметки), отступы сохраняются;
//! - списки `-`/`*` (маркер заменяется на `•`), нумерованные — маркер сохраняется;
//! - ссылки `[текст](url)` — текст + приглушённый URL в скобках;
//! - горизонтальные линии `---`/`***`/`___` — линия `─` на всю ширину;
//! - цитаты `>` — приглушённый префикс `│`;
//! - экранирование `\*`, `\\`, `` \` `` и прочей пунктуации — литеральный символ.
//!
//! Абзацы, пункты списков и цитаты переносятся по словам под заданную ширину;
//! слова длиннее строки рубятся посимвольно. ANSI-коды при подсчёте видимой
//! ширины не учитываются. Вывод построчный: непустой результат всегда
//! оканчивается переводом строки.

/// Режим вывода: с ANSI-escape (TUI) или без (headless, логи, пайпы).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderMode {
    /// ANSI-цвета и атрибуты для терминала.
    #[default]
    Ansi,
    /// Без escape-последовательностей; разметка сохраняется текстом
    /// (маркеры `•`, префиксы `│`, линии `─`), но цветов нет.
    Strip,
}

/// Ширина по умолчанию, если размер терминала неизвестен.
pub const DEFAULT_WIDTH: usize = 80;

/// Сброс всех атрибутов.
const ANSI_RESET: &str = "\u{1b}[0m";
/// Жирное начертание.
const ANSI_BOLD: &str = "\u{1b}[1m";
/// Приглушённый текст (курсив, URL, префиксы цитат, линии).
const ANSI_DIM: &str = "\u{1b}[2m";
/// Циан — инлайн-код.
const ANSI_CYAN: &str = "\u{1b}[36m";
/// Акцентный цвет заголовков (яркий маджента).
const ANSI_ACCENT: &str = "\u{1b}[95m";
/// Фон код-фенсов: чёрный текст на циановом фоне.
const ANSI_BG_CYAN: &str = "\u{1b}[30;46m";

/// Видимый стиль текстового сегмента (без привязки к конкретным ANSI-кодам).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    /// Обычный текст.
    Normal,
    /// Жирный (`**..**`).
    Bold,
    /// Приглушённый (курсив `*..*`, URL ссылок, префиксы цитат, линии).
    Dim,
    /// Инлайн-код (`` `..` ``) — циан.
    Code,
    /// Заголовок — жирный акцентный цвет.
    Heading,
}

/// Текстовый сегмент с единым стилем — атом рендера.
#[derive(Debug, Clone)]
struct Seg {
    /// Стиль сегмента.
    style: Style,
    /// Видимый текст сегмента.
    text: String,
}

impl Seg {
    /// Создаёт сегмент из чего угодно, конвертируемого в `String`.
    fn new(style: Style, text: impl Into<String>) -> Self {
        Self {
            style,
            text: text.into(),
        }
    }
}

/// Рендерит Markdown в ANSI-строку для TUI. См. документацию модуля.
pub fn render(text: &str, width: usize) -> String {
    MarkdownRenderer::new(width).render(text)
}

/// Рендерит Markdown в «голый» текст без ANSI (headless, логи, пайпы).
pub fn render_strip(text: &str, width: usize) -> String {
    MarkdownRenderer::stripped(width).render(text)
}

/// Рендерер Markdown с фиксированной шириной строки и режимом вывода.
///
/// Ширина зажимается снизу единицей, чтобы перенос не зацикливался.
#[derive(Debug, Clone, Copy)]
pub struct MarkdownRenderer {
    /// Целевая видимая ширина строки (минимум 1).
    width: usize,
    /// Режим вывода: ANSI или strip.
    mode: RenderMode,
}

impl MarkdownRenderer {
    /// Рендерер для TUI: ANSI-цвета включены.
    pub fn new(width: usize) -> Self {
        Self {
            width: width.max(1),
            mode: RenderMode::Ansi,
        }
    }

    /// Рендерер для headless-режима: без escape-последовательностей.
    pub fn stripped(width: usize) -> Self {
        Self {
            width: width.max(1),
            mode: RenderMode::Strip,
        }
    }

    /// Текущая целевая ширина.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Текущий режим вывода.
    pub fn mode(&self) -> RenderMode {
        self.mode
    }

    /// Рендерит `text` построчно: блоки (заголовки, фенсы, списки, цитаты,
    /// линии, абзацы) распознаются по началу строки, инлайн-разметка —
    /// внутри блока. Незакрытый фенс в конце входа — не ошибка.
    pub fn render(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len() + 64);
        let mut in_fence = false;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("```") {
                if in_fence {
                    // Закрывающий фенс: пустая полоса — «нижняя кромка» блока.
                    self.write_fence_band(&mut out, "");
                    in_fence = false;
                } else {
                    self.write_fence_band(&mut out, rest.trim());
                    in_fence = true;
                }
                continue;
            }
            if in_fence {
                self.write_code_line(&mut out, line);
            } else {
                self.write_block(&mut out, line);
            }
        }
        out
    }

    /// Полоса-разделитель код-фенса на всю ширину; у открывающей — метка языка.
    /// В strip-режиме строки-ограничители фенса не выводятся вовсе.
    fn write_fence_band(&self, out: &mut String, lang: &str) {
        if self.mode == RenderMode::Strip {
            return;
        }
        let label = if lang.is_empty() {
            String::new()
        } else {
            format!(" {lang} ")
        };
        let pad = self.width.saturating_sub(label.chars().count());
        out.push_str(ANSI_BG_CYAN);
        out.push_str(&label);
        out.push_str(&" ".repeat(pad));
        out.push_str(ANSI_RESET);
        out.push('\n');
    }

    /// Строка внутри код-фенса: дословно, с отступами, на циановом фоне
    /// (в ANSI-режиме фон добивается пробелами до полной ширины).
    fn write_code_line(&self, out: &mut String, line: &str) {
        if self.mode == RenderMode::Strip {
            out.push_str(line);
            out.push('\n');
            return;
        }
        let pad = self.width.saturating_sub(line.chars().count());
        out.push_str(ANSI_BG_CYAN);
        out.push_str(line);
        out.push_str(&" ".repeat(pad));
        out.push_str(ANSI_RESET);
        out.push('\n');
    }

    /// Один блочный элемент вне код-фенса.
    fn write_block(&self, out: &mut String, line: &str) {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            out.push('\n');
            return;
        }
        let t = trimmed.trim_start();

        // Заголовок: инлайн-разметка работает, но весь текст — акцентный.
        if let Some(level) = heading_level(t) {
            let content = t[level..].trim_start();
            let segs = inline_parse(content)
                .into_iter()
                .map(|s| Seg {
                    style: heading_style(s.style),
                    text: s.text,
                })
                .collect::<Vec<_>>();
            let empty = Seg::new(Style::Normal, "");
            self.push_wrapped(out, &segs, &empty, &empty);
            return;
        }

        // Горизонтальная линия на всю ширину.
        if is_rule(t) {
            let bar = "─".repeat(self.width);
            self.emit(out, Style::Dim, &bar);
            out.push('\n');
            return;
        }

        // Цитата: приглушённая вертикальная черта, контент с переносом.
        if let Some(rest) = t.strip_prefix('>') {
            let content = rest.strip_prefix(' ').unwrap_or(rest);
            let segs = inline_parse(content);
            let prefix = Seg::new(Style::Dim, "│ ");
            self.push_wrapped(out, &segs, &prefix, &prefix);
            return;
        }

        // Маркированный список: маркер подменяется на `•`.
        if let Some(content) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            let segs = inline_parse(content);
            let first = Seg::new(Style::Normal, "• ");
            let cont = Seg::new(Style::Normal, "  ");
            self.push_wrapped(out, &segs, &first, &cont);
            return;
        }

        // Нумерованный список: маркер сохраняется, продолжение — с отступом.
        if let Some(mlen) = numbered_marker_len(t) {
            let segs = inline_parse(&t[mlen..]);
            let first = Seg::new(Style::Normal, &t[..mlen]);
            let cont = Seg::new(Style::Normal, " ".repeat(mlen));
            self.push_wrapped(out, &segs, &first, &cont);
            return;
        }

        // Обычный абзац.
        let segs = inline_parse(t);
        let empty = Seg::new(Style::Normal, "");
        self.push_wrapped(out, &segs, &empty, &empty);
    }

    /// Переносит сегменты по ширине и дописывает строки в вывод.
    fn push_wrapped(&self, out: &mut String, segs: &[Seg], first: &Seg, cont: &Seg) {
        for line in self.wrap(segs, first, cont) {
            out.push_str(&line);
            out.push('\n');
        }
    }

    /// Жадный перенос по словам. `first` — префикс первой строки,
    /// `cont` — префикс строк-продолжений (висячий отступ).
    ///
    /// Эффективная ширина зажимается снизу так, чтобы префикс продолжения
    /// всегда помещался с хотя бы одним символом — это исключает зацикливание
    /// при узкой ширине и длинном маркере (например, `100. `).
    fn wrap(&self, segs: &[Seg], first: &Seg, cont: &Seg) -> Vec<String> {
        let width = self
            .width
            .max(cont.text.chars().count() + 1)
            .max(first.text.chars().count() + 1);
        let mut lines = Vec::new();
        let mut cur = String::new();
        self.emit(&mut cur, first.style, &first.text);
        let mut col = first.text.chars().count();
        let mut has_words = false;

        for seg in segs {
            for word in words_of(seg) {
                let wlen = word.chars().count();
                // Не влезло в текущую строку — перенос на строку-продолжение.
                if has_words && col + 1 + wlen > width {
                    col = self.start_continuation(&mut lines, &mut cur, cont);
                    has_words = false;
                }
                // Слово длиннее всей доступной ширины — рубим посимвольно.
                if !has_words && wlen > width - col {
                    let mut rest = word;
                    while !rest.is_empty() {
                        let avail = width - col;
                        let cut = rest
                            .char_indices()
                            .nth(avail)
                            .map_or(rest.len(), |(i, _)| i);
                        self.emit(&mut cur, seg.style, &rest[..cut]);
                        col += rest[..cut].chars().count();
                        rest = &rest[cut..];
                        if !rest.is_empty() {
                            col = self.start_continuation(&mut lines, &mut cur, cont);
                        }
                    }
                    has_words = true;
                    continue;
                }
                if has_words {
                    self.emit(&mut cur, Style::Normal, " ");
                    col += 1;
                }
                self.emit(&mut cur, seg.style, word);
                col += wlen;
                has_words = true;
            }
        }
        lines.push(cur);
        lines
    }

    /// Завершает текущую строку и начинает строку-продолжение с префикса
    /// `cont`; возвращает новую видимую колонку.
    fn start_continuation(&self, lines: &mut Vec<String>, cur: &mut String, cont: &Seg) -> usize {
        lines.push(std::mem::take(cur));
        self.emit(cur, cont.style, &cont.text);
        cont.text.chars().count()
    }

    /// Дописывает `text` стилем `style`; в strip-режиме — без escape-кодов.
    fn emit(&self, out: &mut String, style: Style, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.mode == RenderMode::Strip {
            out.push_str(text);
            return;
        }
        match style {
            Style::Normal => out.push_str(text),
            Style::Bold => {
                out.push_str(ANSI_BOLD);
                out.push_str(text);
                out.push_str(ANSI_RESET);
            }
            Style::Dim => {
                out.push_str(ANSI_DIM);
                out.push_str(text);
                out.push_str(ANSI_RESET);
            }
            Style::Code => {
                out.push_str(ANSI_CYAN);
                out.push_str(text);
                out.push_str(ANSI_RESET);
            }
            Style::Heading => {
                out.push_str(ANSI_BOLD);
                out.push_str(ANSI_ACCENT);
                out.push_str(text);
                out.push_str(ANSI_RESET);
            }
        }
    }
}

/// Слова сегмента для переноса: инлайн-код атомарен (пробелы внутри
/// не разбиваются), остальное режется по пробельным символам.
fn words_of(seg: &Seg) -> Vec<&str> {
    if seg.style == Style::Code {
        vec![seg.text.as_str()]
    } else {
        seg.text.split_whitespace().collect()
    }
}

/// Стиль сегмента внутри заголовка: всё акцентное, кроме инлайн-кода.
fn heading_style(style: Style) -> Style {
    match style {
        Style::Code => Style::Code,
        _ => Style::Heading,
    }
}

/// Уровень заголовка ATX (1..=4), если строка — `# `..`#### `.
fn heading_level(s: &str) -> Option<usize> {
    let hashes = s.bytes().take_while(|b| *b == b'#').count();
    if (1..=4).contains(&hashes) && s.as_bytes().get(hashes) == Some(&b' ') {
        Some(hashes)
    } else {
        None
    }
}

/// Горизонтальная линия: не менее трёх одинаковых символов `-`, `*` или `_`.
fn is_rule(s: &str) -> bool {
    let t = s.trim();
    let Some(first) = t.chars().next() else {
        return false;
    };
    t.len() >= 3 && matches!(first, '-' | '*' | '_') && t.chars().all(|c| c == first)
}

/// Длина маркера нумерованного списка (`1. `, `12. `) в байтах,
/// включая точку и пробел; `None`, если строка не начинается с маркера.
fn numbered_marker_len(s: &str) -> Option<usize> {
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    let bytes = s.as_bytes();
    if digits > 0 && bytes.get(digits) == Some(&b'.') && bytes.get(digits + 1) == Some(&b' ') {
        Some(digits + 2)
    } else {
        None
    }
}

/// Позиция первого вхождения `needle` в `chars`, начиная с индекса `start`.
fn find_from(chars: &[char], start: usize, needle: char) -> Option<usize> {
    chars[start..]
        .iter()
        .position(|c| *c == needle)
        .map(|p| p + start)
}

/// Позиция первой пары `**`, начиная с индекса `start`.
fn find_pair(chars: &[char], start: usize) -> Option<usize> {
    (start..chars.len().saturating_sub(1)).find(|j| chars[*j] == '*' && chars[*j + 1] == '*')
}

/// Инлайн-код `` `...` `` с позиции `start` (где стоит открывающий бэктик).
/// Возвращает сегмент и индекс следующего после закрывающего символа.
fn code_span(chars: &[char], start: usize) -> Option<(Seg, usize)> {
    let end = find_from(chars, start + 1, '`')?;
    if end == start + 1 {
        return None;
    }
    let text: String = chars[start + 1..end].iter().collect();
    Some((Seg::new(Style::Code, text), end + 1))
}

/// Жирный `**...**` с позиции `start`. Незакрытый, пустой или закрытый
/// после пробела (нарушение фланкирования) — не матчится.
fn bold_span(chars: &[char], start: usize) -> Option<(Seg, usize)> {
    if chars.get(start + 1) != Some(&'*') {
        return None;
    }
    if chars.get(start + 2).is_none_or(|c| c.is_whitespace()) {
        return None;
    }
    let mut from = start + 2;
    loop {
        let end = find_pair(chars, from)?;
        if end != start + 2 && !chars[end - 1].is_whitespace() {
            let text: String = chars[start + 2..end].iter().collect();
            return Some((Seg::new(Style::Bold, text), end + 2));
        }
        from = end + 2;
    }
}

/// Курсив `*...*` с позиции `start` — приглушённый стиль. Закрывающая
/// звезда не должна идти после пробела (фланкирование).
fn italic_span(chars: &[char], start: usize) -> Option<(Seg, usize)> {
    if chars
        .get(start + 1)
        .is_none_or(|c| c.is_whitespace() || *c == '*')
    {
        return None;
    }
    let mut from = start + 1;
    loop {
        let end = find_from(chars, from, '*')?;
        if !chars[end - 1].is_whitespace() {
            let text: String = chars[start + 1..end].iter().collect();
            return Some((Seg::new(Style::Dim, text), end + 1));
        }
        from = end + 1;
    }
}

/// Ссылка `[текст](url)` с позиции `start`. Возвращает текст, URL и индекс
/// следующего символа после `)`. Пустой текст, пустой URL или URL
/// с пробельными символами — не матчится.
fn parse_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let close = find_from(chars, start + 1, ']')?;
    if close == start + 1 {
        return None;
    }
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = find_from(chars, close + 2, ')')?;
    let url: String = chars[close + 2..end].iter().collect();
    if url.is_empty() || url.chars().any(char::is_whitespace) {
        return None;
    }
    let text: String = chars[start + 1..close].iter().collect();
    Some((text, url, end + 1))
}

/// Сбрасывает накопленный литеральный буфер в список сегментов.
fn flush_buf(segs: &mut Vec<Seg>, buf: &mut String) {
    if !buf.is_empty() {
        segs.push(Seg::new(Style::Normal, std::mem::take(buf)));
    }
}

/// Разбор инлайн-разметки одной строки в последовательность сегментов.
///
/// Без вложенности: первый корректно закрытый спан побеждает. Обратный слеш
/// перед ASCII-пунктуацией экранирует её (например, `\*` — литеральная звезда).
/// Незакрытые маркеры остаются литеральным текстом.
fn inline_parse(text: &str) -> Vec<Seg> {
    let chars: Vec<char> = text.chars().collect();
    let mut segs: Vec<Seg> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() && chars[i + 1].is_ascii_punctuation() {
            buf.push(chars[i + 1]);
            i += 2;
            continue;
        }
        let parsed: Option<(Vec<Seg>, usize)> = match c {
            '`' => code_span(&chars, i).map(|(s, n)| (vec![s], n)),
            '*' => bold_span(&chars, i)
                .or_else(|| italic_span(&chars, i))
                .map(|(s, n)| (vec![s], n)),
            '[' => parse_link(&chars, i).map(|(t, u, n)| {
                (
                    vec![
                        Seg::new(Style::Normal, t),
                        Seg::new(Style::Dim, format!(" ({u})")),
                    ],
                    n,
                )
            }),
            _ => None,
        };
        if let Some((new_segs, next)) = parsed {
            flush_buf(&mut segs, &mut buf);
            segs.extend(new_segs);
            i = next;
        } else {
            buf.push(c);
            i += 1;
        }
    }
    flush_buf(&mut segs, &mut buf);
    segs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Убирает CSI-последовательности (ESC `[` ... `m`), чтобы в тестах
    /// мерить видимый текст, а не escape-коды.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for esc in chars.by_ref() {
                    if esc == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Видимая ширина строки после удаления ANSI-кодов.
    fn visible_width(s: &str) -> usize {
        strip_ansi(s).chars().count()
    }

    #[test]
    fn heading_is_bold_accent() {
        let out = render("# Шапка", 80);
        assert!(out.contains(ANSI_BOLD));
        assert!(out.contains(ANSI_ACCENT));
        assert!(out.contains("Шапка"));
        assert!(!strip_ansi(&out).contains('#'));
        // #### — ещё заголовок, ##### — уже абзац.
        assert!(render("#### Четыре", 80).contains(ANSI_ACCENT));
        assert!(strip_ansi(&render("##### Пять", 80)).contains("##### Пять"));
    }

    #[test]
    fn bold_and_italic_spans() {
        let out = render("**жирный** и *тихий*", 80);
        assert!(out.contains(&format!("{ANSI_BOLD}жирный{ANSI_RESET}")));
        assert!(out.contains(&format!("{ANSI_DIM}тихий{ANSI_RESET}")));
    }

    #[test]
    fn inline_code_is_cyan() {
        let out = render("команда `cargo test` готова", 80);
        assert!(out.contains(&format!("{ANSI_CYAN}cargo test{ANSI_RESET}")));
    }

    #[test]
    fn unclosed_markers_stay_literal() {
        let out = render("**не закрыто и *тоже", 80);
        let plain = strip_ansi(&out);
        assert!(plain.contains("**не закрыто и *тоже"));
        // Пустые спаны — тоже литералы.
        assert!(strip_ansi(&render("`` и ** **", 80)).contains("``"));
    }

    #[test]
    fn fence_preserves_content_verbatim() {
        let src = "```rust\n    let x = **не жирный**;\n    // `кавычки`\n```";
        let out = render(src, 60);
        assert!(out.contains(ANSI_BG_CYAN));
        assert!(out.contains("    let x = **не жирный**;"));
        assert!(out.contains("rust"));
        // Внутри фенса инлайн-разметка не срабатывает.
        assert!(!out.contains(ANSI_BOLD));
        assert!(!out.contains(ANSI_CYAN));
    }

    #[test]
    fn fence_in_strip_mode_is_raw() {
        let src = "```\nfn main() {}\n```";
        assert_eq!(render_strip(src, 80), "fn main() {}\n");
    }

    #[test]
    fn bullet_marker_becomes_dot() {
        let out = render("- раз\n* два", 80);
        let plain = strip_ansi(&out);
        let mut lines = plain.lines();
        assert_eq!(lines.next(), Some("• раз"));
        assert_eq!(lines.next(), Some("• два"));
    }

    #[test]
    fn numbered_markers_are_kept() {
        let out = render("1. раз\n12. два", 80);
        let plain = strip_ansi(&out);
        assert!(plain.contains("1. раз"));
        assert!(plain.contains("12. два"));
    }

    #[test]
    fn link_text_plus_dim_url() {
        let out = render("[доки](https://example.com/docs)", 80);
        assert!(out.contains("доки"));
        assert!(out.contains(&format!("{ANSI_DIM}(https://example.com/docs){ANSI_RESET}")));
        let plain = strip_ansi(&out);
        assert_eq!(plain.trim_end(), "доки (https://example.com/docs)");
    }

    #[test]
    fn broken_link_stays_literal() {
        let out = render("[нет скобки]( и [пустой]()", 80);
        let plain = strip_ansi(&out);
        assert!(plain.contains("[нет скобки]("));
        assert!(plain.contains("[пустой]()"));
    }

    #[test]
    fn escaped_stars_are_literal() {
        let out = render("\\*не курсив\\* и \\\\ слеш", 80);
        let plain = strip_ansi(&out);
        assert!(plain.contains("*не курсив*"));
        assert!(plain.contains("\\ слеш"));
        assert!(!out.contains(ANSI_DIM));
    }

    #[test]
    fn rule_spans_full_width() {
        let out = render("---", 25);
        let plain = strip_ansi(&out);
        assert_eq!(plain.trim_end_matches('\n'), "─".repeat(25));
        assert!(out.contains(ANSI_DIM));
        // ___ и *** тоже линии.
        assert_eq!(strip_ansi(&render("___", 10)).trim_end(), "─".repeat(10));
    }

    #[test]
    fn quote_gets_dim_bar_prefix() {
        let out = render("> важная мысль", 80);
        let plain = strip_ansi(&out);
        assert_eq!(plain.trim_end(), "│ важная мысль");
        assert!(out.contains(ANSI_DIM));
    }

    #[test]
    fn wrap_respects_width_and_keeps_words() {
        let text = "Раз два три четыре пять шесть семь восемь девять десять слов много";
        let out = render(text, 20);
        for line in out.lines() {
            assert!(visible_width(line) <= 20, "строка слишком длинная: {line:?}");
        }
        let plain = strip_ansi(&out);
        let got: Vec<&str> = plain.split_whitespace().collect();
        let want: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(got, want);
    }

    #[test]
    fn list_wrap_uses_hanging_indent() {
        let out = render("- слово1 слово2 слово3 слово4", 14);
        let plain = strip_ansi(&out);
        let lines: Vec<&str> = plain.lines().collect();
        assert!(lines.len() > 1);
        assert!(lines[0].starts_with("• "));
        assert!(lines[1].starts_with("  "));
        for line in lines {
            assert!(visible_width(line) <= 14, "строка слишком длинная: {line:?}");
        }
    }

    #[test]
    fn long_word_is_hard_split() {
        let word = "abcdefghijklmnopqrstuvwxyzабвг";
        let out = render(word, 10);
        let plain = strip_ansi(&out);
        for line in plain.lines() {
            assert!(line.chars().count() <= 10);
        }
        // Склейка строк возвращает исходное слово.
        assert_eq!(plain.lines().collect::<String>(), word);
    }

    #[test]
    fn strip_mode_has_no_ansi() {
        let src = "# Заголовок\n\nТекст **жирный** и *тихий*, `код`, [ссылка](https://a.b).\n\n- пункт\n> цитата\n---\n```rust\ncode();\n```";
        let out = render_strip(src, 40);
        assert!(!out.contains('\u{1b}'));
        assert!(out.contains("• пункт"));
        assert!(out.contains("│ цитата"));
        assert!(out.contains("code();"));
    }

    #[test]
    fn blank_lines_pass_through() {
        assert_eq!(render_strip("а\n\nб", 80), "а\n\nб\n");
    }

    #[test]
    fn empty_input_gives_empty_output() {
        assert_eq!(render("", 80), "");
        assert_eq!(render_strip("", 80), "");
    }

    #[test]
    fn heading_keeps_inline_code() {
        let out = render("## Про `cargo`", 80);
        assert!(out.contains(ANSI_ACCENT));
        assert!(out.contains(&format!("{ANSI_CYAN}cargo{ANSI_RESET}")));
    }

    #[test]
    fn renderer_accessors_and_width_clamp() {
        let r = MarkdownRenderer::stripped(100);
        assert_eq!(r.width(), 100);
        assert_eq!(r.mode(), RenderMode::Strip);
        // Ширина 0 зажимается до 1, рендер не зацикливается.
        let zero = MarkdownRenderer::new(0);
        assert_eq!(zero.width(), 1);
        // При ширине 1 длинное слово рубится посимвольно, без зацикливания.
        assert_eq!(zero.render("абв"), "а\nб\nв\n");
    }

    #[test]
    fn default_width_constant_is_sane() {
        assert_eq!(DEFAULT_WIDTH, 80);
    }
}
