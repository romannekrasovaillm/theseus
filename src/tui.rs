//! TUI на ratatui: лог, статус-бар, ввод, попап разрешений, выделение мышью.
//! Дизайн-токены оформления — семантические роли [`crate::theme`], а не
//! разбросанные по коду цветовые константы; тема переключается командой /theme.

use crate::agent::{Agent, AgentEvent, Controls};
use crate::gitutil::GitRepo;
use crate::history::{InputHistory, DEFAULT_CAPACITY};
use crate::slash::{self, Parsed};
use crate::theme::{Color16, ColorSpec, Theme, ThemeRole};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    EnableBracketedPaste, DisableBracketedPaste, EnableMouseCapture, DisableMouseCapture};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;
use std::borrow::Cow;
use std::collections::HashSet;
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct PermBroker {
    pending: Mutex<Option<(String, Sender<bool>)>>,
}

impl PermBroker {
    pub fn new() -> Arc<Self> {
        Arc::new(PermBroker { pending: Mutex::new(None) })
    }
    /// вызывается агентом (блокирует его поток до ответа)
    pub fn ask(&self, question: &str) -> bool {
        let (tx, rx) = channel();
        *self.pending.lock().unwrap() = Some((question.to_string(), tx));
        rx.recv().unwrap_or(false)
    }
    fn take(&self) -> Option<(String, Sender<bool>)> {
        self.pending.lock().unwrap().take()
    }
    fn peek(&self) -> Option<String> {
        self.pending.lock().unwrap().as_ref().map(|(q, _)| q.clone())
    }
}

struct LogLine {
    spans: Vec<Span<'static>>,
}

/// Накапливаемый стиль при разборе SGR-последовательностей markdown-рендера.
#[derive(Default, Clone, Copy)]
struct MdStyle {
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
    dim: bool,
}

impl MdStyle {
    fn to_style(self) -> Style {
        let mut s = Style::default();
        if let Some(fg) = self.fg { s = s.fg(fg); }
        if let Some(bg) = self.bg { s = s.bg(bg); }
        if self.bold { s = s.add_modifier(Modifier::BOLD); }
        if self.dim { s = s.add_modifier(Modifier::DIM); }
        s
    }

    /// Применить SGR-код из палитры crate::markdown (0, 1, 2, 36, 95, 30;46).
    fn apply_sgr(&mut self, code: &str) {
        match code {
            "0" => *self = MdStyle::default(),
            "1" => self.bold = true,
            "2" => self.dim = true,
            "36" => self.fg = Some(Color::Cyan),
            "95" => self.fg = Some(Color::LightMagenta),
            // код-фенс: мягкий светло-серый текст на нейтральном тёмно-сером
            // поле 256-палитры (видно на тёмном фоне, не режет глаза)
            "38;5;248;48;5;238" => {
                self.fg = Some(Color::Indexed(248));
                self.bg = Some(Color::Indexed(238));
            }
            _ => {} // чужие коды игнорируем — текст не теряется
        }
    }
}

/// Преобразовать ANSI-строки crate::markdown::render в стилизованные строки
/// ratatui. Неполная ESC-последовательность в конце строки отбрасывается.
fn md_ansi_to_lines(rendered: &str) -> Vec<Vec<Span<'static>>> {
    let mut lines = Vec::new();
    for raw_line in rendered.lines() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut style = MdStyle::default();
        let mut rest = raw_line;
        while let Some(pos) = rest.find('\u{1b}') {
            if pos > 0 {
                spans.push(Span::styled(rest[..pos].to_string(), style.to_style()));
            }
            let tail = &rest[pos..];
            match tail.find('m') {
                Some(end) => {
                    style.apply_sgr(&tail[2..end]);
                    rest = &tail[end + 1..];
                }
                None => { rest = ""; break; }
            }
        }
        if !rest.is_empty() {
            spans.push(Span::styled(rest.to_string(), style.to_style()));
        }
        if spans.is_empty() {
            spans.push(Span::raw(""));
        }
        lines.push(spans);
    }
    lines
}

// ---------------------------------------------------------------------------
// Дизайн-токены TUI: роли crate::theme → ratatui Color, встроенные палитры
// ---------------------------------------------------------------------------

/// Цветовая спецификация темы → цвет ratatui.
///
/// [`ColorSpec::Default`] (mono-тема) → [`Color::Reset`]: «без цвета»,
/// дифференциация текста остаётся на атрибутах Bold/Dim (см. [`role_style`]).
fn spec_color(spec: ColorSpec) -> Color {
    match spec {
        ColorSpec::Default => Color::Reset,
        ColorSpec::Ansi(color) => color16_color(color),
        ColorSpec::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// 16-цветовая палитра crate::theme → цвета ratatui (обычные и яркие).
fn color16_color(color: Color16) -> Color {
    match color {
        Color16::Black => Color::Black,
        Color16::Red => Color::Red,
        Color16::Green => Color::Green,
        Color16::Yellow => Color::Yellow,
        Color16::Blue => Color::Blue,
        Color16::Magenta => Color::Magenta,
        Color16::Cyan => Color::Cyan,
        Color16::White => Color::Gray,
        Color16::BrightBlack => Color::DarkGray,
        Color16::BrightRed => Color::LightRed,
        Color16::BrightGreen => Color::LightGreen,
        Color16::BrightYellow => Color::LightYellow,
        Color16::BrightBlue => Color::LightBlue,
        Color16::BrightMagenta => Color::LightMagenta,
        Color16::BrightCyan => Color::LightCyan,
        Color16::BrightWhite => Color::White,
    }
}

/// Цвет семантической роли активной темы.
fn role_color(theme: &Theme, role: ThemeRole) -> Color {
    spec_color(theme.get(role))
}

/// Стиль семантической роли активной темы.
///
/// В mono-теме у ролей нет цвета ([`ColorSpec::Default`] → [`Color::Reset`]);
/// роль [`ThemeRole::Dim`] там дополнительно получает атрибут DIM, чтобы
/// приглушённый текст оставался визуально отличим («только bold/dim»).
fn role_style(theme: &Theme, role: ThemeRole) -> Style {
    let spec = theme.get(role);
    let style = Style::default().fg(spec_color(spec));
    if role == ThemeRole::Dim && spec == ColorSpec::Default {
        style.add_modifier(Modifier::DIM)
    } else {
        style
    }
}

/// Встроенная палитра TUI по имени темы: «dark» (по умолчанию), «light»,
/// «mono» (без цвета). Неизвестное имя → `None`; сравнение регистронезависимое.
///
/// Палитры построены на типах [`crate::theme`]: тема — это отображение
/// семантических ролей в цвета, а не разбросанные по коду константы.
fn tui_theme(name: &str) -> Option<Theme> {
    let ansi = ColorSpec::Ansi;
    if name.eq_ignore_ascii_case("dark") {
        Some(
            Theme::new("dark")
                .with_role(ThemeRole::Accent, ansi(Color16::BrightMagenta))
                .with_role(ThemeRole::Dim, ansi(Color16::BrightBlack))
                .with_role(ThemeRole::Error, ansi(Color16::Red))
                .with_role(ThemeRole::Warn, ansi(Color16::Yellow))
                .with_role(ThemeRole::Ok, ansi(Color16::Green))
                .with_role(ThemeRole::UserText, ansi(Color16::Green))
                .with_role(ThemeRole::AgentText, ansi(Color16::BrightWhite))
                .with_role(ThemeRole::ToolName, ansi(Color16::Yellow))
                .with_role(ThemeRole::StatusBar, ansi(Color16::Cyan))
                .with_role(ThemeRole::PopupBg, ColorSpec::Rgb(40, 42, 54)),
        )
    } else if name.eq_ignore_ascii_case("light") {
        Some(
            Theme::new("light")
                .with_role(ThemeRole::Accent, ansi(Color16::Magenta))
                .with_role(ThemeRole::Dim, ansi(Color16::BrightBlack))
                .with_role(ThemeRole::Error, ansi(Color16::Red))
                .with_role(ThemeRole::Warn, ansi(Color16::Yellow))
                .with_role(ThemeRole::Ok, ansi(Color16::Green))
                .with_role(ThemeRole::UserText, ansi(Color16::Green))
                .with_role(ThemeRole::AgentText, ansi(Color16::Black))
                .with_role(ThemeRole::ToolName, ansi(Color16::Magenta))
                .with_role(ThemeRole::StatusBar, ansi(Color16::Cyan))
                .with_role(ThemeRole::PopupBg, ColorSpec::Rgb(240, 240, 240)),
        )
    } else if name.eq_ignore_ascii_case("mono") {
        // mono: ни одного цвета — только атрибуты bold/dim
        let mut theme = Theme::new("mono");
        for role in ThemeRole::ALL {
            theme.set(role, ColorSpec::Default);
        }
        Some(theme)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Чистые UI-хелперы: спиннер, время, контекст-бар, slash-completion, ввод
// ---------------------------------------------------------------------------

/// Кадры анимации спиннера «работаю…» (braille), смена — по тикам редрава.
const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Кадр спиннера по номеру тика (100 мс на кадр, цикл по кругу).
fn spinner_frame(tick: u64) -> char {
    SPINNER_FRAMES[tick as usize % SPINNER_FRAMES.len()]
}

/// Формат времени «HH:MM» из unix-секунд и смещения часового пояса (сек).
///
/// Для времени суток достаточно остатка от деления на 86400 — civil-алгоритм
/// с датами не нужен; `rem_euclid` корректно заворачивает отрицательные
/// смещения (полночь UTC при UTC-1 — это 23:00 «вчера»).
fn fmt_hhmm(unix_secs: u64, offset_secs: i64) -> String {
    let day = (unix_secs as i64 + offset_secs).rem_euclid(86_400);
    let (h, m) = (day / 3600, day % 3600 / 60);
    format!("{h:02}:{m:02}")
}

/// Разбор смещения вида «+0300»/«-0530» (вывод `date +%z`) в секунды.
fn parse_tz_offset(text: &str) -> Option<i64> {
    let (sign, digits) = match text.as_bytes() {
        [b'+', rest @ ..] => (1i64, rest),
        [b'-', rest @ ..] => (-1i64, rest),
        _ => return None,
    };
    if digits.len() != 4 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let digits = std::str::from_utf8(digits).ok()?;
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

/// Локальное смещение UTC в секундах, считанное один раз при старте TUI
/// через `date +%z` (без дополнительных крейтов). При любой ошибке — UTC.
fn local_utc_offset_secs() -> i64 {
    std::process::Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|s| parse_tz_offset(s.trim()))
        .unwrap_or(0)
}

/// Процент заполнения контекста (может превышать 100 при переполнении).
fn context_pct(est_tokens: usize, limit: usize) -> usize {
    if limit == 0 {
        return 0;
    }
    est_tokens.saturating_mul(100) / limit
}

/// Текст контекст-бара: 10 ячеек █/░ (заполнение с округлением) + процент.
fn context_bar_text(est_tokens: usize, limit: usize) -> String {
    let pct = context_pct(est_tokens, limit);
    let filled = ((pct.min(100) + 5) / 10).min(10);
    format!("{}{} {pct}%", "█".repeat(filled), "░".repeat(10 - filled))
}

/// Роль цвета контекст-бара по порогам: <60% Ok, <85% Warn, ≥85% Error.
fn context_bar_role(pct: usize) -> ThemeRole {
    if pct < 60 {
        ThemeRole::Ok
    } else if pct < 85 {
        ThemeRole::Warn
    } else {
        ThemeRole::Error
    }
}

/// Чистый разбор `context_limit_tokens` из TOML-текста конфига.
fn parse_context_limit(text: &str) -> Option<usize> {
    let value = text.parse::<toml::Value>().ok()?;
    let raw = value.get("context_limit_tokens")?.as_integer()?;
    usize::try_from(raw).ok()
}

/// Лимит контекста для контекст-бара: `context_limit_tokens` из
/// `~/.config/theseus/config.toml`, дефолт 120000 (как в crate::config).
/// Сигнатуру `run_tui` менять нельзя, а поле лимита у агента приватно,
/// поэтому читаем тот же файл конфига при старте TUI.
fn context_limit_from_config() -> usize {
    const DEFAULT_LIMIT: usize = 120_000;
    let Ok(home) = std::env::var("HOME") else {
        return DEFAULT_LIMIT;
    };
    let path = PathBuf::from(home).join(".config/theseus/config.toml");
    let Ok(text) = std::fs::read_to_string(path) else {
        return DEFAULT_LIMIT;
    };
    parse_context_limit(&text).unwrap_or(DEFAULT_LIMIT)
}

/// Максимум строк в панели slash-completion.
const MAX_COMPLETIONS: usize = 6;

/// Максимум видимых строк многострочного поля ввода (дальше — окно за курсором).
const MAX_INPUT_LINES: usize = 8;

/// Подсказки slash-completion для панели над строкой ввода.
/// Простое правило показа: ввод — это «/» + префикс без пробелов. Голый «/»
/// выводит ВЕСЬ список команд (пользователь осматривает, что доступно);
/// непустой префикс фильтрует по имени или алиасу, регистронезависимо,
/// не больше [`MAX_COMPLETIONS`] команд.
fn slash_completions(input: &str) -> Vec<slash::SlashCmd> {
    let Some(body) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if body.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    // голый «/» — показать всё, что умеет харнесс (как меню у тройки лидеров)
    if body.is_empty() {
        return slash::builtin_commands();
    }
    let needle = body.to_lowercase();
    slash::builtin_commands()
        .into_iter()
        .filter(|cmd| {
            cmd.name.starts_with(needle.as_str())
                || cmd.aliases.iter().any(|alias| alias.starts_with(needle.as_str()))
        })
        .take(MAX_COMPLETIONS)
        .collect()
}

/// Плейсхолдер пустой строки ввода; `None`, когда ввод непустой.
fn input_placeholder(input: &str) -> Option<&'static str> {
    if input.is_empty() {
        Some("задача или /команда…")
    } else {
        None
    }
}

/// Общий префикс имён кандидатов (посимвольно, до первого расхождения).
fn common_name_prefix(cmds: &[slash::SlashCmd]) -> String {
    let Some(first) = cmds.first() else { return String::new(); };
    let mut prefix = first.name.to_string();
    for cmd in &cmds[1..] {
        while !cmd.name.starts_with(prefix.as_str()) {
            prefix.pop();
        }
    }
    prefix
}

/// Автодополнение slash-команды по Tab.
/// Возвращает (новый ввод, состояние цикла кандидатов): ровно один кандидат
/// — полное имя + пробел (готово к Enter), цикл сброшен; несколько — первый
/// Tab даёт общий префикс имён (длиннее введённого), а когда префикс исчерпан —
/// подстановка кандидатов по кругу. Состояние цикла = (исходный префикс,
/// индекс следующего кандидата): совпадения считаются от ИСХОДНОГО префикса,
/// иначе после подстановки полного имени множество схлопывалось бы до одного.
/// Ввод с пробелом (аргументы) или без '/' не дополняется.
fn slash_complete(input: &str, cycle: Option<(String, usize)>) -> Option<(String, Option<(String, usize)>)> {
    let body = input.strip_prefix('/')?;
    if body.is_empty() || body.chars().any(char::is_whitespace) {
        return None;
    }
    let (base, idx) = match &cycle {
        Some((b, i)) => (b.clone(), *i),
        None => (body.to_string(), 0),
    };
    let matches = slash_completions(&format!("/{base}"));
    match matches.len() {
        0 => None,
        1 => Some((format!("/{} ", matches[0].name), None)),
        n => {
            if cycle.is_none() {
                // первый Tab: общий префикс имён, длиннее введённого
                let prefix = common_name_prefix(&matches);
                if prefix.len() > body.len() {
                    return Some((format!("/{prefix}"), None));
                }
            }
            let cmd = &matches[idx % n];
            Some((format!("/{}", cmd.name), Some((base, (idx + 1) % n))))
        }
    }
}

/// Welcome-блок пустого лога (старт TUI без первой задачи): заголовок,
/// модель@url, подсказки клавиш и 3 стартовых промпта из онбординга.
/// Чистая функция — рендерится вместо лога, пока в нём нет ни одной строки.
fn welcome_lines(model_info: &str, theme: &Theme) -> Vec<Line<'static>> {
    let title = role_style(theme, ThemeRole::Accent).add_modifier(Modifier::BOLD);
    let dim = role_style(theme, ThemeRole::Dim);
    let prompt_style = role_style(theme, ThemeRole::UserText);
    let ok = role_style(theme, ThemeRole::Ok);
    let mut lines = vec![];
    // компактный «логотип» рамкой — ширина рамки по контенту (CJK/кириллица — 1 ячейка)
    let logo = " T H E S E U S  — агентный TUI-харнесс ";
    let w = logo.chars().count();
    lines.push(Line::from(Span::styled(format!("  ╔{}╗", "═".repeat(w)), dim)));
    lines.push(Line::from(vec![
        Span::styled("  ║", dim),
        Span::styled(logo.to_string(), title),
        Span::styled("║", dim),
    ]));
    lines.push(Line::from(Span::styled(format!("  ╚{}╝", "═".repeat(w)), dim)));
    lines.extend([
        Line::from(Span::styled(format!("  модель: {model_info}"), dim)),
        Line::raw(""),
        Line::from(Span::styled(
            "  Enter — отправить · ↑/↓ — история · /help — команды · Esc — выход",
            dim,
        )),
        Line::raw(""),
        Line::from(Span::styled("  С чего начать:", title)),
    ]);
    for prompt in crate::onboarding::suggested_starter_prompts().iter().take(3) {
        lines.push(Line::from(vec![
            Span::styled("   • ", ok),
            Span::styled((*prompt).to_string(), prompt_style),
        ]));
    }
    lines
}

// ---------------------------------------------------------------------------
// Выделение текста мышью: состояние, маппинг экран → лог, подсветка
// ---------------------------------------------------------------------------

/// Выделение мышью: якорь (точка MouseDown) и текущая позиция курсора —
/// экранные координаты (колонка, строка), как в crossterm MouseEvent.
#[derive(Clone, Copy, Debug)]
struct Sel {
    anchor: (u16, u16),
    current: (u16, u16),
}

impl Sel {
    /// (start, end) в порядке «выше/левее → ниже/правее»: сравнение по
    /// (строка, колонка), чтобы драг в любую сторону давал одинаковый текст.
    fn normalized(self) -> ((u16, u16), (u16, u16)) {
        if (self.anchor.1, self.anchor.0) <= (self.current.1, self.current.0) {
            (self.anchor, self.current)
        } else {
            (self.current, self.anchor)
        }
    }
}

/// Plain-текст строки лога: конкатенация содержимого спанов (стили сброшены).
fn line_plain(line: &LogLine) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Точка внутри прямоугольника (полуоткрытые границы, как у Rect).
fn point_in_rect(col: u16, row: u16, r: Rect) -> bool {
    (r.x..r.x.saturating_add(r.width)).contains(&col)
        && (r.y..r.y.saturating_add(r.height)).contains(&row)
}

/// Зажать координату в диапазон [start, start+len-1] (len ≥ 1 обязателен).
fn clip_axis(v: u16, start: u16, len: u16) -> u16 {
    v.clamp(start, start.saturating_add(len).saturating_sub(1))
}

/// Зажать экранную точку внутрь области лога (драг может уходить за края).
fn clip_to_area(col: u16, row: u16, area: Rect) -> (u16, u16) {
    (clip_axis(col, area.x, area.width), clip_axis(row, area.y, area.height))
}

/// Извлечь выделенный текст из лога. Маппинг экранной строки в индекс
/// log-строки — ровно как в draw(): follow показывает хвост
/// (`skip(total - visible)`), иначе окно от `scroll`; колонка — позиция
/// в plain-тексте строки без рамки (`x - log_area.x`). Краевые строки
/// обрезаются по колонкам (правая граница не включительна), средние —
/// целиком; склейка через «\n». Строки за пределами лога пропускаются.
/// Чистая функция — тестируется без терминала.
fn extract_selection(app: &TuiApp, sel: Sel) -> String {
    let area = app.log_area;
    if area.width == 0 || area.height == 0 {
        return String::new();
    }
    let (start, end) = sel.normalized();
    let (sx, sy) = clip_to_area(start.0, start.1, area);
    let (ex, ey) = clip_to_area(end.0, end.1, area);
    if (sy, sx) == (ey, ex) {
        return String::new(); // клик без драга — не выделение
    }
    let total = app.log.len();
    let visible = area.height as usize;
    let first = if app.follow {
        total.saturating_sub(visible)
    } else {
        app.scroll.min(total.saturating_sub(1))
    };
    let mut out: Vec<String> = Vec::new();
    for row in sy..=ey {
        let idx = first + (row - area.y) as usize;
        let Some(line) = app.log.get(idx) else { continue };
        let plain = line_plain(line);
        let len = plain.chars().count();
        let from = (if row == sy { (sx - area.x) as usize } else { 0 }).min(len);
        let to = (if row == ey { (ex - area.x) as usize } else { len }).min(len);
        out.push(plain.chars().skip(from).take(to.saturating_sub(from)).collect());
    }
    out.join("\n")
}

/// Подсветка выделения в кадре: REVERSED-модификатор на всех спанах строк
/// между якорем и курсором (v1 — построчно, без резки граничных колонок).
/// Строка body[i] рисуется на экранной строке `area.y + i`.
fn apply_highlight(body: &mut [Line<'static>], sel: Sel, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (start, end) = sel.normalized();
    let sy = clip_axis(start.1, area.y, area.height);
    let ey = clip_axis(end.1, area.y, area.height);
    for (i, line) in body.iter_mut().enumerate() {
        if (sy..=ey).contains(&(area.y + i as u16)) {
            for span in &mut line.spans {
                span.style = span.style.add_modifier(Modifier::REVERSED);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Системный буфер обмена: нативные бэкенды → встроенный python-xlib хелпер
// ---------------------------------------------------------------------------

/// Порядок проб нативных бэкендов: wayland → X11-утилиты.
const NATIVE_CLIP_BACKENDS: [&str; 3] = ["wl-copy", "xclip", "xsel"];

/// Имя файла python-хелпера в ~/.theseus (владелец X11 CLIPBOARD selection).
const CLIP_SCRIPT_NAME: &str = "theseus_clip.py";

/// pidfile действующего владельца буфера (гасим его перед новым захватом).
const CLIP_PIDFILE_NAME: &str = "clip.pid";

/// Таймаут запуска бэкенда и ожидания маркера READY от хелпера.
const CLIP_TIMEOUT: Duration = Duration::from_secs(3);

/// python-xlib хелпер — владелец X11 CLIPBOARD. Пишется в
/// ~/.theseus/theseus_clip.py при первом копировании и запускается detached:
/// в X11 буфер живёт, пока жив процесс-владелец selection, поэтому хелпер
/// остаётся обслуживать SelectionRequest до перехвата (SelectionClear).
const PYTHON_CLIP_HELPER: &str = r#"#!/usr/bin/env python3
"""theseus_clip.py — владелец X11 CLIPBOARD для TUI theseus.

В X11 буфер обмена живёт в процессе-владельце selection: скрипт читает
текст из stdin, захватывает CLIPBOARD и обслуживает SelectionRequest от
вставляющих приложений, пока selection не перехватит другой клиент
(SelectionClear) — тогда завершается. Свой pid пишет в
~/.theseus/clip.pid, чтобы следующий запуск погасил предыдущий инстанс.
Маркер READY в stdout — сигнал родителю, что владение захвачено.
"""
import os
import sys

PIDFILE = os.path.expanduser("~/.theseus/clip.pid")


def main():
    data = sys.stdin.buffer.read()
    try:
        from Xlib import X, Xatom, display
        from Xlib.protocol import event as xevent
    except ImportError:
        print("NOXLIB", flush=True)
        return 1

    d = display.Display()
    screen = d.screen()
    win = screen.root.create_window(0, 0, 1, 1, 0, screen.root_depth)
    clipboard = d.intern_atom("CLIPBOARD")
    targets = d.intern_atom("TARGETS")
    utf8 = d.intern_atom("UTF8_STRING")

    os.makedirs(os.path.dirname(PIDFILE), exist_ok=True)
    with open(PIDFILE, "w") as fh:
        fh.write(str(os.getpid()))

    win.set_selection_owner(clipboard, X.CurrentTime)
    d.sync()
    owner = d.get_selection_owner(clipboard)
    if getattr(owner, "id", 0) != win.id:
        print("NOOWNER", flush=True)
        return 1
    # владение захвачено — Rust ждёт этот маркер (до 3 с)
    print("READY", flush=True)

    while True:
        ev = d.next_event()
        if ev.type == X.SelectionClear and ev.selection == clipboard:
            break
        if ev.type == X.SelectionRequest and ev.selection == clipboard:
            try:
                prop = ev.property
                reply = prop
                if prop == X.NONE:
                    reply = X.NONE
                elif ev.target == targets:
                    # по ICCCM свойство ставится на окно REQUESTOR'а, не на своё
                    ev.requestor.change_property(prop, Xatom.ATOM, 32,
                                                 [targets, utf8, Xatom.STRING])
                elif ev.target in (utf8, Xatom.STRING):
                    ev.requestor.change_property(prop, ev.target, 8, data)
                else:
                    reply = X.NONE  # неподдерживаемый target — отказ
                notify = xevent.SelectionNotify(
                    time=ev.time,
                    requestor=ev.requestor,
                    selection=ev.selection,
                    target=ev.target,
                    property=reply,
                )
                ev.requestor.send_event(notify, propagate=False)
                d.sync()
            except Exception:
                pass  # ошибка одного requestor не роняет владельца буфера
    return 0


def drop_pidfile():
    """Подчистить pidfile при выходе, но только если он всё ещё наш."""
    try:
        with open(PIDFILE) as fh:
            ours = fh.read().strip() == str(os.getpid())
        if ours:
            os.remove(PIDFILE)
    except OSError:
        pass


if __name__ == "__main__":
    try:
        sys.exit(main())
    finally:
        drop_pidfile()
"#;

/// Читалка X11 CLIPBOARD для end-to-end теста хелпера: запрашивает selection
/// у текущего владельца, как любое вставляющее приложение, и печатает текст.
#[cfg(test)]
const PYTHON_CLIP_READER: &str = r#"#!/usr/bin/env python3
"""theseus_clip_reader.py — читалка X11 CLIPBOARD (самопроверка хелпера)."""
import select
import sys

from Xlib import X, display

d = display.Display()
screen = d.screen()
win = screen.root.create_window(0, 0, 1, 1, 0, screen.root_depth)
clipboard = d.intern_atom("CLIPBOARD")
utf8 = d.intern_atom("UTF8_STRING")
prop = d.intern_atom("THESEUS_READBACK")
win.convert_selection(clipboard, utf8, prop, X.CurrentTime)
d.sync()
while True:
    ready, _, _ = select.select([d.fileno()], [], [], 3)
    if not ready:
        print("TIMEOUT", flush=True)
        sys.exit(1)
    ev = d.next_event()
    if ev.type == X.SelectionNotify:
        if ev.property == X.NONE:
            print("REFUSED", flush=True)
            sys.exit(1)
        full = win.get_full_property(prop, X.AnyPropertyType)
        if full is None:
            print("NOPROP", flush=True)
            sys.exit(1)
        sys.stdout.buffer.write(full.value)
        sys.exit(0)
"#;

/// Первый доступный нативный бэкенд буфера. «Искатель» инъецирован,
/// чтобы порядок выбора тестировался без реальной системы (mock fn(&str)->bool).
fn detect_backends(has: impl Fn(&str) -> bool) -> Option<&'static str> {
    NATIVE_CLIP_BACKENDS.into_iter().find(|&prog| has(prog))
}

/// Боевая проверка «программа есть в PATH» (вызывается только при
/// фактическом копировании, не на кадр).
fn in_path(prog: &str) -> bool {
    let dirs = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    dirs.iter().any(|dir| dir.join(prog).is_file())
}

/// Копировать текст в системный буфер. Порядок: первый нативный бэкенд
/// из PATH (wl-copy, xclip, xsel), иначе встроенный python-xlib хелпер.
/// Возвращает имя сработавшего бэкенда либо текст причины фейла.
fn copy_to_clipboard(text: &str) -> std::result::Result<String, String> {
    let via_python = |t: &str| copy_via_python(t).map(|()| "python-xlib".to_string());
    if let Some(prog) = detect_backends(in_path) {
        match run_native_clip(prog, text) {
            Ok(()) => return Ok(prog.to_string()),
            // бэкенд есть, но не сработал (напр., нет WAYLAND_DISPLAY) —
            // идём в python-хелпер, обе причины сохраняем для диагностики
            Err(native_err) => return via_python(text).map_err(|py_err| {
                format!("буфер недоступен: поставьте xclip ({prog}: {native_err}; python-xlib: {py_err})")
            }),
        }
    }
    via_python(text)
        .map_err(|py_err| format!("буфер недоступен: поставьте xclip (python-xlib: {py_err})"))
}

/// Скормить текст нативному бэкенду (stdin) и дождаться выхода ≤ таймаута.
fn run_native_clip(prog: &str, text: &str) -> std::result::Result<(), String> {
    let mut child = Command::new(prog)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("не запустился: {e}"))?;
    feed_stdin(&mut child, text);
    wait_child(&mut child)
}

/// Передать текст на stdin процесса из отдельного потока: большое выделение
/// не должно заблокировать UI о полный буфер пайпа.
fn feed_stdin(child: &mut std::process::Child, text: &str) {
    use std::io::Write;
    if let Some(mut stdin) = child.stdin.take() {
        let bytes = text.as_bytes().to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
}

/// Дождаться завершения процесса до CLIP_TIMEOUT; по таймауту — kill.
fn wait_child(child: &mut std::process::Child) -> std::result::Result<(), String> {
    let deadline = Instant::now() + CLIP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("код выхода {status}")),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let secs = CLIP_TIMEOUT.as_secs();
                return Err(format!("таймаут {secs}с"));
            }
            Err(e) => return Err(format!("ошибка ожидания: {e}")),
        }
    }
}

/// Каталог данных theseus (~/.theseus).
fn theseus_dir() -> std::result::Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME не задан".to_string())?;
    Ok(PathBuf::from(home).join(".theseus"))
}

/// Погасить прежнего владельца буфера по pidfile: X11 CLIPBOARD живёт,
/// пока жив процесс-владелец, поэтому перед новым захватом старый хелпер
/// завершаем (иначе он висел бы до перехвата selection).
fn kill_prev_clip_owner(dir: &Path) {
    let pidfile = dir.join(CLIP_PIDFILE_NAME);
    let Ok(content) = std::fs::read_to_string(&pidfile) else { return };
    let pid = content.trim();
    // pidfile пишем сами, но файл мог повредиться: принимаем только цифры
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return;
    }
    let alive = Command::new("kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if alive {
        let _ = Command::new("kill")
            .arg(pid)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = std::fs::remove_file(&pidfile);
}

/// Копирование через python-xlib хелпер: записать скрипт (если нет или
/// устарел), погасить прежнего владельца, запустить detached с текстом на
/// stdin и дождаться READY из stdout.
fn copy_via_python(text: &str) -> std::result::Result<(), String> {
    let dir = theseus_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("нет каталога {}: {e}", dir.display()))?;
    let script = dir.join(CLIP_SCRIPT_NAME);
    // перезаписываем и при расхождении версий: иначе после обновления
    // theseus на диске остался бы старый хелпер
    let current = std::fs::read_to_string(&script).unwrap_or_default();
    if current != PYTHON_CLIP_HELPER {
        std::fs::write(&script, PYTHON_CLIP_HELPER)
            .map_err(|e| format!("не записан {}: {e}", script.display()))?;
    }
    kill_prev_clip_owner(&dir);
    let mut child = Command::new("python3")
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("python3 не запустился: {e}"))?;
    feed_stdin(&mut child, text);
    wait_ready(&mut child)?;
    // жнец в фоне: хелпер выйдет при перехвате selection — без зомби;
    // Child намеренно не wait'им в UI-потоке (хелпер живёт владельцем буфера)
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Дождаться строки READY из stdout хелпера до CLIP_TIMEOUT (чтение — в
/// отдельном потоке, чтобы не висеть дольше таймаута). По таймауту хелпер
/// убиваем.
fn wait_ready(child: &mut std::process::Child) -> std::result::Result<(), String> {
    use std::io::BufRead;
    let Some(mut stdout) = child.stdout.take() else {
        return Err("нет stdout у хелпера".to_string());
    };
    let (tx, rx) = channel::<Option<String>>();
    std::thread::spawn(move || {
        let mut line = String::new();
        let read = std::io::BufReader::new(&mut stdout).read_line(&mut line);
        let _ = tx.send(match read {
            Ok(n) if n > 0 => Some(line),
            _ => None,
        });
    });
    match rx.recv_timeout(CLIP_TIMEOUT) {
        Ok(Some(line)) if line.trim() == "READY" => Ok(()),
        Ok(Some(line)) => Err(format!("хелпер ответил «{}» вместо READY", line.trim())),
        Ok(None) => Err("хелпер завершился без READY".to_string()),
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            let secs = CLIP_TIMEOUT.as_secs();
            Err(format!("таймаут READY {secs}с"))
        }
    }
}

pub struct TuiApp {
    log: Vec<LogLine>,
    input: String,
    status: String,
    accounting: String,
    /// git-контекст заголовка: «⎇ ветка» / «⎇ ветка*» (звёздочка — dirty)
    git_status: String,
    scroll: usize,
    follow: bool,
    agent_done: bool,
    started_at: Instant,
    stream_open: bool,
    /// индекс ПЕРВОЙ строки стрим-блока в логе (markdown налету, v0.6.4):
    /// блок перерендеривается целиком на каждую дельту в этой позиции
    stream_line_idx: Option<usize>,
    /// накопленный текст текущего стрима (дельты конкатенируются, блок
    /// перерендеривается с него; очищается на AgentText)
    stream_text: String,
    /// длина стрим-блока в строках лога (для drain при перерендере)
    stream_block_len: usize,
    /// активная цветовая тема (дизайн-токены crate::theme; dark по умолчанию)
    theme: Theme,
    /// оценка токенов из последнего Status — для контекст-бара заголовка
    ctx_est_tokens: Option<usize>,
    /// лимит контекста из ~/.config/theseus/config.toml (дефолт 120000)
    ctx_limit: usize,
    /// смещение локального часового пояса (сек) для префиксов времени
    tz_offset_secs: i64,
    /// агент выполняет задачу — показывать спиннер в заголовке
    agent_running: bool,
    /// «модель @ url» для welcome-блока пустого лога
    model_info: String,
    /// состояние цикла автодополнения slash-команд (индекс следующего кандидата);
    /// сбрасывается любой клавишей кроме Tab
    completion_cycle: Option<(String, usize)>,
    /// тип текущего блока лога — для разделителей между блоками (воздух)
    /// и единого timestamp'а на первой строке блока (дизайн v0.5.4)
    block_kind: Option<BlockKind>,
    /// индекс последней строки вызова инструмента: результат допишется в неё же
    /// (компактный трейс в одну строку — как у лидеров, v0.6.0)
    last_tool_open: Option<usize>,
    /// активное выделение мышью в области лога (экранные координаты)
    sel: Option<Sel>,
    /// внутренняя область лога без рамки (заполняется в draw каждый кадр)
    log_area: Rect,
    /// код режима разрешений из Controls.mode_atomic (обновляется в цикле run_tui):
    /// индикатор режима в заголовке ввода слева (Совет/Авто-правки/Автомат)
    mode_code: u8,
    /// курсор в поле ввода — символьный индекс (многострочный ввод v0.6.3):
    /// вставка/удаление идут по курсору, не только в конце строки
    cursor: usize,
}

/// Тип блока в логе: пользователь / ответ агента / инструменты / системные заметки.
/// Между разными типами вставляется пустая строка-разделитель.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind { User, Agent, Tool, Notice }

/// Санитация текста перед показом в терминале: управляющие символы C0/C1
/// (form feed \x0c из pdftotext, \x08, \x0b, табы) нельзя отдавать в вывод —
/// xterm трактует FF/VT как перевод строки БЕЗ возврата каретки, а BS двигает
/// курсор назад. ratatui про это не знает: курсор «уплывает», кадр рисует
/// мусорные хвосты (баг скриншота 16-42-57: «conditioned on its decision»
/// размножен по правому краю — хвост preview из bash-вывода pdftotext).
/// \n и \r → видимый «⏎», прочие управляющие → пробел, остальное как есть.
fn sanitize_log_str(s: &str) -> Cow<'_, str> {
    if !s.chars().any(char::is_control) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(s.chars().map(|c| match c {
        '\n' | '\r' => '⏎',
        c if c.is_control() => ' ',
        c => c,
    }).collect())
}

/// Точная высота строк с учётом переносов: рендер в офскрин-буфер той же
/// ширины (тот же WordWrapper, что и у основного рендера кадра) и поиск
/// последней непустой строки. Нужна для пиннинга низа лога (автопрокрутка):
/// логические строки с wrap занимают больше экрана, чем считает их количество.
fn wrapped_height(lines: Vec<Line>, width: u16, max_height: u16) -> usize {
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Widget;
    let area = Rect::new(0, 0, width.max(1), max_height.max(1));
    let mut buf = Buffer::empty(area);
    Widget::render(Paragraph::new(lines).wrap(Wrap { trim: false }), area, &mut buf);
    for y in (0..max_height).rev() {
        if (0..width).any(|x| buf.get(x, y).symbol() != " ") {
            return y as usize + 1;
        }
    }
    0
}

/// Позиция сразу после последнего контентного символа текста с переносами —
/// (строка, колонка) в визуальных координатах. Точный ответ для курсора поля
/// ввода при враппинге длинных строк (офскрин-рендер тем же WordWrapper'ом).
fn wrapped_end_pos(lines: Vec<Line>, width: u16, max_height: u16) -> (usize, usize) {
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Widget;
    let area = Rect::new(0, 0, width.max(1), max_height.max(1));
    let mut buf = Buffer::empty(area);
    Widget::render(Paragraph::new(lines).wrap(Wrap { trim: false }), area, &mut buf);
    for y in (0..max_height).rev() {
        for x in (0..width).rev() {
            if buf.get(x, y).symbol() != " " {
                return (y as usize, x as usize + 1);
            }
        }
    }
    (0, 0)
}

/// Визуальная позиция курсора поля ввода: рендер префикса с сентинелем «█»
/// на конце — иначе хвостовой пробел (пустая ячейка) не двигал курсор, и при
/// вводе казалось, что пробел «не нажимается» (баг пользователя 20.07).
fn wrapped_cursor_pos(lines: Vec<Line>, width: u16, max_height: u16) -> (usize, usize) {
    let (y, x) = wrapped_end_pos(lines, width, max_height);
    // курсор — ПОД сентинелем: wrapped_end_pos вернул позицию ПОСЛЕ него
    (y, x.saturating_sub(1))
}

/// Байтовый индекс символа `char_idx` в строке (для insert/remove по курсору).
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(s.len())
}

/// Вставка текста в поле ввода по курсору (курсор — символьный индекс).
fn input_insert(input: &mut String, cursor: &mut usize, text: &str) {
    input.insert_str(char_to_byte(input, *cursor), text);
    *cursor += text.chars().count();
}

/// Backspace по курсору: удалить символ перед ним (включая `\n` —
/// многострочное поле при удалении переноса сжимается обратно).
fn input_backspace(input: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    input.remove(char_to_byte(input, *cursor - 1));
    *cursor -= 1;
}

/// Санитация вставляемого (paste) текста: CRLF/CR → LF, таб → 4 пробела,
/// прочие управляющие символы (C0/C1, кроме `\n`) выбрасываются — иначе
/// форм-фиды и табы ломают отрисовку кадра (урок бага 16-42-57).
fn sanitize_paste(s: &str) -> String {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Санитация стрим-дельты ДО markdown-рендера: управляющие символы → пробел,
/// но `\n` сохраняем — это структура markdown (в отличие от лога, где \n → ⏎).
fn sanitize_stream(s: &str) -> String {
    s.chars().map(|c| if c.is_control() && c != '\n' { ' ' } else { c }).collect()
}

/// Санитация спанов строки лога от управляющих символов (общий код push() и
/// вставок стрим-блока — ни один путь не должен отдать C0/C1 в терминал).
fn sanitize_spans(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    spans.into_iter().map(|sp| {
        if sp.content.chars().any(char::is_control) {
            Span::styled(sanitize_log_str(&sp.content).into_owned(), sp.style)
        } else {
            sp
        }
    }).collect()
}

/// Приблизительная высота логической строки лога в визуальных строках
/// (ceil по числу символов; широкие runes занижают оценку — компенсируется
/// запасом окна и точным пиннингом через wrapped_height).
fn approx_rows(spans: &[Span], width: usize) -> usize {
    let w: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if w == 0 { 1 } else { w.div_ceil(width.max(1)) }
}

/// Верх окна ручного скролла, при котором экран ещё полон: самая поздняя
/// логическая строка, от которой хватает контента на `visible_rows` визуальных.
/// Кламп не даёт колесу «провалиться» в пустоту под концом лога.
fn manual_max_top(log: &[LogLine], visible_rows: usize, width: usize) -> usize {
    let mut approx = 0usize;
    let mut top = log.len();
    while top > 0 && approx < visible_rows {
        top -= 1;
        approx += approx_rows(&log[top].spans, width);
    }
    top
}

impl TuiApp {
    fn new() -> Self {
        TuiApp {
            log: vec![], input: String::new(),
            status: "инициализация…".into(), accounting: String::new(),
            git_status: String::new(),
            scroll: 0, follow: true, agent_done: false, started_at: Instant::now(),
            stream_open: false, stream_line_idx: None,
            stream_text: String::new(), stream_block_len: 0,
            // дефолты для тестов и до инициализации в run_tui: dark-тема,
            // стандартный лимит, UTC; боевые значения подставляет run_tui
            theme: tui_theme("dark").unwrap_or_else(|| Theme::new("dark")),
            ctx_est_tokens: None, ctx_limit: 120_000, tz_offset_secs: 0,
            agent_running: false, model_info: String::new(),
            completion_cycle: None, block_kind: None,
            sel: None, log_area: Rect::default(), last_tool_open: None,
            mode_code: crate::permissions::MODE_UNSET,
            cursor: 0,
        }
    }
    fn push(&mut self, spans: Vec<Span<'static>>) {
        // единая точка входа строк в лог — чистим управляющие символы здесь,
        // чтобы ни один путь (markdown, события, статусы) не расстроил терминал
        self.log.push(LogLine { spans: sanitize_spans(spans) });
        if self.follow { self.scroll = self.log.len().saturating_sub(1); }
    }
    /// Перерендер стрим-блока (markdown налету, v0.6.4): старые rendered-строки
    /// снимаются по stream_line_idx, новые вставляются туда же. Строки,
    /// добавленные ПОСЛЕ начала стрима (Reasoning и т.п.), не задеваются.
    fn render_stream_block(&mut self) {
        let Some(idx) = self.stream_line_idx else { return };
        let end = (idx + self.stream_block_len).min(self.log.len());
        if idx < end {
            self.log.drain(idx..end);
        }
        let agent = role_style(&self.theme, ThemeRole::AgentText);
        let rendered = crate::markdown::render(&self.stream_text, 100);
        let lines = md_ansi_to_lines(&rendered);
        let count = lines.len();
        for (off, spans) in lines.into_iter().enumerate() {
            let mut line = if off == 0 {
                self.gutter_first("◆ ", agent)
            } else {
                self.gutter_cont()
            };
            line.extend(spans);
            self.log.insert(idx + off, LogLine { spans: sanitize_spans(line) });
        }
        self.stream_block_len = count;
        if self.follow { self.scroll = self.log.len().saturating_sub(1); }
    }
    /// Верх окна ручного скролла с полным экраном (кламп против «провала»
    /// колеса в пустоту под концом лога). Читает log_area последнего кадра.
    fn max_scroll(&self) -> usize {
        manual_max_top(&self.log, self.log_area.height as usize,
                       self.log_area.width.max(1) as usize)
    }
    /// Начало блока нового типа: вставить пустую строку-разделитель
    /// (воздух между блоками — междустрочный ритм вместо сплошной простыни).
    fn begin_block(&mut self, kind: BlockKind) {
        if self.block_kind.is_some_and(|k| k != kind) && !self.log.is_empty() {
            self.push(Vec::new());
        }
        self.block_kind = Some(kind);
    }
    /// Текущее время «HH:MM» (локальный пояс, сэмплирован при старте).
    fn ts(&self) -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        fmt_hhmm(secs, self.tz_offset_secs)
    }
    /// Желобок первой строки блока: «HH:MM ❯» — время один раз на блок.
    fn gutter_first(&self, marker: &str, style: Style) -> Vec<Span<'static>> {
        vec![
            Span::styled(format!("{} ", self.ts()), role_style(&self.theme, ThemeRole::Dim)),
            Span::styled(marker.to_string(), style),
        ]
    }
    /// Желобок продолжения: тонкая вертикаль — блок читается как единое целое.
    fn gutter_cont(&self) -> Vec<Span<'static>> {
        vec![Span::styled("     │ ", role_style(&self.theme, ThemeRole::Dim))]
    }
    /// Желобок вложенной строки (результат инструмента).
    fn gutter_sub(&self) -> Vec<Span<'static>> {
        vec![Span::styled("     ↳ ", role_style(&self.theme, ThemeRole::Dim))]
    }
    fn on_event(&mut self, ev: AgentEvent) {
        if !matches!(ev, AgentEvent::AgentTextDelta(_)) {
            self.stream_open = false;
        }
        // стили — из активной темы (дизайн-токены), а не хардкод-цвета;
        // клонируем раз на событие, чтобы не держать заимствование self
        let theme = self.theme.clone();
        let dim = role_style(&theme, ThemeRole::Dim);
        let accent = role_style(&theme, ThemeRole::Accent);
        let error = role_style(&theme, ThemeRole::Error);
        match ev {
            AgentEvent::AgentTextDelta(s) => {
                // стриминг с markdown налету (v0.6.4, запрос пользователя):
                // дельта копится в stream_text, блок перерендеривается целиком —
                // разметка оформляется по мере поступления, а не в конце ответа.
                // Незакрытые маркеры парсер показывает литералом (устаканиваются
                // с приходом закрывающего маркера).
                if self.stream_line_idx.is_none() {
                    self.begin_block(BlockKind::Agent);
                    self.stream_line_idx = Some(self.log.len());
                    self.stream_block_len = 0;
                }
                self.stream_open = true;
                self.stream_text.push_str(&sanitize_stream(&s));
                self.render_stream_block();
            }
            AgentEvent::UserMsg(t) => {
                self.stream_open = false;
                self.begin_block(BlockKind::User);
                let user = role_style(&theme, ThemeRole::UserText);
                let mut line = self.gutter_first("❯ ", user);
                line.push(Span::styled(t, user.add_modifier(Modifier::BOLD)));
                self.push(line);
            }
            AgentEvent::AgentText(t) => {
                // финальный текст — тот же блок, последний перерендер точным текстом
                // (на случай потерянных дельт); стрим-состояние сбрасывается
                self.stream_text = t;
                if self.stream_line_idx.is_none() {
                    // ответ пришёл одним куском без дельт — блока ещё нет
                    self.begin_block(BlockKind::Agent);
                    self.stream_line_idx = Some(self.log.len());
                    self.stream_block_len = 0;
                }
                self.render_stream_block();
                self.stream_open = false;
                self.stream_line_idx = None;
                self.stream_block_len = 0;
                self.stream_text.clear();
            }
            AgentEvent::Reasoning(n) => {
                self.begin_block(BlockKind::Agent);
                let mut line = self.gutter_sub();
                line.push(Span::styled(format!("(мышление: {n} символов)"), dim));
                self.push(line);
            }
            AgentEvent::ToolCall { name, args, decision } => {
                let short: String = args.chars().take(80).collect();
                let tool = role_style(&theme, ThemeRole::ToolName);
                self.begin_block(BlockKind::Tool);
                let mut line = self.gutter_first("⚙ ", tool);
                line.extend([
                    Span::styled(name, tool.add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" {short}"), dim),
                    Span::styled(format!(" [{decision}]"), dim),
                ]);
                self.push(line);
                // строка открыта: результат инструмента допишем в неё же
                // (компактный трейс в одну строку — как у лидеров, v0.6.0)
                self.last_tool_open = Some(self.log.len() - 1);
            }
            AgentEvent::ToolResult { preview, ok, .. } => {
                let style = if ok { dim } else { error };
                // встроенные \n в preview (многострочный read_file) заменяем
                // на видимый разделитель ⏎: иначе ratatui рисует их инлайн,
                // строки сливаются в одну длинную и перенос рвёт слова
                // (баг «висячих хвостов» на скриншоте 12-00-24);
                // остальные управляющие (\x0c из pdftotext, \x08) — через
                // sanitize_log_str: xterm исполняет их как курсорные команды,
                // кадр разъезжается (баг скриншота 16-42-57)
                let short: String = sanitize_log_str(&preview.chars().take(90)
                    .collect::<String>().replace('\n', " ⏎ ")).into_owned();
                // результат сразу после вызова — дописываем в ту же строку (1 строка
                // на инструмент вместо двух: трейс не уходит вниз, скролл не нужен)
                if self.last_tool_open == Some(self.log.len().saturating_sub(1))
                    && !self.log.is_empty()
                {
                    let idx = self.log.len() - 1;
                    self.log[idx].spans.push(Span::styled(format!("  → {short}"), style));
                    self.last_tool_open = None;
                } else {
                    self.last_tool_open = None;
                    self.begin_block(BlockKind::Tool);
                    let mut line = self.gutter_sub();
                    line.push(Span::styled(short, style));
                    self.push(line);
                }
            }
            AgentEvent::Status { turns, est_tokens, mode } => {
                // запоминаем оценку заполнения контекста для бара в заголовке
                self.ctx_est_tokens = Some(est_tokens);
                self.status = format!("ход {turns} | ~{est_tokens} ток | {mode} | {:.0}s",
                                      self.started_at.elapsed().as_secs_f32());
            }
            AgentEvent::Compact { from_msgs, to_msgs } => {
                self.begin_block(BlockKind::Notice);
                let mut line = self.gutter_first("⤓ ", accent);
                line.push(Span::styled(format!("компактификация: {from_msgs} → {to_msgs} сообщений"), accent));
                self.push(line);
            }
            AgentEvent::TodoRejected(m) => {
                self.begin_block(BlockKind::Notice);
                let mut line = self.gutter_first("⛔ ", error);
                line.push(Span::styled(m, error));
                self.push(line);
            }
            AgentEvent::Finished(s) => {
                self.begin_block(BlockKind::Notice);
                let ok = role_style(&theme, ThemeRole::Ok);
                let mut line = self.gutter_first("✔ ", ok.add_modifier(Modifier::BOLD));
                line.push(Span::styled(s, ok));
                self.push(line);
                self.agent_done = true;
            }
            AgentEvent::Error(e) => {
                self.begin_block(BlockKind::Notice);
                let mut line = self.gutter_first("✖ ", error);
                line.push(Span::styled(e, error));
                self.push(line);
                self.agent_done = true;
            }
            AgentEvent::Accounting { calls, prompt_t, completion_t } => {
                self.accounting = format!("API: {calls} выз. | токены {prompt_t}+{completion_t}");
            }
            AgentEvent::GoalSet(g) => {
                self.begin_block(BlockKind::Notice);
                let mut line = self.gutter_first("🎯 ", accent);
                line.push(Span::styled(format!("GOAL: {g}"), accent.add_modifier(Modifier::BOLD)));
                self.push(line);
            }
            AgentEvent::PlanChanged(on) => {
                self.begin_block(BlockKind::Notice);
                let mut line = self.gutter_first("📋 ", accent);
                line.push(Span::styled(format!("plan mode: {}", if on { "ON (только чтение)" } else { "OFF" }), accent));
                self.push(line);
            }
            AgentEvent::MemoryConsolidated(n) => {
                self.begin_block(BlockKind::Notice);
                let mut line = self.gutter_first("🧠 ", dim);
                line.push(Span::styled(format!("память: консолидировано {n} фактов"), dim));
                self.push(line);
            }
            AgentEvent::HookNote(n) => {
                self.begin_block(BlockKind::Notice);
                let mut line = self.gutter_first("🪝 ", dim);
                line.push(Span::styled(n, dim));
                self.push(line);
            }
            AgentEvent::PermAsk { .. } => {}
        }
    }
}

fn draw(f: &mut ratatui::Frame, app: &mut TuiApp, perm_q: Option<&str>) {
    // панель slash-completion над вводом: пока ввод — «/префикс» без пробелов.
    // Голый «/» отдаёт ВЕСЬ список команд — панель не должна съедать экран:
    // максимум ~60% высоты терминала, остаток заменяет строка «…ещё N».
    let completions = slash_completions(&app.input);
    let max_rows = ((f.size().height as usize) * 3 / 5).saturating_sub(2).max(3);
    let shown = completions.len().min(max_rows);
    let hidden = completions.len() - shown;
    let completion_height = if shown == 0 {
        0
    } else {
        shown as u16 + 2 + u16::from(hidden > 0)
    };
    // многострочный ввод (v0.6.3): высота по ВИЗУАЛЬНЫМ строкам — переносы и
    // враппинг длинных строк при ручном вводе тоже растят поле (по умолчанию
    // одна строка); буфер — точная верхняя оценка высоты по байтам
    let input_width = f.size().width.saturating_sub(2).max(1);
    let input_cap = (app.input.len() / input_width as usize
        + app.input.lines().count() + 2) as u16;
    let input_lines: Vec<Line> = app.input.split('\n').map(Line::from).collect();
    let input_rows = wrapped_height(input_lines, input_width, input_cap).max(1);
    let input_height = (input_rows.min(MAX_INPUT_LINES) + 2) as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(completion_height),
            Constraint::Length(input_height),
        ])
        .split(f.size());

    // заголовок: бейдж, статус агента + учёт API, спиннер, контекст-бар, git
    let badge = if app.theme.get(ThemeRole::StatusBar) == ColorSpec::Default {
        // mono-тема: цвета нет — бейдж выделяем только жирным
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Black)
            .bg(role_color(&app.theme, ThemeRole::StatusBar))
            .add_modifier(Modifier::BOLD)
    };
    let mut head = vec![
        Span::styled(" theseus ", badge),
        Span::styled(
            format!("  {}  {}", app.status, app.accounting),
            role_style(&app.theme, ThemeRole::AgentText),
        ),
    ];
    let sep = || Span::styled(" │ ", role_style(&app.theme, ThemeRole::Dim));
    if app.agent_running {
        // агент работает: анимированный спиннер (кадр по тикам редрава 100 мс)
        let tick = app.started_at.elapsed().as_millis() as u64 / 100;
        head.push(sep());
        head.push(Span::styled(
            format!("{} работаю…", spinner_frame(tick)),
            role_style(&app.theme, ThemeRole::Accent),
        ));
    }
    if let Some(est) = app.ctx_est_tokens {
        let pct = context_pct(est, app.ctx_limit);
        head.push(sep());
        head.push(Span::styled(
            context_bar_text(est, app.ctx_limit),
            role_style(&app.theme, context_bar_role(pct)),
        ));
    }
    if !app.git_status.is_empty() {
        head.push(sep());
        head.push(Span::styled(
            app.git_status.clone(),
            role_style(&app.theme, ThemeRole::Warn),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(head)), chunks[0]);

    // лог; пока он пуст (старт без первой задачи) — welcome-блок.
    // log_area — внутренняя область без рамки: от неё считается маппинг
    // координат мыши (выделение); обновляется каждый кадр
    let log_block = Block::default().borders(Borders::ALL).title(" лог ");
    app.log_area = log_block.inner(chunks[1]);
    let visible_rows = chunks[1].height.saturating_sub(2) as usize;
    let mut body: Vec<Line> = if app.log.is_empty() {
        welcome_lines(&app.model_info, &app.theme)
    } else {
        let total = app.log.len();
        if app.follow {
            // автопрокрутка: суффикс последних visible+20 ЛОГИЧЕСКИХ строк,
            // точный пиннинг низа по высоте с переносами — ниже. Раньше брали
            // последние `visible` строк и рисовали с их начала: длинные
            // обёрнутые ответы уходили за нижний край, и пользователю
            // приходилось догонять лог мышью вручную.
            app.log.iter().skip(total.saturating_sub(visible_rows + 20))
                .map(|l| Line::from(l.spans.clone())).collect()
        } else {
            // ручной скролл: верх окна — логическая строка scroll, окно добирается
            // вперёд по приблизительной высоте до полного экрана и пиннится низом
            // (pin ниже). Верх клампится к manual_max_top: колесо не «проваливается»
            // в пустоту под концом лога (баг пользователя 20.07).
            let width = app.log_area.width.max(1) as usize;
            let top = app.scroll.min(manual_max_top(&app.log, visible_rows, width));
            let mut approx = 0usize;
            let mut end = top;
            while end < total && approx < visible_rows + 2 {
                approx += approx_rows(&app.log[end].spans, width);
                end += 1;
            }
            app.log.iter().skip(top).take(end - top)
                .map(|l| Line::from(l.spans.clone())).collect()
        }
    };
    // подсветка активного выделения мышью: REVERSED на спанах попавших строк
    if let Some(sel) = app.sel {
        if !app.log.is_empty() {
            apply_highlight(&mut body, sel, app.log_area);
        }
    }
    // «думающая» строка (v0.5.5): пока агент работает и стрим ещё не открыт —
    // виртуальная строка-спиннер в конце лога (не пишется в log, живёт только
    // в кадре). Глаз пользователя замечает её сразу, в отличие от шапки.
    if app.agent_running && !app.stream_open && !app.log.is_empty() {
        let tick = app.started_at.elapsed().as_millis() as u64 / 100;
        body.push(Line::from(vec![
            Span::styled(format!("{} ", spinner_frame(tick)), role_style(&app.theme, ThemeRole::Accent)),
            Span::styled("думаю…".to_string(), role_style(&app.theme, ThemeRole::Dim)),
        ]));
    }
    // точный пиннинг низа в ОБОИХ режимах: реальная высота окна с переносами
    // (офскрин-рендер тем же WordWrapper'ом) минус экран. Follow — низ лога,
    // ручной скролл — низ окна (полный экран контента, без пустого «подвала»)
    let pin = if !app.log.is_empty() {
        wrapped_height(body.clone(), app.log_area.width,
                       visible_rows as u16 * 4 + 60)
            .saturating_sub(visible_rows) as u16
    } else {
        0
    };
    f.render_widget(
        Paragraph::new(body).block(log_block).wrap(Wrap { trim: false }).scroll((pin, 0)),
        chunks[1]);

    // панель совпадений slash-команд: имя + summary; при голом «/» — весь
    // список (обрезка по высоте терминала со строкой «…ещё N»)
    if shown > 0 {
        let accent = role_style(&app.theme, ThemeRole::Accent);
        let dim = role_style(&app.theme, ThemeRole::Dim);
        let mut lines: Vec<Line> = completions
            .iter()
            .take(shown)
            .map(|cmd| {
                let name = cmd.name;
                Line::from(vec![
                    Span::styled(format!("/{name:<10}"), accent),
                    Span::styled(cmd.summary, dim),
                ])
            })
            .collect();
        if hidden > 0 {
            lines.push(Line::from(Span::styled(
                format!("…ещё {hidden} — продолжайте ввод"), dim)));
        }
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" команды ")),
            chunks[2]);
    }

    // заголовок ввода: слева — индикатор режима разрешений (Совет/Авто-правки/Автомат,
    // цвет по режиму); пока агент работает — крупный видимый индикатор мышления
    let mode_badge = match app.mode_code {
        crate::permissions::MODE_SEMI => (" Авто-правки ", ThemeRole::Accent),
        crate::permissions::MODE_YOLO => (" Автомат ", ThemeRole::Ok),
        crate::permissions::MODE_ASK => (" Совет ", ThemeRole::Warn),
        _ => (" ", ThemeRole::Dim),
    };
    let mut title_spans: Vec<Span> = Vec::new();
    if !mode_badge.0.trim().is_empty() {
        title_spans.push(Span::styled(mode_badge.0.to_string(),
            role_style(&app.theme, mode_badge.1).add_modifier(Modifier::BOLD)));
        title_spans.push(Span::styled("│ ".to_string(), role_style(&app.theme, ThemeRole::Dim)));
    }
    let (hint, hint_style) = if app.agent_running {
        let tick = app.started_at.elapsed().as_millis() as u64 / 100;
        (format!("{} агент думает… (Enter — в очередь · Ctrl+S — вставить сразу · Esc — прервать) ",
                 spinner_frame(tick)),
         role_style(&app.theme, ThemeRole::Accent).add_modifier(Modifier::BOLD))
    } else if app.agent_done {
        ("Enter — новая задача | драг — выделение | Esc — выход ".to_string(),
         role_style(&app.theme, ThemeRole::Dim))
    } else {
        ("Enter — отправить | Ctrl+N — новая строка | ↑/↓ — история | /help — команды | PgUp/PgDn/колесо — скролл | драг — выделение | Esc — выход ".to_string(),
         role_style(&app.theme, ThemeRole::Dim))
    };
    title_spans.push(Span::styled(hint, hint_style));
    let input_block = Block::default().borders(Borders::ALL)
        .title(Line::from(title_spans));
    // курсор в визуальных координатах: офскрин-рендер префикса ввода с
    // сентинелем «█» — и переносы, и враппинг, и ХВОСТОВЫЕ ПРОБЕЛЫ учтены точно
    let cursor_byte = char_to_byte(&app.input, app.cursor);
    let probe = format!("{}█", &app.input[..cursor_byte]);
    let probe_lines: Vec<Line> = probe.split('\n').map(Line::from).collect();
    let (cur_row, cur_col) = wrapped_cursor_pos(probe_lines, input_width, input_cap);
    let visible_n = input_rows.min(MAX_INPUT_LINES);
    let offset = if cur_row >= visible_n { cur_row + 1 - visible_n } else { 0 };
    match input_placeholder(&app.input) {
        // пустой ввод — dim-плейсхолдер вместо пустой строки
        Some(placeholder) => f.render_widget(
            Paragraph::new(Line::from(Span::styled(placeholder, role_style(&app.theme, ThemeRole::Dim))))
                .block(input_block),
            chunks[3]),
        None => f.render_widget(
            Paragraph::new(app.input.as_str()).block(input_block)
                .wrap(Wrap { trim: false }).scroll((offset as u16, 0)),
            chunks[3]),
    }
    // курсор терминала — внутри рамки поля
    let cx = chunks[3].x + 1 + cur_col as u16;
    let cy = chunks[3].y + 1 + (cur_row - offset) as u16;
    f.set_cursor(
        cx.min(chunks[3].x + chunks[3].width.saturating_sub(2)),
        cy.min(chunks[3].y + chunks[3].height.saturating_sub(2)),
    );

    if let Some(q) = perm_q {
        let area = centered(60, 30, f.size());
        f.render_widget(Clear, area);
        let block = Block::default().borders(Borders::ALL).title(" запрос разрешения ")
            .border_style(role_style(&app.theme, ThemeRole::Warn));
        let inner = block.inner(area);
        f.render_widget(block.style(Style::default().bg(role_color(&app.theme, ThemeRole::PopupBg))), area);
        // две зоны: вопрос сверху (обрезается по высоте), ответы — фиксированные
        // 2 строки снизу. Длинная команда не может вытеснить [y]/[a]/[n]
        // (баг скриншота 12-15-53: подсказки ответа уехали за край попапа)
        let zones = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(inner);
        let q_text = crate::textutil::cap_lines(q, zones[0].height as usize);
        f.render_widget(Paragraph::new(q_text).wrap(Wrap { trim: true }), zones[0]);
        let answer_style = role_style(&app.theme, ThemeRole::Warn).add_modifier(Modifier::BOLD);
        f.render_widget(Paragraph::new(vec![
            Line::from(Span::styled("[y] разрешить  [a] всегда*  [n] отклонить".to_string(), answer_style)),
            Line::from(Span::styled("(*«всегда» — до конца сессии)".to_string(), role_style(&app.theme, ThemeRole::Dim))),
        ]), zones[1]);
    }
}

fn centered(px: u16, py: u16, r: Rect) -> Rect {
    let v = Layout::default().direction(Direction::Vertical)
        .constraints([Constraint::Percentage((100 - py) / 2), Constraint::Percentage(py), Constraint::Percentage((100 - py) / 2)])
        .split(r);
    Layout::default().direction(Direction::Horizontal)
        .constraints([Constraint::Percentage((100 - px) / 2), Constraint::Percentage(px), Constraint::Percentage((100 - px) / 2)])
        .split(v[1])[1]
}

/// Рабочий каталог для локальных команд TUI (сессии, трасса, скиллы, git).
/// Агент хранит workspace в приватном поле, а сигнатуру `run_tui` мы не ломаем,
/// поэтому берём текущий каталог: в типичном запуске (`theseus` из корня
/// проекта или `-w .`) он совпадает с workspace агента.
fn workspace_guess() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Путь к файлу истории ввода (`~/.theseus/history`); `None`, если HOME не задан.
fn history_path() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".theseus").join("history"))
}

/// Сохранить историю на диск, создав `~/.theseus` при необходимости.
/// История — некритичные данные: ошибки записи молча игнорируем.
fn save_history(history: &InputHistory, path: Option<&Path>) {
    if let Some(p) = path {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = history.save(p);
    }
}

/// Краткий git-контекст для заголовка: «⎇ main» или «⎇ main*» (звёздочка —
/// есть незакоммиченные изменения). Пустая строка вне репозитория.
/// Вызывается редко (вход в TUI, конец задачи): spawn git стоит миллисекунды,
/// но на каждый кадр его не делаем.
fn git_status_line(workspace: &Path) -> String {
    let Some(repo) = GitRepo::discover(workspace) else { return String::new(); };
    let Some(branch) = repo.current_branch() else { return String::new(); };
    if repo.is_dirty() { format!("⎇ {branch}*") } else { format!("⎇ {branch}") }
}

/// Печать многострочного текста в лог цветом семантической роли темы.
fn push_lines(app: &mut TuiApp, text: &str, role: ThemeRole) {
    let style = role_style(&app.theme, role);
    for line in text.lines() {
        app.push(vec![Span::styled(line.to_string(), style)]);
    }
}

/// `/skills [фильтр]`: список скиллов из каталогов workspace и домашнего.
fn cmd_skills(app: &mut TuiApp, filter: &str) {
    let theme = app.theme.clone();
    let mut dirs = vec![workspace_guess().join(".theseus/skills")];
    if let Some(home) = std::env::var("HOME").ok().map(PathBuf::from) {
        dirs.push(home.join(".theseus/skills"));
    }
    let all = crate::skills::discover(&dirs);
    let shown: Vec<_> = all.iter()
        .filter(|s| filter.is_empty() || s.name.contains(filter))
        .collect();
    if shown.is_empty() {
        app.push(vec![Span::styled("скиллов нет (искал в .theseus/skills и ~/.theseus/skills)".to_string(),
            role_style(&theme, ThemeRole::Dim))]);
        return;
    }
    app.push(vec![Span::styled(format!("скиллы ({}):", shown.len()), role_style(&theme, ThemeRole::Accent))]);
    for s in shown {
        let desc: String = s.description.chars().take(80).collect();
        app.push(vec![Span::styled(format!("  {} — {desc}", s.name), role_style(&theme, ThemeRole::Dim))]);
    }
}

/// `/memory`: сводка кросс-сессионной памяти (файл ~/.theseus/memory/MEMORY.md).
fn cmd_memory(app: &mut TuiApp) {
    match std::env::var("HOME").ok().map(PathBuf::from) {
        Some(home) => {
            let mem = crate::memory::Memory::open(&home.join(".theseus"));
            let accent = role_style(&app.theme, ThemeRole::Accent);
            app.push(vec![Span::styled(
                format!("🧠 память: {} фактов (~/.theseus/memory/MEMORY.md)", mem.fact_count()),
                accent)]);
        }
        None => {
            let error = role_style(&app.theme, ThemeRole::Error);
            app.push(vec![Span::styled("память недоступна: HOME не задан".to_string(), error)]);
        }
    }
}

/// `/sessions`: список файлов сессий workspace (`.theseus/session-*`), как `--sessions` в CLI.
fn cmd_sessions(app: &mut TuiApp) {
    let theme = app.theme.clone();
    let dir = workspace_guess().join(".theseus");
    let mut files: Vec<_> = std::fs::read_dir(&dir).into_iter().flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("session-"))
        .map(|e| e.path())
        .collect();
    files.sort();
    if files.is_empty() {
        app.push(vec![Span::styled(format!("сессий нет в {}", dir.display()), role_style(&theme, ThemeRole::Dim))]);
        return;
    }
    app.push(vec![Span::styled(format!("сессии ({}):", files.len()), role_style(&theme, ThemeRole::Accent))]);
    for f in files.iter().take(10) {
        app.push(vec![Span::styled(format!("  {}", f.display()), role_style(&theme, ThemeRole::Dim))]);
    }
}

/// `/trace`: сводка по JSONL-потоку трассы (`.theseus/trace-*.jsonl`, формат
/// `JsonlTraceWriter`): сколько спанов открыто за сессию и сколько ещё не закрыто.
/// Реестр агента приватен, поэтому читаем его сериализованный вид на диске.
fn cmd_trace(app: &mut TuiApp) {
    let theme = app.theme.clone();
    let dir = workspace_guess().join(".theseus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir).into_iter().flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("trace-")))
        .collect();
    files.sort();
    let Some(latest) = files.last() else {
        app.push(vec![Span::styled(format!("трасса: файлов trace-*.jsonl нет в {}", dir.display()),
            role_style(&theme, ThemeRole::Dim))]);
        return;
    };
    let mut opened: HashSet<u64> = HashSet::new();
    let mut closed: HashSet<u64> = HashSet::new();
    if let Ok(text) = std::fs::read_to_string(latest) {
        for line in text.lines() {
            let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let Some(id) = rec["id"].as_u64() else { continue };
            match rec["event"].as_str() {
                Some("open") => { opened.insert(id); }
                Some("close" | "auto_close") => { closed.insert(id); }
                _ => {}
            }
        }
    }
    let name = latest.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    app.push(vec![Span::styled(
        format!("📈 трасса {name}: {} спанов, открытые: {}", opened.len(), opened.difference(&closed).count()),
        role_style(&theme, ThemeRole::Accent))]);
}

/// Slash-команды TUI (v0.4): разбор через реестр [`crate::slash`].
/// true = выход из приложения.
fn handle_slash(text: &str, app: &mut TuiApp, controls: &Controls, model_info: &str) -> bool {
    let theme = app.theme.clone();
    let accent = role_style(&theme, ThemeRole::Accent);
    let error = role_style(&theme, ThemeRole::Error);
    let dim = role_style(&theme, ThemeRole::Dim);
    match slash::parse(text) {
        // одинокий «/» — не команда: молча игнорируем
        Parsed::NotSlash => {}
        Parsed::Unknown { name, suggestions } => {
            // «/abort» нет в общем реестре (реестр описывает и CLI-команды),
            // а в TUI это прерывание хода — сохраняем прежнее поведение.
            if name.eq_ignore_ascii_case("abort") {
                controls.abort.store(true, std::sync::atomic::Ordering::Relaxed);
                app.push(vec![Span::styled("⏹ прерываю агента…".to_string(), error)]);
            } else {
                let hint = if suggestions.is_empty() {
                    String::new()
                } else {
                    let list = suggestions.iter().map(|s| format!("/{s}")).collect::<Vec<_>>().join(", ");
                    format!(" Похожие: {list}.")
                };
                app.push(vec![Span::styled(format!("неизвестная команда /{name} — см. /help.{hint}"), error)]);
            }
        }
        Parsed::Cmd { cmd, args } => match cmd.name {
            "help" => {
                let topic = args.split_whitespace().next().unwrap_or("");
                if topic.is_empty() {
                    push_lines(app, &slash::help_index(), ThemeRole::Accent);
                } else if let Some(found) = slash::builtin_commands().into_iter().find(|c| c.matches(topic)) {
                    push_lines(app, &slash::help_page(&found), ThemeRole::Accent);
                } else {
                    app.push(vec![Span::styled(format!("нет справки по /{topic} — см. /help"), error)]);
                }
            }
            "goal" => {
                if args.is_empty() {
                    app.push(vec![Span::styled("использование: /goal <текст цели>".to_string(), error)]);
                } else {
                    *controls.goal_slot.lock().unwrap() = Some(args.to_string());
                    app.push(vec![Span::styled(format!("🎯 цель поставлена: {args}"), accent)]);
                }
            }
            "plan" => {
                let cur = controls.plan.load(std::sync::atomic::Ordering::Relaxed);
                controls.plan.store(!cur, std::sync::atomic::Ordering::Relaxed);
                app.push(vec![Span::styled(
                    format!("📋 plan mode: {}", if !cur { "ON — агент только читает и планирует" } else { "OFF — реализация разрешена" }),
                    accent)]);
            }
            "model" => {
                app.push(vec![Span::styled(format!("модель: {model_info}"), accent)]);
            }
            "mode" => {
                use crate::permissions::{MODE_ASK, MODE_SEMI, MODE_YOLO};
                let arg = args.split_whitespace().next().unwrap_or("");
                let (code, label) = match arg {
                    "ask" => (MODE_ASK, "Совет"),
                    "semi" => (MODE_SEMI, "Авто-правки"),
                    "yolo" => (MODE_YOLO, "Автомат"),
                    _ => {
                        let cur = controls.mode_atomic.load(std::sync::atomic::Ordering::Relaxed);
                        let label = match cur {
                            MODE_SEMI => "Авто-правки",
                            MODE_YOLO => "Автомат",
                            MODE_ASK => "Совет",
                            _ => "из запуска (по флагу)",
                        };
                        app.push(vec![Span::styled(
                            format!("режим разрешений: {label}. Переключить: /mode ask (Совет) | /mode semi (Авто-правки) | /mode yolo (Автомат)"), accent)]);
                        return false;
                    }
                };
                controls.mode_atomic.store(code, std::sync::atomic::Ordering::Relaxed);
                app.push(vec![Span::styled(format!("⚡ режим разрешений → {label}"),
                    accent.add_modifier(Modifier::BOLD))]);
            }
            "theme" => {
                let arg = args.split_whitespace().next().unwrap_or("");
                if arg.is_empty() {
                    app.push(vec![Span::styled(
                        format!("тема: {} (варианты: dark, light, mono)", app.theme.name),
                        accent)]);
                } else if let Some(new_theme) = tui_theme(arg) {
                    // переключаем в рантайме: следующий кадр уже в новой палитре
                    app.theme = new_theme;
                    let ok = role_style(&app.theme, ThemeRole::Ok);
                    app.push(vec![Span::styled(format!("🎨 тема переключена: {arg}"), ok)]);
                } else {
                    app.push(vec![Span::styled(
                        format!("неизвестная тема «{arg}» — варианты: dark, light, mono"),
                        error)]);
                }
            }
            "compact" => {
                // публичного метода ручной компактификации у агента нет
                // (maybe_compact — приватный, agent/mod.rs не трогаем):
                // честно сообщаем про автоматический порог (L1→L2→L3)
                app.push(vec![Span::styled("⤓ compact: будет выполнено автоматически по порогу".to_string(),
                    accent)]);
            }
            "skills" => cmd_skills(app, args),
            "memory" => cmd_memory(app),
            "sessions" => cmd_sessions(app),
            "trace" => cmd_trace(app),
            "yolo" => {
                // переключателя режима разрешений в рантайме нет: режим
                // фиксируется в PermissionEngine при старте (флаг --yolo)
                app.push(vec![Span::styled(
                    "режим yolo задаётся флагом --yolo при запуске; в текущей сессии не переключается".to_string(),
                    accent)]);
            }
            "doctor" => {
                app.push(vec![Span::styled("диагностика запускается из CLI: theseus doctor [--fix]".to_string(),
                    accent)]);
            }
            "hooks" => {
                app.push(vec![Span::styled(
                    "хуки настраиваются в конфиге (~/.config/theseus/config.toml); проверка — theseus doctor".to_string(),
                    accent)]);
            }
            "mcp" => {
                app.push(vec![Span::styled("MCP-серверы подключаются из конфига при старте сессии".to_string(),
                    accent)]);
            }
            "peers" => {
                // статус внешних CLI-агентов (probe по PATH, ~5с на пару)
                app.push(vec![Span::styled("проверяю внешних агентов…".to_string(), dim)]);
                let probed = crate::peers::probe_peers(&crate::peers::builtin_peers());
                let ok = role_style(&theme, ThemeRole::Ok);
                for line in crate::peers::format_peers(&probed).lines() {
                    app.push(vec![Span::styled(line.to_string(),
                        if line.contains('✅') { ok } else { dim })]);
                }
            }
            "new" | "clear" => {
                // новая сессия: лог очищается сразу, историю агента и файлы
                // транскрипта ротирует run() при старте следующей задачи
                // (флаг reset_session → Agent::reset_session_state)
                app.log.clear();
                app.block_kind = None;
                app.follow = true;
                app.scroll = 0;
                // индексы в очищенный лог недействительны — сбрасываем всё,
                // что ссылается на строки/области прежней сессии
                app.stream_open = false;
                app.stream_line_idx = None;
                app.stream_block_len = 0;
                app.stream_text.clear();
                app.last_tool_open = None;
                app.sel = None;
                app.completion_cycle = None;
                app.ctx_est_tokens = None;
                controls.reset_session.store(true, std::sync::atomic::Ordering::Relaxed);
                app.push(vec![Span::styled(
                    format!("╭─ новая сессия: история очищена, транскрипт — в новые файлы (/{0}) ─╮", cmd.name),
                    role_style(&app.theme, ThemeRole::Dim))]);
            }
            "quit" => return true,
            other => {
                app.push(vec![Span::styled(format!("команда /{other} пока не поддержана в TUI — см. /help"),
                    error)]);
            }
        },
    }
    false
}

enum AState {
    Running(JoinHandle<Agent>),
    Idle(Box<Agent>),
    Empty,
}

pub fn run_tui(mut agent: Agent, broker: Arc<PermBroker>, first_prompt: Option<String>,
               controls: Controls, model_info: String) -> Result<()> {
    let (tx, rx) = channel::<AgentEvent>();
    let b2 = broker.clone();
    agent.perm_answerer = Some(Box::new(move |q: &str| b2.ask(q)));
    agent.events = Some(tx.clone());
    agent.controls = controls.clone();

    let mut state = if let Some(p) = first_prompt {
        AState::Running(std::thread::spawn(move || {
            let _ = agent.run(&p);
            agent
        }))
    } else {
        AState::Idle(Box::new(agent))
    };

    let mut stdout: Stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    enable_raw_mode()?;
    // колесо мыши — прокрутка лога (как у тройки лидеров); на выходе — Disable
    stdout.execute(EnableMouseCapture)?;
    // bracketed paste — многострочная вставка одним событием (Event::Paste)
    stdout.execute(EnableBracketedPaste)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut app = TuiApp::new();
    // метаданные старта: модель для welcome-блока, лимит контекст-бара из
    // конфига (сигнатуру run_tui не трогаем), часовой пояс префиксов времени —
    // один вызов `date` на сессию
    app.model_info = model_info.clone();
    app.ctx_limit = context_limit_from_config();
    app.tz_offset_secs = local_utc_offset_secs();

    // история ввода (~/.theseus/history) и git-контекст заголовка (v0.4)
    let hist_path = history_path();
    let mut history = hist_path.as_deref()
        .map(|p| InputHistory::load(p, DEFAULT_CAPACITY))
        .unwrap_or_else(InputHistory::with_default_capacity);
    let workspace = workspace_guess();
    app.git_status = git_status_line(&workspace);

    loop {
        while let Ok(ev) = rx.try_recv() { app.on_event(ev); }
        // спиннер «работаю…» показываем только пока агент выполняет задачу
        app.agent_running = matches!(state, AState::Running(_));
        // индикатор режима разрешений (слева в заголовке ввода)
        app.mode_code = controls.mode_atomic.load(std::sync::atomic::Ordering::Relaxed);
        let pq = broker.peek();
        terminal.draw(|f| draw(f, &mut app, pq.as_deref()))?;

        if event::poll(Duration::from_millis(100))? {
            let ev = event::read()?;
            // bracketed paste: многострочная вставка одним событием (v0.6.3) —
            // текст вставляется по курсору после санитации (CRLF→LF, табы и
            // управляющие символы не ломают кадр)
            if let Event::Paste(s) = ev {
                let clean = sanitize_paste(&s);
                input_insert(&mut app.input, &mut app.cursor, &clean);
                app.completion_cycle = None;
                continue;
            }
            // колесо мыши: прокрутка лога; при достижении дна — возврат в follow
            if let Event::Mouse(m) = ev {
                match m.kind {
                    MouseEventKind::ScrollUp => {
                        // вверх — в ручной режим с клампом: нельзя «выше» верхнего
                        // полного окна, пустого экрана под текстом не будет
                        app.follow = false;
                        app.scroll = app.scroll.min(app.max_scroll()).saturating_sub(3);
                    }
                    MouseEventKind::ScrollDown => {
                        // вниз — до нижнего полного окна, дальше — follow (низ лога)
                        let mt = app.max_scroll();
                        app.scroll = app.scroll.saturating_add(3);
                        if app.scroll >= mt { app.scroll = mt; app.follow = true; }
                    }
                    // выделение в логе: Down начинает, Drag тянет, Up копирует
                    MouseEventKind::Down(MouseButton::Left) => {
                        if point_in_rect(m.column, m.row, app.log_area) {
                            app.sel = Some(Sel { anchor: (m.column, m.row), current: (m.column, m.row) });
                            app.follow = false;
                        } else {
                            // клик вне области лога — сброс выделения
                            app.sel = None;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(sel) = &mut app.sel {
                            sel.current = (m.column, m.row);
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some(sel) = app.sel.take() {
                            let text = extract_selection(&app, sel);
                            // клик без драга даёт пустой текст — буфер не трогаем
                            if !text.is_empty() {
                                let count = text.chars().count();
                                let (msg, role) = match copy_to_clipboard(&text) {
                                    Ok(backend) => (format!("📋 скопировано {count} символов ({backend})"), ThemeRole::Ok),
                                    Err(e) => (format!("📋 {e}"), ThemeRole::Error),
                                };
                                let style = role_style(&app.theme, role);
                                app.push(vec![Span::styled(msg, style)]);
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if let Event::Key(key) = ev {
                if key.kind != KeyEventKind::Press { continue; }

                // активный попап разрешения?
                if pq.is_some() {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            if let Some((_, answer)) = broker.take() { let _ = answer.send(true); }
                        }
                        KeyCode::Char('a') | KeyCode::Char('A') => {
                            if let Some((_, answer)) = broker.take() { let _ = answer.send(true); }
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            if let Some((_, answer)) = broker.take() { let _ = answer.send(false); }
                        }
                        _ => {}
                    }
                    continue;
                }

                // любая клавиша кроме Tab сбрасывает цикл автодополнения slash-команд
                if key.code != KeyCode::Tab {
                    app.completion_cycle = None;
                }
                match key.code {
                    KeyCode::Esc => {
                        // Esc: агент работает → прервать ход; idle → выход (v0.3)
                        if matches!(state, AState::Running(_)) {
                            controls.abort.store(true, std::sync::atomic::Ordering::Relaxed);
                            let error = role_style(&app.theme, ThemeRole::Error);
                            app.push(vec![Span::styled("⏹ прерываю агента…", error)]);
                        } else {
                            break;
                        }
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Tab => {
                        // автодополнение: один кандидат — полное имя (+пробел),
                        // несколько — общий префикс, дальше — цикл по кандидатам
                        if let Some((new_input, cycle)) = slash_complete(&app.input, app.completion_cycle.clone()) {
                            app.input = new_input;
                            app.cursor = app.input.chars().count();
                            app.completion_cycle = cycle;
                        }
                    }
                    // Alt+Enter — новая строка в поле ввода (многострочный ввод
                    // v0.6.3; просто Enter — отправка, как и было)
                    KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                        input_insert(&mut app.input, &mut app.cursor, "\n");
                    }
                    KeyCode::Enter => {
                        // «\» в конце ввода + Enter — новая строка (конвенция
                        // Claude Code): бэкслеш снимается, отправки нет
                        if app.cursor == app.input.chars().count() && app.input.ends_with('\\') {
                            app.input.pop();
                            app.cursor -= 1;
                            input_insert(&mut app.input, &mut app.cursor, "\n");
                            continue;
                        }
                        let text = app.input.trim().to_string();
                        if text.is_empty() { continue; }
                        app.input.clear();
                        app.cursor = 0;
                        // история: любая отправленная строка (и промпт, и команда);
                        // push сбрасывает навигацию ↑/↓, сохраняем на диск сразу
                        history.push(&text);
                        save_history(&history, hist_path.as_deref());
                        // slash-команды через реестр crate::slash (v0.4)
                        if text.starts_with('/') {
                            if handle_slash(&text, &mut app, &controls, &model_info) { break; }
                            continue;
                        }
                        if let AState::Idle(mut a) = state {
                            app.agent_done = false;
                            // новая задача — возвращаем автопрокрутку: ручной скролл
                            // выше (PgUp/колесо/выделение мышью) выключал follow, и
                            // лог «уходил вниз» без догона (баг пользователя 20.07)
                            app.follow = true;
                            app.scroll = 0;
                            a.controls = controls.clone();
                            let b3 = broker.clone();
                            a.perm_answerer = Some(Box::new(move |q: &str| b3.ask(q)));
                            a.events = Some(tx.clone());
                            let prompt = text;
                            state = AState::Running(std::thread::spawn(move || {
                                let _ = a.run(&prompt);
                                *a
                            }));
                        } else {
                            // агент занят → Enter ставит в ОЧЕРЕДЬ (Normal): вольётся
                            // на границе хода, стрим не прерывается; срочная вставка
                            // с прерыванием — Ctrl+S (урок Codex steering/mailbox)
                            controls.prompt_slot.lock().unwrap().push(crate::scheduler::QueuedPrompt::new(
                                text.clone(), crate::scheduler::Priority::Normal,
                                crate::scheduler::PromptSource::User));
                            let user = role_style(&app.theme, ThemeRole::UserText);
                            app.push(vec![Span::styled(format!("📨 в очередь: {text}"), user),
                                Span::styled("  (приму после текущего хода · Ctrl+S — вставить сразу)".to_string(),
                                    role_style(&app.theme, ThemeRole::Dim))]);
                        }
                    }
                    // Ctrl+N — новая строка в поле ввода (надёжно во всех терминалах;
                    // Alt+Enter — тоже поддерживается, но не везде различим)
                    KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input_insert(&mut app.input, &mut app.cursor, "\n");
                    }
                    // Ctrl+S — срочная вставка с преемпцией стрима (Immediate);
                    // когда агент свободен — эквивалент Enter
                    KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        let text = app.input.trim().to_string();
                        if text.is_empty() { continue; }
                        app.input.clear();
                        app.cursor = 0;
                        history.push(&text);
                        save_history(&history, hist_path.as_deref());
                        if text.starts_with('/') {
                            if handle_slash(&text, &mut app, &controls, &model_info) { break; }
                            continue;
                        }
                        if let AState::Idle(mut a) = state {
                            // агент свободен — Ctrl+S = Enter (та же ветка запуска)
                            app.agent_done = false;
                            // новая задача — возвращаем автопрокрутку: ручной скролл
                            // выше (PgUp/колесо/выделение мышью) выключал follow, и
                            // лог «уходил вниз» без догона (баг пользователя 20.07)
                            app.follow = true;
                            app.scroll = 0;
                            a.controls = controls.clone();
                            let b3 = broker.clone();
                            a.perm_answerer = Some(Box::new(move |q: &str| b3.ask(q)));
                            a.events = Some(tx.clone());
                            let prompt = text;
                            state = AState::Running(std::thread::spawn(move || {
                                let _ = a.run(&prompt);
                                *a
                            }));
                        } else {
                            // Immediate: стрим прервётся, частичный ответ сохранится
                            controls.prompt_slot.lock().unwrap().push(crate::scheduler::QueuedPrompt::new(
                                text.clone(), crate::scheduler::Priority::Immediate,
                                crate::scheduler::PromptSource::User));
                            let user = role_style(&app.theme, ThemeRole::UserText);
                            app.push(vec![Span::styled(format!("⚡ вставляю посреди хода: {text}"), user),
                                Span::styled("  (стрим прервётся, частичный ответ сохранится)".to_string(),
                                    role_style(&app.theme, ThemeRole::Dim))]);
                        }
                    }
                    KeyCode::Backspace => { input_backspace(&mut app.input, &mut app.cursor); }
                    // ← / → — навигация курсором по тексту (многострочный ввод)
                    KeyCode::Left => { app.cursor = app.cursor.saturating_sub(1); }
                    KeyCode::Right => {
                        app.cursor = (app.cursor + 1).min(app.input.chars().count());
                    }
                    KeyCode::Up => {
                        // листание истории «назад»: текущий ввод уходит в черновик
                        if let Some(entry) = history.prev(&app.input) {
                            app.input = entry.to_string();
                            app.cursor = app.input.chars().count();
                        }
                    }
                    KeyCode::Down => {
                        // «вперёд»; за самой новой записью восстанавливается черновик
                        if let Some(entry) = history.next() {
                            app.input = entry.to_string();
                            app.cursor = app.input.chars().count();
                        }
                    }
                    KeyCode::Char(c) => {
                        let mut b = [0u8; 4];
                        input_insert(&mut app.input, &mut app.cursor, c.encode_utf8(&mut b));
                    }
                    KeyCode::PageUp => {
                        app.follow = false;
                        app.scroll = app.scroll.min(app.max_scroll()).saturating_sub(10);
                    }
                    KeyCode::PageDown => {
                        let mt = app.max_scroll();
                        app.scroll = app.scroll.saturating_add(10);
                        if app.scroll >= mt { app.scroll = mt; app.follow = true; }
                    }
                    KeyCode::End => { app.follow = true; }
                    _ => {}
                }
            }
        }

        let finished = matches!(&state, AState::Running(h) if h.is_finished());
        if finished {
            let old = std::mem::replace(&mut state, AState::Empty);
            if let AState::Running(h) = old {
                match h.join() {
                    Ok(mut a) => {
                        // авто-цепочка очереди (v0.6.1): накопленные по Enter вставки,
                        // не влившиеся на границе хода, запускаются следующей задачей
                        // без участия пользователя — «поставить в очередь» работает
                        // до конца, а не только до ближайшего финиша
                        let queued = controls.prompt_slot.lock().unwrap().drain();
                        if queued.is_empty() {
                            state = AState::Idle(Box::new(a));
                            app.agent_done = true;
                        } else {
                            let joined = queued.iter()
                                .map(|p| p.text.clone()).collect::<Vec<_>>().join("\n");
                            let preview: String = joined.chars().take(80).collect();
                            app.push(vec![Span::styled(
                                format!("📨 беру из очереди ({}): {preview}", queued.len()),
                                role_style(&app.theme, ThemeRole::Accent))]);
                            app.agent_done = false;
                            // новая задача — возвращаем автопрокрутку: ручной скролл
                            // выше (PgUp/колесо/выделение мышью) выключал follow, и
                            // лог «уходил вниз» без догона (баг пользователя 20.07)
                            app.follow = true;
                            app.scroll = 0;
                            a.controls = controls.clone();
                            let b4 = broker.clone();
                            a.perm_answerer = Some(Box::new(move |q: &str| b4.ask(q)));
                            a.events = Some(tx.clone());
                            let prompt = joined;
                            state = AState::Running(std::thread::spawn(move || {
                                let _ = a.run(&prompt);
                                a
                            }));
                        }
                    }
                    Err(_) => {
                        let error = role_style(&app.theme, ThemeRole::Error);
                        app.push(vec![Span::styled("✖ поток агента паниковал", error)]);
                        break;
                    }
                }
                // задача завершена — рабочее дерево могло измениться: обновляем
                // git-контекст заголовка (не на каждый кадр: spawn git стоит мс)
                app.git_status = git_status_line(&workspace);
            }
        }
    }

    // выход из TUI: история на диск (каталог ~/.theseus создастся при save)
    save_history(&history, hist_path.as_deref());
    terminal.backend_mut().execute(DisableBracketedPaste)?;
    terminal.backend_mut().execute(DisableMouseCapture)?;
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    Ok(())
}

#[cfg(test)]
mod md_render_tests {
    use super::*;

    /// Текст без разметки проходит без потерь и без стилей.
    #[test]
    fn plain_text_passthrough() {
        let lines = md_ansi_to_lines("просто текст\nвторая строка");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0][0].content.as_ref(), "просто текст");
        assert_eq!(lines[0][0].style, Style::default());
    }

    /// Заголовок markdown: bold + акцентный цвет сохраняются одновременно.
    #[test]
    fn header_keeps_bold_and_accent() {
        let rendered = crate::markdown::render("# Заголовок", 80);
        let lines = md_ansi_to_lines(&rendered);
        let style = lines[0][0].style;
        assert!(style.add_modifier.contains(Modifier::BOLD), "{style:?}");
        assert_eq!(style.fg, Some(Color::LightMagenta));
        assert!(lines[0][0].content.contains("Заголовок"));
    }

    /// Инлайн-код: циан; после reset стиль снимается.
    #[test]
    fn inline_code_cyan_then_reset() {
        let rendered = crate::markdown::render("текст `код` дальше", 80);
        let lines = md_ansi_to_lines(&rendered);
        let code_span = lines[0].iter().find(|s| s.content.contains("код")).unwrap();
        assert_eq!(code_span.style.fg, Some(Color::Cyan));
        let last = lines[0].last().unwrap();
        if last.content.contains("дальше") {
            assert_eq!(last.style.fg, None, "после reset цвет обязан сняться");
        }
    }

    /// Код-фенс: нейтральная фоновая полоса 256-палитры — мягкий светло-серый
    /// текст (gray 248) на тёмно-сером поле (gray 238); видна на тёмном фоне,
    /// не режет глаза (замечание пользователя 20.07).
    #[test]
    fn code_fence_background() {
        let rendered = crate::markdown::render("```rust\nfn main() {}\n```", 80);
        let lines = md_ansi_to_lines(&rendered);
        let has_bg = lines.iter().flatten().any(|s| s.style.bg == Some(Color::Indexed(238)));
        assert!(has_bg, "ожидалась фоновая полоса gray-238: {lines:?}");
        let has_fg = lines.iter().flatten().any(|s| s.style.fg == Some(Color::Indexed(248)));
        assert!(has_fg, "ожидался текст gray-248 на полосе: {lines:?}");
        // и вся цепочка до буфера кадра: bg доходит до ячеек терминала
        let mut app = TuiApp::new();
        app.on_event(AgentEvent::AgentText("```rust\nfn main() {}\n```".into()));
        let backend = ratatui::backend::TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let buf = terminal.backend().buffer();
        let area = buf.area;
        let mut found_bg = false;
        for y in 0..area.height {
            for x in 0..area.width {
                if buf.get(x, y).bg == Color::Indexed(238) {
                    found_bg = true;
                }
            }
        }
        assert!(found_bg, "bg gray-238 не дошёл до буфера кадра");
        // и через живой путь стрима: дельты с незакрытым фенсом → финал с
        // закрытым — фон фенса обязан появиться в логе после финала
        let mut app = TuiApp::new();
        let final_text = "## Пример\n\nЗдесь идёт обычный параграф текста.\n\n```python\ndef greet(name):\n    \"\"\"Приветствует пользователя.\"\"\"\n    return message\n```\n";
        app.on_event(AgentEvent::AgentTextDelta("## Пример\n\n```python\ndef greet(name):\n".into()));
        app.on_event(AgentEvent::AgentText(final_text.into()));
        let has_bg_log = app.log.iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.bg == Some(Color::Indexed(238)));
        assert!(has_bg_log, "после стрима фон фенса потерян: {:?}", app.log.iter()
            .map(|l| l.spans.iter().map(|s| (s.content.clone(), s.style.bg))
                .collect::<Vec<_>>()).collect::<Vec<_>>());
    }

    /// Список: маркер заменён на •, текст не потерян.
    #[test]
    fn list_bullet_marker() {
        let rendered = crate::markdown::render("- пункт один\n- пункт два", 80);
        let lines = md_ansi_to_lines(&rendered);
        assert_eq!(lines.len(), 2);
        let joined: String = lines.iter().flatten().map(|s| s.content.as_ref()).collect();
        assert!(joined.contains('•'), "{joined}");
        assert!(joined.contains("пункт два"), "{joined}");
    }

    /// Ни один спан не содержит сырых ESC-последовательностей.
    #[test]
    fn no_raw_escapes_leak() {
        let rendered = crate::markdown::render("# H\n**жирный** и `код`\n> цитата\n---\n[т](http://u)", 80);
        let lines = md_ansi_to_lines(&rendered);
        for span in lines.iter().flatten() {
            assert!(!span.content.contains('\u{1b}'), "ESC в спане: {:?}", span.content);
        }
    }

    /// Регрессия (живая сессия): стрим-строка заменяется рендером даже когда
    /// между дельтами и AgentText пришли промежуточные Status/Reasoning —
    /// раньше флаг сбрасывался, и фразы показывались дважды.
    #[test]
    fn stream_line_replaced_despite_intermediate_events() {
        let mut app = TuiApp::new();
        app.on_event(AgentEvent::AgentTextDelta("Привет, ".into()));
        app.on_event(AgentEvent::AgentTextDelta("мир".into()));
        app.on_event(AgentEvent::Status { turns: 1, est_tokens: 100, mode: "Ask".into() });
        app.on_event(AgentEvent::Reasoning(42));
        let before = app.log.len();
        app.on_event(AgentEvent::AgentText("Привет, мир!".into()));
        // сырая стрим-строка заменена: строк столько же или больше, но текста «Привет, »
        // в сыром виде (без рендера) быть не должно
        assert!(app.log.len() >= before, "строки: {}", app.log.len());
        let raw_count = app.log.iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.content.contains("Привет, "))
            .count();
        assert_eq!(raw_count, 1, "фраза не должна дублироваться: {:?}", app.log.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()).collect::<Vec<_>>())
            .collect::<Vec<_>>());
    }
}

// ---------------------------------------------------------------------------
// Тесты чистых UI-хелперов (темы, контекст-бар, completion, время, ввод)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod ui_helpers_tests {
    use super::*;

    /// Контекст-бар: ячейки заполняются пропорционально, процент честный.
    #[test]
    fn context_bar_rendering() {
        assert_eq!(context_bar_text(0, 120_000), "░░░░░░░░░░ 0%");
        assert_eq!(context_bar_text(60_000, 120_000), "█████░░░░░ 50%");
        assert_eq!(context_bar_text(120_000, 120_000), "██████████ 100%");
        // переполнение: ячеек не больше десяти, процент не обрезается
        assert_eq!(context_bar_text(180_000, 120_000), "██████████ 150%");
        // нулевой лимит — безопасный ноль, без деления на ноль
        assert_eq!(context_bar_text(100, 0), "░░░░░░░░░░ 0%");
    }

    /// Пороги цвета контекст-бара: <60% Ok, <85% Warn, ≥85% Error.
    #[test]
    fn context_bar_color_thresholds() {
        assert_eq!(context_bar_role(0), ThemeRole::Ok);
        assert_eq!(context_bar_role(59), ThemeRole::Ok);
        assert_eq!(context_bar_role(60), ThemeRole::Warn);
        assert_eq!(context_bar_role(84), ThemeRole::Warn);
        assert_eq!(context_bar_role(85), ThemeRole::Error);
        assert_eq!(context_bar_role(150), ThemeRole::Error);
    }

    /// Slash-completion: «/» + непустой префикс без пробелов, регистронезависимо.
    #[test]
    fn slash_completion_filter() {
        // префикс имени
        let names: Vec<&str> = slash_completions("/th").iter().map(|c| c.name).collect();
        assert_eq!(names, ["theme"]);
        // регистронезависимо
        assert!(slash_completions("/TH").iter().any(|c| c.name == "theme"));
        // префикс алиаса тоже находит команду (/heal → алиас health → doctor)
        assert!(slash_completions("/heal").iter().any(|c| c.name == "doctor"));
        // голый «/» — ВЕСЬ список команд (осмотр доступного), без обрезки по MAX
        let bare = slash_completions("/");
        assert_eq!(bare.len(), slash::builtin_commands().len(),
            "голый «/» обязан показать все команды");
        assert!(bare.iter().any(|c| c.name == "new") && bare.iter().any(|c| c.name == "clear"));
        // пробел в команде, ввод без слеша, неизвестный префикс — панели нет
        assert!(slash_completions("/help тема").is_empty());
        assert!(slash_completions("просто текст").is_empty());
        assert!(slash_completions("/zzz").is_empty());
        // не больше MAX_COMPLETIONS, и все команды — по префиксу имени или алиаса
        let all = slash_completions("/m");
        assert!(all.len() <= MAX_COMPLETIONS);
        assert!(all.iter().all(|c| c.name.starts_with('m')
            || c.aliases.iter().any(|a| a.starts_with('m'))));
    }

    /// Формат времени HH:MM: полночь, минуты, пояс +03:00, заворот назад.
    #[test]
    fn time_format_hhmm() {
        assert_eq!(fmt_hhmm(0, 0), "00:00");
        assert_eq!(fmt_hhmm(3661, 0), "01:01");
        assert_eq!(fmt_hhmm(0, 10_800), "03:00"); // UTC+3
        assert_eq!(fmt_hhmm(0, -3600), "23:00"); // UTC-1: полночь UTC — это вчера 23:00
        assert_eq!(fmt_hhmm(86_399, 0), "23:59");
    }

    /// Разбор смещения `date +%z` в секунды.
    #[test]
    fn tz_offset_parsing() {
        assert_eq!(parse_tz_offset("+0300"), Some(10_800));
        assert_eq!(parse_tz_offset("-0530"), Some(-19_800));
        assert_eq!(parse_tz_offset("+0000"), Some(0));
        assert_eq!(parse_tz_offset(""), None);
        assert_eq!(parse_tz_offset("0300"), None);
        assert_eq!(parse_tz_offset("+03"), None);
        assert_eq!(parse_tz_offset("+0a00"), None);
    }

    /// Тема → цвет роли: dark-палитра по спецификации, mono — без цвета.
    #[test]
    fn theme_role_colors() {
        let dark = tui_theme("dark").expect("dark-тема обязана быть");
        assert_eq!(role_color(&dark, ThemeRole::Accent), Color::LightMagenta);
        assert_eq!(role_color(&dark, ThemeRole::Dim), Color::DarkGray);
        assert_eq!(role_color(&dark, ThemeRole::Error), Color::Red);
        assert_eq!(role_color(&dark, ThemeRole::Warn), Color::Yellow);
        assert_eq!(role_color(&dark, ThemeRole::Ok), Color::Green);
        assert_eq!(role_color(&dark, ThemeRole::UserText), Color::Green);
        assert_eq!(role_color(&dark, ThemeRole::AgentText), Color::White);
        assert_eq!(role_color(&dark, ThemeRole::ToolName), Color::Yellow);
        assert_eq!(role_color(&dark, ThemeRole::StatusBar), Color::Cyan);

        let mono = tui_theme("mono").expect("mono-тема обязана быть");
        for role in ThemeRole::ALL {
            assert_eq!(role_color(&mono, role), Color::Reset, "роль {}", role.as_str());
        }
        // mono: приглушённый текст различим атрибутом DIM
        assert!(role_style(&mono, ThemeRole::Dim).add_modifier.contains(Modifier::DIM));

        assert!(tui_theme("light").is_some());
        assert!(tui_theme("DARK").is_some(), "имя темы регистронезависимо");
        assert!(tui_theme("bogus").is_none());
    }

    /// Плейсхолдер показывается только при пустом вводе.
    #[test]
    fn placeholder_only_for_empty_input() {
        assert_eq!(input_placeholder(""), Some("задача или /команда…"));
        assert_eq!(input_placeholder("/"), None);
        assert_eq!(input_placeholder("текст"), None);
    }

    /// Разбор context_limit_tokens из TOML-текста конфига.
    #[test]
    fn context_limit_toml_parsing() {
        assert_eq!(parse_context_limit("model = \"m\"\ncontext_limit_tokens = 131072\n"), Some(131_072));
        assert_eq!(parse_context_limit("model = \"m\"\n"), None);
        assert_eq!(parse_context_limit("context_limit_tokens = \"строка\"\n"), None);
        assert_eq!(parse_context_limit("это не = toml = синтаксис ==="), None);
        assert_eq!(parse_context_limit("context_limit_tokens = -5\n"), None);
    }

    /// Кадры спиннера: начало цикла и заворот по кругу.
    #[test]
    fn spinner_cycles_frames() {
        assert_eq!(spinner_frame(0), '⠋');
        assert_eq!(spinner_frame(1), '⠙');
        assert_eq!(spinner_frame(10), '⠋', "цикл из 10 кадров");
        assert_eq!(spinner_frame(23), SPINNER_FRAMES[3]);
    }

    /// Welcome-блок: заголовок, модель@url, подсказки и стартовые промпты.
    #[test]
    fn welcome_block_contents() {
        let theme = tui_theme("dark").expect("dark-тема обязана быть");
        let lines = welcome_lines("deepseek-chat @ https://api.deepseek.com", &theme);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("T H E S E U S"), "нет заголовка:\n{text}");
        assert!(text.contains("deepseek-chat @ https://api.deepseek.com"), "нет модели:\n{text}");
        assert!(text.contains(crate::onboarding::suggested_starter_prompts()[0]),
            "нет первого стартового промпта:\n{text}");
        assert!(text.contains("/help"), "нет подсказки клавиш:\n{text}");
    }

    /// Дизайн блоков: время один раз на первой строке блока, между блоками
    /// разных типов — пустая строка-разделитель, у продолжений времени нет.
    #[test]
    fn blocks_have_single_timestamp_and_spacers() {
        let mut app = TuiApp::new();
        app.on_event(AgentEvent::UserMsg("привет".into()));
        app.on_event(AgentEvent::AgentTextDelta("фрагмент".into()));
        // UserMsg (1) + разделитель (1) + стрим-строка (1) = 3
        assert_eq!(app.log.len(), 3, "строк: {}", app.log.len());
        // первая строка блока несёт время «HH:MM »
        let first = &app.log[0].spans[0].content;
        let chars: Vec<char> = first.chars().collect();
        assert_eq!(chars.len(), 6, "префикс «HH:MM »: {first:?}");
        assert_eq!(chars[2], ':');
        // вторая строка — пустой разделитель
        assert!(app.log[1].spans.is_empty());
        // продолжение агентского блока — желобок без времени
        app.on_event(AgentEvent::AgentText("строка1\nстрока2".into()));
        let last = &app.log[app.log.len() - 1].spans[0].content;
        assert!(last.contains('│'), "продолжение — желобок: {last:?}");
        assert!(last.chars().nth(2) != Some(':'), "времени на продолжении нет: {last:?}");
    }

    /// Автодополнение: единственный кандидат — полное имя с пробелом.
    #[test]
    fn complete_single_candidate_full_name_with_space() {
        // «quit» единственный кандидат по префиксу «qui»
        let (out, cycle) = slash_complete("/qui", None).unwrap();
        assert_eq!(out, "/quit ");
        assert_eq!(cycle, None);
    }

    /// Автодополнение: несколько кандидатов — общий префикс имён.
    #[test]
    fn complete_multiple_common_prefix() {
        // «s» → sessions/skills/skill_search: общий префикс «s» — не длиннее ввода,
        // поэтому сразу цикл с первого кандидата (сортировка по score: префикс имени)
        let (out, cycle) = slash_complete("/s", None).unwrap();
        assert!(out.starts_with("/s"), "{out}");
        assert!(cycle.is_some(), "ожидался цикл: {out}");
        // «the» → единственный theme
        let (out2, _) = slash_complete("/the", None).unwrap();
        assert_eq!(out2, "/theme ");
    }

    /// Цикл: повторные Tab листают кандидатов по кругу и возвращаются к первому.
    #[test]
    fn complete_cycles_candidates() {
        // по префиксу «s» два кандидата (skills, sessions) — цикл периода 2
        let (first, c1) = slash_complete("/s", None).unwrap();
        let (second, c2) = slash_complete(&first, c1).unwrap();
        let (third, _) = slash_complete(&second, c2).unwrap();
        assert_ne!(first, second, "цикл обязан менять кандидата: {first} == {second}");
        assert_eq!(third, first, "по кругу обязаны вернуться к первому: {third} != {first}");
    }

    /// Алиас тоже дополняется (например «h» → help; «q» → quit через алиас q).
    #[test]
    fn complete_via_alias() {
        let (out, _) = slash_complete("/q", None).unwrap();
        assert_eq!(out, "/quit ");
    }

    /// Не дополняется: ввод с пробелом (аргументы), без слеша, корень «/».
    #[test]
    fn complete_rejects_non_command_input() {
        assert!(slash_complete("/help model", None).is_none());
        assert!(slash_complete("обычный текст", None).is_none());
        assert!(slash_complete("/", None).is_none());
        assert!(slash_complete("/zzz-несуществующая", None).is_none());
    }
}

#[cfg(test)]
mod render_bug_tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Дамп буфера TestBackend в строки (только текст ячеек).
    fn buffer_lines(term: &Terminal<TestBackend>) -> Vec<String> {
        let buf = term.backend().buffer();
        let area = buf.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.get(x, y).symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// Регрессия (скриншот 12-00-24): ToolResult.preview со встроенными \n
    /// (read_file многострочного файла) не должен рисовать «хвосты» текста
    /// у правого края экрана на пустых строках.
    #[test]
    fn tool_result_with_embedded_newlines_has_no_stray_tail() {
        let mut app = TuiApp::new();
        app.on_event(AgentEvent::UserMsg("глянь конфиг".into()));
        // ровно как в сессии: preview многострочного конфига с \n и табами
        let preview = "     1\t# theseus config\n     2\tmodel = \"deepseek-v4-pro\"\n     3\tweb_allowed_domains = [\"duckduckgo.com\", \"api.duckduckgo.com\", \"wikipedia.org\", \"ru.wiki";
        app.on_event(AgentEvent::ToolResult { name: "read_file".into(), preview: preview.into(), ok: true });

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let lines = buffer_lines(&terminal);
        for (i, l) in lines.iter().enumerate() {
            println!("{i:2} |{l}|");
        }
        let joined = lines.join("\n");
        // переносы показаны явно маркером ⏎ (а не слипшимися строками)
        assert!(joined.contains('⏎'), "нет маркера ⏎:\n{joined}");
        // и главное: НЕТ слипшегося инлайна «…config     2	model…» —
        // именно он давал «висячие хвосты» при переносе
        assert!(!joined.contains("config     2"), "строки слиплись без разделителя:\n{joined}");
    }

    /// Регрессия (скриншот 16-42-57): preview bash-вывода pdftotext содержит
    /// form feed \x0c (разрыв страницы PDF). xterm исполняет \x0c как перевод
    /// строки БЕЗ возврата каретки: курсор уплывает, физический экран и
    /// front-буфер ratatui расходятся навсегда — «conditioned on its decision»
    /// размножен по правому краю на десятке строк. Ни один управляющий символ
    /// не должен дойти до терминала ни по одному пути записи в лог.
    #[test]
    fn control_chars_never_reach_log_or_buffer() {
        let mut app = TuiApp::new();
        // путь 1: ToolCall→ToolResult (прямой допис спана в строку вызова);
        // preview — хвост реального из events-1784467287.jsonl:310, \x0c внутри окна take(90)
        app.on_event(AgentEvent::ToolCall {
            name: "bash".into(), args: r#"{"command":"pdftotext paper.pdf -"}"#.into(),
            decision: "Allow".into(),
        });
        let preview = "annotation di\u{c}6\n\nW. Gao et al.\n\nconditioned on its decision context x and triggered module k";
        app.on_event(AgentEvent::ToolResult { name: "bash".into(), preview: preview.into(), ok: true });
        // путь 2: UserMsg через push()
        app.on_event(AgentEvent::UserMsg("строка с \u{c} и \u{8} управляющими".into()));
        // путь 3: стрим-дельта с управляющим символом (допис в last_mut, мимо push)
        app.on_event(AgentEvent::AgentTextDelta("начало \u{b}хвост".into()));

        // в логе не осталось ни одного управляющего символа
        for (i, line) in app.log.iter().enumerate() {
            for sp in &line.spans {
                assert!(!sp.content.chars().any(char::is_control),
                    "строка {i} содержит управляющий символ: {:?}", sp.content);
            }
        }
        // и в отрисованном буфере их нет — терминал не получит курсорных команд
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let joined = buffer_lines(&terminal).join("\n");
        assert!(!joined.chars().any(|c| c.is_control() && c != '\n'),
            "в буфере терминала управляющий символ:\n{joined:?}");
        // содержимое не потеряно: \x0c стал пробелом, \n — маркером ⏎
        let text: String = app.log.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("di 6"), "\\x0c должен стать пробелом: {text}");
        assert!(text.contains('⏎'), "переносы показаны маркером: {text}");
        // \x0b не дошёл (стал пробелом); markdown нормализует пробелы в один
        assert!(text.contains("начало хвост"), "\\x0b стал пробелом: {text}");
    }

    /// Голый «/» (v0.6.0, запрос пользователя): в панели — весь список команд,
    /// чтобы пользователь мог осмотреться и выбрать; на низком терминале —
    /// обрезка по высоте со строкой «…ещё N» вместо съедания экрана.
    #[test]
    fn bare_slash_lists_all_commands() {
        let mut app = TuiApp::new();
        app.input = "/".into();
        // обычный терминал: все builtin-команды видны целиком, без обрезки
        let backend = TestBackend::new(100, 45);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let joined = buffer_lines(&terminal).join("\n");
        for cmd in slash::builtin_commands() {
            assert!(joined.contains(&format!("/{:<10}", cmd.name)),
                "нет команды /{} в панели:\n{joined}", cmd.name);
        }
        assert!(!joined.contains("…ещё"), "на 45 строках обрезки быть не должно:\n{joined}");
        // низкий терминал: панель обрезана, хвост — «…ещё N — продолжайте ввод»
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let joined = buffer_lines(&terminal).join("\n");
        assert!(joined.contains("…ещё"), "на 18 строках должна быть обрезка:\n{joined}");
    }

    /// Два режима вставки посреди хода (v0.6.1, запрос пользователя): пока агент
    /// работает, заголовок ввода подсказывает оба пути — Enter в очередь и
    /// Ctrl+S вставить сразу, а не только «Esc — прервать».
    #[test]
    fn running_hint_shows_queue_and_preempt_keys() {
        let mut app = TuiApp::new();
        app.agent_running = true;
        let backend = TestBackend::new(140, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let joined = buffer_lines(&terminal).join("\n");
        assert!(joined.contains("Enter — в очередь"), "нет подсказки очереди:\n{joined}");
        assert!(joined.contains("Ctrl+S — вставить сразу"), "нет подсказки преемпции:\n{joined}");
        assert!(joined.contains("Esc — прервать"), "Esc-подсказка не должна пропасть:\n{joined}");
    }

    /// Автопрокрутка (замечание пользователя «сообщения уходят вниз, приходится
    /// скроллить мышью»): follow-режим обязан держать НИЗ лога видимым, даже
    /// когда последние сообщения — длинные обёрнутые строки. Раньше брались
    /// последние `visible` ЛОГИЧЕСКИХ строк, и хвост уходил за нижний край.
    #[test]
    fn follow_pins_bottom_with_wrapped_lines() {
        let mut app = TuiApp::new();
        for i in 1..=6 {
            app.push(vec![Span::raw(format!("строка {i}"))]);
        }
        // длинная строка: при ширине 60 займёт ~6 визуальных строк
        app.push(vec![Span::raw("длинная ".repeat(40))]);
        app.push(vec![Span::raw("ПОСЛЕДНЯЯ_СТРОКА_МАРКЕР".to_string())]);
        app.follow = true;
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let joined = buffer_lines(&terminal).join("\n");
        assert!(joined.contains("ПОСЛЕДНЯЯ_СТРОКА_МАРКЕР"),
            "низ лога не виден — автопрокрутка не работает:\n{joined}");
        assert!(!joined.contains("строка 1"),
            "верх должен был уйти за край:\n{joined}");
    }

    /// (строка, колонка) курсора по символьному индексу — вспомогательная
    /// для тестов редактирования ввода.
    fn cursor_line_col(input: &str, cursor: usize) -> (usize, usize) {
        let mut line = 0;
        let mut col = 0;
        for (i, c) in input.chars().enumerate() {
            if i == cursor {
                break;
            }
            if c == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    /// Многострочный ввод (v0.6.3): редактирование по курсору — вставка и
    /// backspace в произвольной позиции, (строка, колонка) от символьного индекса.
    #[test]
    fn input_editing_at_cursor() {
        let mut s = String::from("привет");
        let mut c = 3; // после «при»
        input_insert(&mut s, &mut c, "X");
        assert_eq!(s, "приXвет");
        assert_eq!(c, 4);
        input_backspace(&mut s, &mut c);
        assert_eq!(s, "привет");
        assert_eq!(c, 3);
        input_insert(&mut s, &mut c, "\nновая ");
        assert_eq!(s, "при\nновая вет");
        assert_eq!(cursor_line_col(&s, c), (1, 6));
        // backspace через перенос строки — поле «сжимается»
        input_backspace(&mut s, &mut c);
        input_backspace(&mut s, &mut c);
        input_backspace(&mut s, &mut c);
        input_backspace(&mut s, &mut c);
        input_backspace(&mut s, &mut c);
        input_backspace(&mut s, &mut c);
        input_backspace(&mut s, &mut c);
        assert_eq!(s, "привет");
        assert_eq!(cursor_line_col(&s, c), (0, 3));
        // навигация в нуле и за концом — безопасна
        let mut c0 = 0;
        input_backspace(&mut s, &mut c0);
        assert_eq!(c0, 0);
        // (строка, колонка) по всем позициям
        assert_eq!(cursor_line_col("", 0), (0, 0));
        assert_eq!(cursor_line_col("ab\ncd", 0), (0, 0));
        assert_eq!(cursor_line_col("ab\ncd", 2), (0, 2));
        assert_eq!(cursor_line_col("ab\ncd", 3), (1, 0));
        assert_eq!(cursor_line_col("ab\ncd", 6), (1, 2));
    }

    /// Санитация вставки: CRLF/CR → LF, таб → 4 пробела, управляющие (кроме \n) —
    /// удаляются: вставленный текст не должен ломать кадр терминала.
    #[test]
    fn paste_sanitize_keeps_newlines_drops_controls() {
        assert_eq!(sanitize_paste("a\r\nb\tc\u{c}d\u{8}e"), "a\nb    cde");
        assert_eq!(sanitize_paste("одна\rдве"), "одна\nдве");
        assert_eq!(sanitize_paste("чистый текст"), "чистый текст");
    }

    /// Поле ввода: одна строка по умолчанию; растёт по переносам; при строках
    /// больше MAX_INPUT_LINES видимое окно держит курсор (хвост виден, начало нет).
    #[test]
    fn multiline_input_grows_and_windows_to_cursor() {
        let mut app = TuiApp::new();
        app.input = "первая\nвторая\nтретья".into();
        app.cursor = app.input.chars().count();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let joined = buffer_lines(&terminal).join("\n");
        for line in ["первая", "вторая", "третья"] {
            assert!(joined.contains(line), "нет строки «{line}»:\n{joined}");
        }
        // 10 строк: окно — последние MAX_INPUT_LINES (3..=10), начало скрыто
        app.input = (1..=10).map(|i| format!("строка{i}")).collect::<Vec<_>>().join("\n");
        app.cursor = app.input.chars().count();
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let joined = buffer_lines(&terminal).join("\n");
        assert!(joined.contains("строка10"), "хвост виден:\n{joined}");
        assert!(joined.contains("строка3"), "окно от 3-й:\n{joined}");
        assert!(!joined.contains("строка2"), "начало скрыто:\n{joined}");
    }

    /// Враппинг при ручном вводе (замечание пользователя): длинная строка БЕЗ
    /// переносов \n тоже растит поле — текст переносится визуально, а не
    /// клиппится в одну строку справа; курсор — на визуальной позиции конца.
    #[test]
    fn typed_long_line_wraps_and_grows_input() {
        let mut app = TuiApp::new();
        // 100 символов при ширине поля ~58 — минимум две визуальные строки
        app.input = "а".repeat(100);
        app.cursor = app.input.chars().count();
        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let lines = buffer_lines(&terminal);
        // первая визуальная строка — сплошные «а» во всю ширину поля
        let rows_with_a = (0..lines.len())
            .filter(|&y| lines[y].contains("аааааааааа")).count();
        assert!(rows_with_a >= 2,
            "строка не перенеслась визуально (ожидалось >= 2 строк с «а»):\n{}",
            lines.join("\n"));
    }

    /// Курсор после хвостового пробела (баг «пробел не нажимается» 20.07):
    /// пробел — тоже колонка; сентинель «█» в wrapped_cursor_pos не даёт
    /// позиции курсора «залипнуть» на последнем непробельном символе.
    #[test]
    fn cursor_pos_accounts_trailing_space() {
        let pos = |s: &str, w: u16| {
            let probe = format!("{s}█");
            let lines: Vec<Line> = probe.split('\n').map(Line::from).collect();
            wrapped_cursor_pos(lines, w, 50)
        };
        assert_eq!(pos("ab ", 10), (0, 3), "пробел занимает колонку");
        assert_eq!(pos("ab", 10), (0, 2));
        assert_eq!(pos("", 10), (0, 0), "пустой ввод — курсор в начале");
        assert_eq!(pos("abc", 2), (1, 1), "враппинг: курсор на второй строке");
        assert_eq!(pos("ab\nc", 10), (1, 1), "после переноса — вторая строка");
        assert_eq!(pos("ab\n", 10), (1, 0), "после \\n — начало новой строки");
    }

    /// Markdown налету (v0.6.4, запрос пользователя): разметка рендерится
    /// во время стрима, а не только по AgentText — сырые маркеры не висят
    /// в логе, а после закрывающего маркера текст «устаканивается» в стиль.
    #[test]
    fn stream_renders_markdown_on_the_fly() {
        let mut app = TuiApp::new();
        app.on_event(AgentEvent::UserMsg("расскажи".into()));
        // заголовок приходит одной дельтой — сразу рендерится без «##»
        app.on_event(AgentEvent::AgentTextDelta("## Тайтл\n".into()));
        let text: String = app.log.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("Тайтл"), "заголовок виден: {text}");
        assert!(!text.contains("## Тайтл"), "сырые маркеры не висят: {text}");
        // маркер, разорванный между дельтами, закрывается — текст устаканивается
        app.on_event(AgentEvent::AgentTextDelta("текст **жир".into()));
        app.on_event(AgentEvent::AgentTextDelta("ный**".into()));
        let text: String = app.log.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("жирный"), "собранный текст: {text}");
        assert!(!text.contains("**"), "незакрытый маркер заменён: {text}");
        // финал не дублирует: AgentText перерендеривает тот же блок
        app.on_event(AgentEvent::AgentText("## Тайтл\nтекст **жирный**".into()));
        let text: String = app.log.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert_eq!(text.matches("Тайтл").count(), 1, "без дублей: {text}");
        assert_eq!(text.matches("жирный").count(), 1, "без дублей: {text}");
        // стрим-состояние сброшено
        assert!(app.stream_line_idx.is_none() && app.stream_block_len == 0
            && app.stream_text.is_empty());
    }

    /// Ручной скролл колесом (баг пользователя 20.07 «проваливается в пустой
    /// экран»): окно ручного режима всегда полное — верх клампится к последнему
    /// ПОЛНОМУ окну (manual_max_top), пустого «подвала» под концом лога нет.
    #[test]
    fn manual_scroll_window_stays_full() {
        let mut app = TuiApp::new();
        for i in 1..=20 {
            app.push(vec![Span::raw(format!("строка {i}"))]);
        }
        app.follow = false;
        let backend = TestBackend::new(60, 12); // видимых строк лога: 12-1-3-2 = 6
        let mut terminal = Terminal::new(backend).unwrap();
        // середина лога: окно заполнено контентом, без пустоты снизу
        app.scroll = 10;
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let joined = buffer_lines(&terminal).join("\n");
        assert!(joined.contains("строка 13"), "окно отсюда:\n{joined}");
        assert!(joined.contains("строка 18"), "и досюда (6 полных строк):\n{joined}");
        assert!(!joined.contains("строка 19"), "за окном:\n{joined}");
        // scroll далеко за max_top (= 20-6 = 14): показывается ПОЛНОЕ нижнее
        // окно (строки 15..20), а не «1 строка текста + пустой экран»
        app.scroll = 19;
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let joined = buffer_lines(&terminal).join("\n");
        assert!(joined.contains("строка 20"), "низ лога виден:\n{joined}");
        assert!(joined.contains("строка 15"), "окно полное сверху:\n{joined}");
        assert!(!joined.contains("строка 14"), "выше окна:\n{joined}");
    }

    /// Индикатор мышления (v0.5.5, замечание пользователя «спиннер в шапке не видно»):
    /// пока агент работает — «думаю…» в конце лога и «агент думает…» в заголовке ввода.
    #[test]
    fn thinking_indicator_visible_while_running() {
        let mut app = TuiApp::new();
        app.on_event(AgentEvent::UserMsg("задача".into()));
        app.agent_running = true;
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let joined = buffer_lines(&terminal).join("\n");
        assert!(joined.contains("думаю…"), "нет «думаю…» в логе:\n{joined}");
        assert!(joined.contains("агент думает"), "нет «агент думает…» в заголовке ввода:\n{joined}");
        // когда агент закончил — индикатор исчезает
        app.agent_running = false;
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let joined2 = buffer_lines(&terminal).join("\n");
        assert!(!joined2.contains("думаю…"), "индикатор не исчез:\n{joined2}");
        assert!(!joined2.contains("агент думает"), "индикатор не исчез из заголовка:\n{joined2}");
    }

    /// Компактный трейс инструментов (v0.6.0): вызов и результат — одна строка,
    /// как у лидеров (не 4-5 строк на инструмент).
    #[test]
    fn tool_call_and_result_share_one_line() {
        let mut app = TuiApp::new();
        app.on_event(AgentEvent::ToolCall {
            name: "read_file".into(), args: r#"{"path":"a.txt"}"#.into(), decision: "Allow".into(),
        });
        app.on_event(AgentEvent::ToolResult {
            name: "read_file".into(), preview: "содержимое файла".into(), ok: true,
        });
        assert_eq!(app.log.len(), 1, "вызов+результат обязаны быть одной строкой: {}", app.log.len());
        let text: String = app.log[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("read_file"), "{text}");
        assert!(text.contains("→ содержимое файла"), "{text}");
        // два инструмента подряд — две строки, без разделителя
        app.on_event(AgentEvent::ToolCall {
            name: "grep".into(), args: r#"{"pattern":"x"}"#.into(), decision: "Allow".into(),
        });
        app.on_event(AgentEvent::ToolResult {
            name: "grep".into(), preview: "a.txt:1:x".into(), ok: true,
        });
        assert_eq!(app.log.len(), 2, "два инструмента — две строки: {}", app.log.len());
    }

    /// Регрессия (скриншот 12-15-53): длинная команда в запросе разрешения
    /// не вытесняет строки ответа [y]/[a]/[n] — они всегда в фиксированной
    /// нижней зоне попапа.
    #[test]
    fn perm_popup_keeps_answers_visible_with_long_command() {
        let mut app = TuiApp::new();
        let long_q = (1..=30)
            .map(|i| format!("команда строка {i} с некоторым текстом"))
            .collect::<Vec<_>>()
            .join("\n");
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app, Some(&long_q))).unwrap();
        let joined = buffer_lines(&terminal).join("\n");
        assert!(joined.contains("[y] разрешить"), "строка ответа не видна:\n{joined}");
        assert!(joined.contains("[n] отклонить"), "строка отказа не видна:\n{joined}");
    }

    /// Индикатор режима в заголовке ввода (v0.5.8): бейдж режима слева.
    #[test]
    fn mode_badge_visible_in_input_title() {
        let mut app = TuiApp::new();
        app.mode_code = crate::permissions::MODE_SEMI;
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let joined = buffer_lines(&terminal).join("\n");
        assert!(joined.contains("Авто-правки"), "нет бейджа полуавтомата:\n{joined}");
        app.mode_code = crate::permissions::MODE_YOLO;
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let joined2 = buffer_lines(&terminal).join("\n");
        assert!(joined2.contains("Автомат"), "нет бейджа автомата:\n{joined2}");
        app.mode_code = crate::permissions::MODE_ASK;
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let joined3 = buffer_lines(&terminal).join("\n");
        assert!(joined3.contains("Совет"), "нет бейджа Совета:\n{joined3}");
    }
}

// ---------------------------------------------------------------------------
// Тесты выделения мышью: plain-текст, извлечение, маппинг, подсветка
// ---------------------------------------------------------------------------

#[cfg(test)]
mod selection_tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// Приложение с готовым логом из plain-строк (по спану на строку).
    fn app_with(lines: &[&str]) -> TuiApp {
        let mut app = TuiApp::new();
        for line in lines {
            app.push(vec![Span::raw((*line).to_string())]);
        }
        app
    }

    /// Тестовая область лога без рамки: колонка 1, строки 0..height.
    fn area(height: u16) -> Rect {
        Rect::new(1, 0, 60, height)
    }

    /// line_plain: спаны склеиваются в один текст, стили отбрасываются.
    #[test]
    fn line_plain_concatenates_spans() {
        let line = LogLine {
            spans: vec![
                Span::styled("12:00 ".to_string(), Style::default().fg(Color::Red)),
                Span::raw("❯ "),
                Span::styled("привет".to_string(), Style::default().add_modifier(Modifier::BOLD)),
            ],
        };
        assert_eq!(line_plain(&line), "12:00 ❯ привет");
    }

    /// Одна строка: обрезка по колонкам, правая граница не включительна.
    #[test]
    fn extract_single_line_clips_columns() {
        let mut app = app_with(&["0123456789"]);
        app.log_area = area(1);
        // экранные колонки 3..7 → символы 2..6 (минус рамка x=1)
        let sel = Sel { anchor: (3, 0), current: (7, 0) };
        assert_eq!(extract_selection(&app, sel), "2345");
    }

    /// Диапазон строк: крайние обрезаны по колонкам, средняя целиком, склейка «\n».
    #[test]
    fn extract_line_range_trims_edges() {
        let mut app = app_with(&["aaa", "bbb", "ccc"]);
        app.log_area = area(3);
        let sel = Sel { anchor: (2, 0), current: (3, 2) };
        assert_eq!(extract_selection(&app, sel), "aa\nbbb\ncc");
    }

    /// Драг снизу-вверх нормализуется к тому же тексту, что и сверху-вниз.
    #[test]
    fn extract_reverse_drag_normalized() {
        let mut app = app_with(&["aaa", "bbb", "ccc"]);
        app.log_area = area(3);
        let forward = Sel { anchor: (2, 0), current: (3, 2) };
        let reverse = Sel { anchor: (3, 2), current: (2, 0) };
        assert_eq!(extract_selection(&app, forward), extract_selection(&app, reverse));
    }

    /// Точки за пределами области зажимаются к краям (драг за рамкой и за экраном).
    #[test]
    fn extract_clips_out_of_area_points() {
        let mut app = app_with(&["hello", "world"]);
        app.log_area = Rect::new(1, 1, 10, 2);
        let sel = Sel { anchor: (0, 0), current: (200, 200) };
        assert_eq!(extract_selection(&app, sel), "hello\nworld");
    }

    /// Клик без драга — пустое выделение (буфер не трогаем).
    #[test]
    fn extract_click_without_drag_is_empty() {
        let mut app = app_with(&["строка"]);
        app.log_area = area(1);
        let sel = Sel { anchor: (3, 0), current: (3, 0) };
        assert_eq!(extract_selection(&app, sel), "");
    }

    /// Маппинг экранных строк в log-индексы повторяет draw(): при follow —
    /// хвост лога, при scroll — окно от scroll; одна и та же экранная
    /// область в двух режимах даёт разный, но точный текст.
    #[test]
    fn extract_maps_rows_like_draw() {
        let mut app = TuiApp::new();
        for i in 0..10 {
            app.push(vec![Span::raw(format!("l{i}"))]);
        }
        app.log_area = area(3);
        let sel = Sel { anchor: (1, 0), current: (60, 2) };
        // follow: виден хвост из 3 строк — l7..l9
        assert_eq!(extract_selection(&app, sel), "l7\nl8\nl9");
        // ручной скролл: окно от scroll=4 — l4..l6
        app.follow = false;
        app.scroll = 4;
        assert_eq!(extract_selection(&app, sel), "l4\nl5\nl6");
    }

    /// Рендер: строки внутри активного выделения рисуются с REVERSED,
    /// соседние — без инверсии; draw() при этом заполняет app.log_area.
    #[test]
    fn draw_highlights_selected_rows_reversed() {
        let mut app = app_with(&["первая строка", "вторая строка", "третья строка"]);
        // терминал 40x12: заголовок 1 + ввод 3 → лог y=1..9, внутренняя y=2..8
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let la = app.log_area;
        assert!(la.width > 0 && la.height > 0, "log_area заполнена draw: {la:?}");
        // выделяем вторую видимую строку лога целиком
        app.sel = Some(Sel { anchor: (la.x, la.y + 1), current: (la.x + la.width - 1, la.y + 1) });
        terminal.draw(|f| draw(f, &mut app, None)).unwrap();
        let buf = terminal.backend().buffer();
        let row = la.y + 1;
        let cell = buf.get(la.x, row);
        assert!(cell.modifier.contains(Modifier::REVERSED),
            "строка {row} без REVERSED: {cell:?}");
        let above = buf.get(la.x, row - 1);
        assert!(!above.modifier.contains(Modifier::REVERSED),
            "соседняя строка инвертирована: {above:?}");
    }
}

// ---------------------------------------------------------------------------
// Тесты бэкендов буфера обмена (без реального X; живой прогон — #[ignore])
// ---------------------------------------------------------------------------

#[cfg(test)]
mod clipboard_tests {
    use super::*;

    /// Порядок выбора нативного бэкенда по инъецированному «искателю»:
    /// wl-copy → xclip → xsel; первый найденный побеждает; пустой PATH →
    /// None (сработает python-хелпер).
    #[test]
    fn detect_backends_order_and_fallback() {
        assert_eq!(detect_backends(|_| true), Some("wl-copy"));
        assert_eq!(detect_backends(|p| p != "wl-copy"), Some("xclip"));
        assert_eq!(detect_backends(|p| p == "xsel"), Some("xsel"));
        assert_eq!(detect_backends(|_| false), None);
    }

    /// Живой end-to-end против реального X11: хелпер захватывает CLIPBOARD,
    /// а отдельный Xlib-клиент (как вставляющее приложение) читает selection
    /// обратно. Ручной прогон: cargo test clipboard_tests -- --ignored
    #[test]
    #[ignore = "x11"]
    fn python_helper_roundtrip_real_x11() {
        if std::env::var_os("DISPLAY").is_none() {
            eprintln!("DISPLAY не задан — пропуск");
            return;
        }
        let text = "theseus x11 roundtrip ✓ 42";
        let backend = copy_to_clipboard(text).unwrap();
        assert!(backend == "python-xlib" || NATIVE_CLIP_BACKENDS.contains(&backend.as_str()),
            "неожиданный бэкенд: {backend}");
        let reader = std::env::temp_dir().join("theseus_clip_reader_test.py");
        std::fs::write(&reader, PYTHON_CLIP_READER).unwrap();
        let out = Command::new("python3").arg(&reader).output().unwrap();
        assert!(out.status.success(), "reader: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8(out.stdout).unwrap(), text);
    }
}
