//! Темы оформления TUI: цветовые спецификации, семантические роли,
//! встроенные темы (dark / light / mono), разбор пользовательских тем
//! из TOML и проверка контрастности по формулам WCAG.
//!
//! Модуль сознательно не зависит от ratatui/crossterm: типы цвета свои,
//! а генерация ANSI escape-последовательностей делается вручную
//! (образец — `codex-rs/tui`, `styles.md`).
//!
//! Замечание о mono-теме: она не задаёт ни одного цвета (все роли —
//! [`ColorSpec::Default`]). Дифференциация текста в таком терминале
//! достигается атрибутами Bold/Dim, которые этот модуль намеренно не
//! моделирует: здесь описываются только цвета.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

/// Порог предупреждения о низком контрасте.
///
/// По WCAG коэффициент 3.0 — минимум для крупного текста и графических
/// компонентов интерфейса; для обычного текста рекомендуется 4.5.
/// Проверка [`contrast_check`] помечает роли с коэффициентом ниже этого
/// порога.
pub const CONTRAST_WARN_THRESHOLD: f64 = 3.0;

// ---------------------------------------------------------------------------
// Color16 — стандартная 16-цветовая палитра терминала
// ---------------------------------------------------------------------------

/// Один из 16 стандартных цветов ANSI-терминала.
///
/// Индексы 0–7 — обычные цвета (SGR 30–37), 8–15 — яркие (SGR 90–97).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Color16 {
    /// Чёрный (0).
    Black,
    /// Красный (1).
    Red,
    /// Зелёный (2).
    Green,
    /// Жёлтый (3).
    Yellow,
    /// Синий (4).
    Blue,
    /// Пурпурный (5).
    Magenta,
    /// Бирюзовый (6).
    Cyan,
    /// Белый (7).
    White,
    /// Ярко-чёрный, он же серый (8).
    BrightBlack,
    /// Ярко-красный (9).
    BrightRed,
    /// Ярко-зелёный (10).
    BrightGreen,
    /// Ярко-жёлтый (11).
    BrightYellow,
    /// Ярко-синий (12).
    BrightBlue,
    /// Ярко-пурпурный (13).
    BrightMagenta,
    /// Ярко-бирюзовый (14).
    BrightCyan,
    /// Ярко-белый (15).
    BrightWhite,
}

impl Color16 {
    /// Все цвета палитры в порядке их ANSI-индексов (0–15).
    pub const ALL: [Self; 16] = [
        Self::Black,
        Self::Red,
        Self::Green,
        Self::Yellow,
        Self::Blue,
        Self::Magenta,
        Self::Cyan,
        Self::White,
        Self::BrightBlack,
        Self::BrightRed,
        Self::BrightGreen,
        Self::BrightYellow,
        Self::BrightBlue,
        Self::BrightMagenta,
        Self::BrightCyan,
        Self::BrightWhite,
    ];

    /// Индекс цвета в палитре (0–15).
    pub fn index(self) -> u8 {
        match self {
            Self::Black => 0,
            Self::Red => 1,
            Self::Green => 2,
            Self::Yellow => 3,
            Self::Blue => 4,
            Self::Magenta => 5,
            Self::Cyan => 6,
            Self::White => 7,
            Self::BrightBlack => 8,
            Self::BrightRed => 9,
            Self::BrightGreen => 10,
            Self::BrightYellow => 11,
            Self::BrightBlue => 12,
            Self::BrightMagenta => 13,
            Self::BrightCyan => 14,
            Self::BrightWhite => 15,
        }
    }

    /// Каноническое имя цвета (kebab-case), обратное к [`Color16::from_name`].
    pub fn name(self) -> &'static str {
        match self {
            Self::Black => "black",
            Self::Red => "red",
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Blue => "blue",
            Self::Magenta => "magenta",
            Self::Cyan => "cyan",
            Self::White => "white",
            Self::BrightBlack => "bright-black",
            Self::BrightRed => "bright-red",
            Self::BrightGreen => "bright-green",
            Self::BrightYellow => "bright-yellow",
            Self::BrightBlue => "bright-blue",
            Self::BrightMagenta => "bright-magenta",
            Self::BrightCyan => "bright-cyan",
            Self::BrightWhite => "bright-white",
        }
    }

    /// SGR-параметр для установки этого цвета как цвета текста (30–37 / 90–97).
    pub fn fg_code(self) -> u8 {
        let idx = self.index();
        if idx < 8 { 30 + idx } else { 90 + idx - 8 }
    }

    /// Разбор имени цвета, регистронезависимо.
    ///
    /// Принимаются имена вида `red`, `bright-blue`, `bright_blue`,
    /// `bright blue`, а также синонимы `gray`/`grey` для ярко-чёрного.
    /// Неизвестные имена дают `None`.
    pub fn from_name(name: &str) -> Option<Self> {
        let normalized: String = name
            .chars()
            .filter(|ch| !matches!(ch, '-' | '_' | ' '))
            .flat_map(char::to_lowercase)
            .collect();
        match normalized.as_str() {
            "black" => Some(Self::Black),
            "red" => Some(Self::Red),
            "green" => Some(Self::Green),
            "yellow" => Some(Self::Yellow),
            "blue" => Some(Self::Blue),
            "magenta" => Some(Self::Magenta),
            "cyan" => Some(Self::Cyan),
            "white" => Some(Self::White),
            "brightblack" | "gray" | "grey" => Some(Self::BrightBlack),
            "brightred" => Some(Self::BrightRed),
            "brightgreen" => Some(Self::BrightGreen),
            "brightyellow" => Some(Self::BrightYellow),
            "brightblue" => Some(Self::BrightBlue),
            "brightmagenta" => Some(Self::BrightMagenta),
            "brightcyan" => Some(Self::BrightCyan),
            "brightwhite" => Some(Self::BrightWhite),
            _ => None,
        }
    }

    /// Приблизительное RGB-представление цвета (палитра VGA).
    ///
    /// Реальный цвет зависит от терминала, но для расчёта контрастности
    /// достаточно типичного приближения.
    pub fn approx_rgb(self) -> (u8, u8, u8) {
        match self {
            Self::Black => (0, 0, 0),
            Self::Red => (170, 0, 0),
            Self::Green => (0, 170, 0),
            Self::Yellow => (170, 85, 0),
            Self::Blue => (0, 0, 170),
            Self::Magenta => (170, 0, 170),
            Self::Cyan => (0, 170, 170),
            Self::White => (170, 170, 170),
            Self::BrightBlack => (85, 85, 85),
            Self::BrightRed => (255, 85, 85),
            Self::BrightGreen => (85, 255, 85),
            Self::BrightYellow => (255, 255, 85),
            Self::BrightBlue => (85, 85, 255),
            Self::BrightMagenta => (255, 85, 255),
            Self::BrightCyan => (85, 255, 255),
            Self::BrightWhite => (255, 255, 255),
        }
    }
}

// ---------------------------------------------------------------------------
// ColorSpec — конкретный цвет роли
// ---------------------------------------------------------------------------

/// Цвет, назначенный семантической роли темы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ColorSpec {
    /// «Без цвета»: использовать цвет терминала по умолчанию.
    ///
    /// Это дно fallback-цепочки [`Theme::get`] и единственный «цвет»
    /// mono-темы. В ANSI выражается SGR-последовательностью `39`
    /// (сброс цвета текста к умолчательному).
    Default,
    /// Цвет из 16-цветовой ANSI-палитры.
    Ansi(Color16),
    /// Точный 24-битный RGB-цвет.
    Rgb(u8, u8, u8),
}

impl ColorSpec {
    /// Приблизительное RGB-представление цвета.
    ///
    /// Для [`ColorSpec::Default`] возвращает `None`: умолчательный цвет
    /// терминала заранее неизвестен, и расчёт яркости для него невозможен.
    pub fn approx_rgb(self) -> Option<(u8, u8, u8)> {
        match self {
            Self::Default => None,
            Self::Ansi(color) => Some(color.approx_rgb()),
            Self::Rgb(r, g, b) => Some((r, g, b)),
        }
    }
}

// ---------------------------------------------------------------------------
// ThemeRole — семантические роли цветов
// ---------------------------------------------------------------------------

/// Семантическая роль цвета в интерфейсе.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ThemeRole {
    /// Акцентный цвет (заголовки, ссылки, активные элементы).
    Accent,
    /// Приглушённый текст (подсказки, второстепенная информация).
    Dim,
    /// Ошибки.
    Error,
    /// Предупреждения.
    Warn,
    /// Успешные операции.
    Ok,
    /// Текст пользователя.
    UserText,
    /// Текст агента; также середина fallback-цепочки [`Theme::get`].
    AgentText,
    /// Имена инструментов в ленте вызовов.
    ToolName,
    /// Строка состояния.
    StatusBar,
    /// Фон всплывающих окон; используется как фон темы в [`contrast_check`].
    PopupBg,
    /// Выделенный текст / текущий элемент списка.
    Selection,
}

impl ThemeRole {
    /// Все роли, в порядке объявления.
    pub const ALL: [Self; 11] = [
        Self::Accent,
        Self::Dim,
        Self::Error,
        Self::Warn,
        Self::Ok,
        Self::UserText,
        Self::AgentText,
        Self::ToolName,
        Self::StatusBar,
        Self::PopupBg,
        Self::Selection,
    ];

    /// Имя роли в snake_case — ключ секции `[roles]` в TOML.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accent => "accent",
            Self::Dim => "dim",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Ok => "ok",
            Self::UserText => "user_text",
            Self::AgentText => "agent_text",
            Self::ToolName => "tool_name",
            Self::StatusBar => "status_bar",
            Self::PopupBg => "popup_bg",
            Self::Selection => "selection",
        }
    }

    /// Разбор имени роли; принимаются snake_case и kebab-case,
    /// регистронезависимо. Неизвестные имена дают `None`.
    pub fn from_name(name: &str) -> Option<Self> {
        let normalized: String = name
            .chars()
            .map(|ch| if ch == '-' { '_' } else { ch })
            .flat_map(char::to_lowercase)
            .collect();
        match normalized.as_str() {
            "accent" => Some(Self::Accent),
            "dim" => Some(Self::Dim),
            "error" => Some(Self::Error),
            "warn" => Some(Self::Warn),
            "ok" => Some(Self::Ok),
            "user_text" => Some(Self::UserText),
            "agent_text" => Some(Self::AgentText),
            "tool_name" => Some(Self::ToolName),
            "status_bar" => Some(Self::StatusBar),
            "popup_bg" => Some(Self::PopupBg),
            "selection" => Some(Self::Selection),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// ThemeError — ошибки разбора темы из TOML
// ---------------------------------------------------------------------------

/// Ошибка разбора темы из TOML-значения (см. [`from_toml`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeError {
    /// Корневое значение не является TOML-таблицей.
    NotATable,
    /// Отсутствует обязательное поле `name`.
    MissingName,
    /// Поле `name` не является строкой.
    InvalidName,
    /// Секция `roles` не является TOML-таблицей.
    RolesNotTable,
    /// В секции `roles` встречена неизвестная роль.
    UnknownRole(String),
    /// Значение цвета для роли не является строкой.
    NonStringColor(String),
    /// Строку цвета не удалось распознать
    /// (ожидались `#rrggbb`, ANSI-имя или `default`).
    BadColorSpec {
        /// Имя роли, у которой цвет не распознан.
        role: String,
        /// Исходная строка значения.
        value: String,
    },
}

impl fmt::Display for ThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotATable => write!(f, "корень темы должен быть TOML-таблицей"),
            Self::MissingName => write!(f, "у темы отсутствует обязательное поле `name`"),
            Self::InvalidName => write!(f, "поле `name` темы должно быть строкой"),
            Self::RolesNotTable => write!(f, "секция `roles` должна быть TOML-таблицей"),
            Self::UnknownRole(role) => write!(f, "неизвестная роль темы: `{role}`"),
            Self::NonStringColor(role) => {
                write!(f, "цвет роли `{role}` должен быть строкой")
            }
            Self::BadColorSpec { role, value } => write!(
                f,
                "нераспознанный цвет `{value}` у роли `{role}` \
                 (ожидается #rrggbb, ANSI-имя или `default`)"
            ),
        }
    }
}

impl Error for ThemeError {}

// ---------------------------------------------------------------------------
// Theme — именованный набор цветов по ролям
// ---------------------------------------------------------------------------

/// Именованная тема: отображение семантических ролей в цвета.
///
/// Роли могут отсутствовать — тогда работает fallback-цепочка
/// в [`Theme::get`].
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    /// Имя темы (`dark`, `light`, `mono` или пользовательское).
    pub name: String,
    /// Цвета по ролям. `BTreeMap` даёт стабильный порядок обхода.
    pub roles: BTreeMap<ThemeRole, ColorSpec>,
}

impl Theme {
    /// Пустая тема с заданным именем (все роли — по fallback-цепочке).
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            roles: BTreeMap::new(),
        }
    }

    /// Builder: назначить цвет роли и вернуть тему.
    pub fn with_role(mut self, role: ThemeRole, spec: ColorSpec) -> Self {
        self.set(role, spec);
        self
    }

    /// Назначить (или переопределить) цвет роли.
    pub fn set(&mut self, role: ThemeRole, spec: ColorSpec) {
        let _ = self.roles.insert(role, spec);
    }

    /// Задан ли цвет роли явно (без учёта fallback).
    pub fn has_role(&self, role: ThemeRole) -> bool {
        self.roles.contains_key(&role)
    }

    /// Цвет роли с fallback-цепочкой: сама роль → [`ThemeRole::AgentText`]
    /// → «Fg» (цвет терминала по умолчанию, [`ColorSpec::Default`]).
    pub fn get(&self, role: ThemeRole) -> ColorSpec {
        self.roles
            .get(&role)
            .or(self.roles.get(&ThemeRole::AgentText))
            .copied()
            .unwrap_or(ColorSpec::Default)
    }
}

// ---------------------------------------------------------------------------
// Встроенные темы
// ---------------------------------------------------------------------------

/// Встроенные темы: `dark`, `light` и `mono`.
///
/// В dark- и light-темах цвета заданы для всех ролей явно.
/// Mono-тема цветов не задаёт вовсе: все роли — [`ColorSpec::Default`],
/// а дифференциация текста остаётся на атрибуты терминала (Bold/Dim),
/// которые этот модуль не моделирует.
pub fn builtin_themes() -> Vec<Theme> {
    vec![dark_theme(), light_theme(), mono_theme()]
}

/// Тёмная тема: светлые цвета текста на тёмном фоне терминала.
fn dark_theme() -> Theme {
    Theme::new("dark")
        .with_role(ThemeRole::Accent, ColorSpec::Ansi(Color16::BrightCyan))
        .with_role(ThemeRole::Dim, ColorSpec::Rgb(140, 143, 150))
        .with_role(ThemeRole::Error, ColorSpec::Ansi(Color16::BrightRed))
        .with_role(ThemeRole::Warn, ColorSpec::Ansi(Color16::BrightYellow))
        .with_role(ThemeRole::Ok, ColorSpec::Ansi(Color16::BrightGreen))
        .with_role(ThemeRole::UserText, ColorSpec::Ansi(Color16::BrightWhite))
        .with_role(ThemeRole::AgentText, ColorSpec::Ansi(Color16::White))
        .with_role(ThemeRole::ToolName, ColorSpec::Ansi(Color16::BrightMagenta))
        .with_role(ThemeRole::StatusBar, ColorSpec::Rgb(215, 218, 228))
        .with_role(ThemeRole::PopupBg, ColorSpec::Rgb(40, 42, 54))
        .with_role(ThemeRole::Selection, ColorSpec::Rgb(160, 220, 255))
}

/// Светлая тема: тёмные цвета текста на светлом фоне терминала.
fn light_theme() -> Theme {
    Theme::new("light")
        .with_role(ThemeRole::Accent, ColorSpec::Ansi(Color16::Blue))
        .with_role(ThemeRole::Dim, ColorSpec::Ansi(Color16::BrightBlack))
        .with_role(ThemeRole::Error, ColorSpec::Ansi(Color16::Red))
        .with_role(ThemeRole::Warn, ColorSpec::Ansi(Color16::Yellow))
        .with_role(ThemeRole::Ok, ColorSpec::Rgb(0, 135, 0))
        .with_role(ThemeRole::UserText, ColorSpec::Ansi(Color16::Black))
        .with_role(ThemeRole::AgentText, ColorSpec::Rgb(35, 35, 35))
        .with_role(ThemeRole::ToolName, ColorSpec::Ansi(Color16::Magenta))
        .with_role(ThemeRole::StatusBar, ColorSpec::Rgb(70, 75, 90))
        .with_role(ThemeRole::PopupBg, ColorSpec::Rgb(240, 240, 240))
        .with_role(ThemeRole::Selection, ColorSpec::Rgb(0, 90, 180))
}

/// Монохромная тема: ни одного цвета, только умолчания терминала.
fn mono_theme() -> Theme {
    let mut theme = Theme::new("mono");
    for role in ThemeRole::ALL {
        theme.set(role, ColorSpec::Default);
    }
    theme
}

// ---------------------------------------------------------------------------
// Разбор темы из TOML
// ---------------------------------------------------------------------------

/// Разбор темы из TOML-значения вида:
///
/// ```toml
/// name = "my-theme"
///
/// [roles]
/// accent = "#ff8800"        # hex #rrggbb
/// dim = "bright-black"      # ANSI-имя (см. Color16::from_name)
/// agent_text = "default"    # цвет терминала по умолчанию
/// ```
///
/// Секция `[roles]` необязательна; незаданные роли вычисляются
/// fallback-цепочкой [`Theme::get`].
pub fn from_toml(value: &toml::Value) -> Result<Theme, ThemeError> {
    let table = value.as_table().ok_or(ThemeError::NotATable)?;
    let name_value = table.get("name").ok_or(ThemeError::MissingName)?;
    let name = name_value.as_str().ok_or(ThemeError::InvalidName)?;
    let mut theme = Theme::new(name);
    if let Some(roles_value) = table.get("roles") {
        let roles = roles_value.as_table().ok_or(ThemeError::RolesNotTable)?;
        for (key, color_value) in roles {
            let role =
                ThemeRole::from_name(key).ok_or_else(|| ThemeError::UnknownRole(key.clone()))?;
            let raw = color_value
                .as_str()
                .ok_or_else(|| ThemeError::NonStringColor(key.clone()))?;
            let spec = parse_color(key, raw)?;
            theme.set(role, spec);
        }
    }
    Ok(theme)
}

/// Разбор строки цвета: `#rrggbb`, ANSI-имя или `default`.
fn parse_color(role: &str, raw: &str) -> Result<ColorSpec, ThemeError> {
    let text = raw.trim();
    if let Some(hex) = text.strip_prefix('#') {
        if hex.len() == 6 {
            if let Ok(value) = u32::from_str_radix(hex, 16) {
                let r = ((value >> 16) & 0xff) as u8;
                let g = ((value >> 8) & 0xff) as u8;
                let b = (value & 0xff) as u8;
                return Ok(ColorSpec::Rgb(r, g, b));
            }
        }
        return Err(bad_color(role, raw));
    }
    if text.eq_ignore_ascii_case("default") {
        return Ok(ColorSpec::Default);
    }
    Color16::from_name(text)
        .map(ColorSpec::Ansi)
        .ok_or_else(|| bad_color(role, raw))
}

/// Конструктор одноимённой ошибки с копированием строк.
fn bad_color(role: &str, raw: &str) -> ThemeError {
    ThemeError::BadColorSpec {
        role: role.to_string(),
        value: raw.to_string(),
    }
}

// ---------------------------------------------------------------------------
// ANSI escape-последовательности
// ---------------------------------------------------------------------------

/// ANSI SGR escape-последовательность для установки цвета текста.
///
/// - [`ColorSpec::Default`] → `\x1b[39m` (сброс к умолчательному цвету);
/// - [`ColorSpec::Ansi`] → `\x1b[30m`…`\x1b[37m` / `\x1b[90m`…`\x1b[97m`;
/// - [`ColorSpec::Rgb`] → `\x1b[38;2;R;G;Bm` (24-битный цвет).
pub fn to_ansi_fg(spec: ColorSpec) -> String {
    match spec {
        ColorSpec::Default => "\u{1b}[39m".to_string(),
        ColorSpec::Ansi(color) => {
            let code = color.fg_code();
            format!("\u{1b}[{code}m")
        }
        ColorSpec::Rgb(r, g, b) => format!("\u{1b}[38;2;{r};{g};{b}m"),
    }
}

// ---------------------------------------------------------------------------
// Проверка контрастности (WCAG)
// ---------------------------------------------------------------------------

/// Линеаризация одного 8-битного канала sRGB по формуле WCAG 2.x.
fn linear_channel(channel: u8) -> f64 {
    let c = f64::from(channel) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Относительная яркость цвета (0.0 — чёрный, 1.0 — белый) по WCAG 2.x.
fn relative_luminance(rgb: (u8, u8, u8)) -> f64 {
    0.2126 * linear_channel(rgb.0) + 0.7152 * linear_channel(rgb.1) + 0.0722 * linear_channel(rgb.2)
}

/// Коэффициент контрастности двух яркостей: от 1.0 (нет контраста) до 21.0.
fn contrast_ratio(lum1: f64, lum2: f64) -> f64 {
    let (hi, lo) = if lum1 >= lum2 { (lum1, lum2) } else { (lum2, lum1) };
    (hi + 0.05) / (lo + 0.05)
}

/// Проверка контрастности ролей темы по формуле WCAG.
///
/// Для каждой роли вычисляется коэффициент контрастности её цвета
/// относительно фона темы. Фоном считается цвет роли [`ThemeRole::PopupBg`];
/// если она не задана (или равна [`ColorSpec::Default`]) — чёрный цвет,
/// как у типичного тёмного терминала.
///
/// Возвращает список `(роль, коэффициент)` для ролей с коэффициентом
/// ниже [`CONTRAST_WARN_THRESHOLD`], отсортированный по возрастанию
/// коэффициента. Роли с цветом [`ColorSpec::Default`] (их яркость
/// неизвестна) и сама роль [`ThemeRole::PopupBg`] (фон) пропускаются.
pub fn contrast_check(theme: &Theme) -> Vec<(ThemeRole, f64)> {
    let background = theme
        .roles
        .get(&ThemeRole::PopupBg)
        .copied()
        .and_then(ColorSpec::approx_rgb)
        .unwrap_or((0, 0, 0));
    let bg_lum = relative_luminance(background);
    let mut flagged = Vec::new();
    for (&role, &spec) in &theme.roles {
        if role == ThemeRole::PopupBg {
            continue;
        }
        let Some(rgb) = spec.approx_rgb() else {
            continue;
        };
        let ratio = contrast_ratio(relative_luminance(rgb), bg_lum);
        if ratio < CONTRAST_WARN_THRESHOLD {
            flagged.push((role, ratio));
        }
    }
    flagged.sort_by(|a, b| a.1.total_cmp(&b.1));
    flagged
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Разбор темы из TOML-исходника (тестовый помощник).
    fn parse_theme(src: &str) -> Result<Theme, ThemeError> {
        let value: toml::Value = toml::from_str(src).expect("TOML в тесте должен быть валидным");
        from_toml(&value)
    }

    #[test]
    fn parse_hex_colors_lowercase_and_uppercase() {
        let src = "name = \"t\"\n[roles]\naccent = \"#ff8800\"\nerror = \"#FF0080\"\n";
        let theme = parse_theme(src).expect("тема должна разобраться");
        assert_eq!(theme.name, "t");
        assert_eq!(theme.get(ThemeRole::Accent), ColorSpec::Rgb(255, 136, 0));
        assert_eq!(theme.get(ThemeRole::Error), ColorSpec::Rgb(255, 0, 128));
    }

    #[test]
    fn parse_ansi_color_names() {
        let cases = [
            ("red", Color16::Red),
            ("BRIGHT-BLUE", Color16::BrightBlue),
            ("bright_green", Color16::BrightGreen),
            ("gray", Color16::BrightBlack),
            ("grey", Color16::BrightBlack),
            ("bright white", Color16::BrightWhite),
            ("BrightYellow", Color16::BrightYellow),
        ];
        for (raw, want) in cases {
            assert_eq!(Color16::from_name(raw), Some(want), "имя: {raw}");
        }
        let src = "name = \"t\"\n[roles]\nwarn = \"bright-yellow\"\n";
        let theme = parse_theme(src).expect("тема должна разобраться");
        assert_eq!(theme.get(ThemeRole::Warn), ColorSpec::Ansi(Color16::BrightYellow));
    }

    #[test]
    fn parse_default_keyword_gives_no_color() {
        let src = "name = \"t\"\n[roles]\nagent_text = \"default\"\n";
        let theme = parse_theme(src).expect("тема должна разобраться");
        assert_eq!(theme.get(ThemeRole::AgentText), ColorSpec::Default);
    }

    #[test]
    fn color16_name_roundtrip_and_fg_codes() {
        let mut indices = Vec::new();
        for color in Color16::ALL {
            assert_eq!(Color16::from_name(color.name()), Some(color));
            indices.push(color.index());
            let code = color.fg_code();
            assert!(
                (30..=37).contains(&code) || (90..=97).contains(&code),
                "неожиданный SGR-код {code} у {}",
                color.name()
            );
        }
        indices.sort_unstable();
        assert_eq!(indices, (0..16).collect::<Vec<u8>>());
        assert_eq!(Color16::from_name("orange"), None);
    }

    #[test]
    fn theme_role_names_roundtrip() {
        for role in ThemeRole::ALL {
            assert_eq!(ThemeRole::from_name(role.as_str()), Some(role));
            let kebab = role.as_str().replace('_', "-");
            assert_eq!(ThemeRole::from_name(&kebab), Some(role));
        }
        assert_eq!(ThemeRole::from_name("bogus"), None);
    }

    #[test]
    fn from_toml_rejects_non_table_root() {
        let err = from_toml(&toml::Value::Integer(42)).expect_err("ожидалась ошибка");
        assert_eq!(err, ThemeError::NotATable);
    }

    #[test]
    fn from_toml_requires_name_string() {
        let err = parse_theme("[roles]\naccent = \"red\"\n").expect_err("ожидалась ошибка");
        assert_eq!(err, ThemeError::MissingName);
        let err = parse_theme("name = 5\n").expect_err("ожидалась ошибка");
        assert_eq!(err, ThemeError::InvalidName);
    }

    #[test]
    fn from_toml_rejects_non_table_roles() {
        let err = parse_theme("name = \"t\"\nroles = \"oops\"\n").expect_err("ожидалась ошибка");
        assert_eq!(err, ThemeError::RolesNotTable);
    }

    #[test]
    fn from_toml_rejects_unknown_role() {
        let src = "name = \"t\"\n[roles]\nbogus_role = \"red\"\n";
        let err = parse_theme(src).expect_err("ожидалась ошибка");
        assert_eq!(err, ThemeError::UnknownRole("bogus_role".to_string()));
    }

    #[test]
    fn from_toml_rejects_bad_color_values() {
        // Слишком короткий hex, битые hex-символы, неизвестное имя.
        for bad in ["#fff", "#zz0000", "#", "orange", ""] {
            let src = format!("name = \"t\"\n[roles]\naccent = \"{bad}\"\n");
            let err = parse_theme(&src).expect_err("ожидалась ошибка BadColorSpec");
            assert!(
                matches!(err, ThemeError::BadColorSpec { .. }),
                "значение {bad:?} дало {err:?}"
            );
        }
        // Нестроковое значение цвета.
        let err = parse_theme("name = \"t\"\n[roles]\naccent = 5\n").expect_err("ожидалась ошибка");
        assert_eq!(err, ThemeError::NonStringColor("accent".to_string()));
    }

    #[test]
    fn get_follows_fallback_chain() {
        // Пустая тема: всё падает в терминальный Fg (Default).
        let empty = Theme::new("empty");
        assert_eq!(empty.get(ThemeRole::Accent), ColorSpec::Default);
        assert_eq!(empty.get(ThemeRole::AgentText), ColorSpec::Default);

        // Задан только AgentText: прочие роли наследуют его.
        let theme = Theme::new("t").with_role(ThemeRole::AgentText, ColorSpec::Ansi(Color16::White));
        assert_eq!(theme.get(ThemeRole::ToolName), ColorSpec::Ansi(Color16::White));
        assert_eq!(theme.get(ThemeRole::UserText), ColorSpec::Ansi(Color16::White));

        // Явно заданная роль побеждает AgentText.
        let theme = theme.with_role(ThemeRole::ToolName, ColorSpec::Ansi(Color16::Magenta));
        assert_eq!(theme.get(ThemeRole::ToolName), ColorSpec::Ansi(Color16::Magenta));
        assert_eq!(theme.get(ThemeRole::AgentText), ColorSpec::Ansi(Color16::White));
        assert!(theme.has_role(ThemeRole::ToolName));
        assert!(!theme.has_role(ThemeRole::Accent));
    }

    #[test]
    fn builtin_themes_structure() {
        let themes = builtin_themes();
        let names: Vec<&str> = themes.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["dark", "light", "mono"]);
        for theme in &themes {
            for role in ThemeRole::ALL {
                if theme.name == "mono" {
                    // Mono-тема: все роли заданы явно, но «без цвета».
                    assert!(theme.has_role(role));
                    assert_eq!(theme.get(role), ColorSpec::Default);
                } else {
                    // Полные темы: все роли заданы явно, fallback не нужен.
                    assert!(theme.has_role(role), "{}: нет роли {}", theme.name, role.as_str());
                    assert_ne!(theme.get(role), ColorSpec::Default);
                }
            }
        }
    }

    #[test]
    fn mono_theme_has_no_colors() {
        let mono = builtin_themes()
            .into_iter()
            .find(|t| t.name == "mono")
            .expect("mono-тема обязана быть");
        assert!(mono.roles.values().all(|&c| c == ColorSpec::Default));
        // ANSI-последовательность «без цвета» — сброс к умолчательному.
        assert_eq!(to_ansi_fg(mono.get(ThemeRole::Accent)), "\u{1b}[39m");
        // Проверять контраст нечего: яркость Default неизвестна.
        assert!(contrast_check(&mono).is_empty());
    }

    #[test]
    fn builtin_dark_and_light_pass_contrast_check() {
        for theme in builtin_themes() {
            if theme.name == "mono" {
                continue;
            }
            let flagged = contrast_check(&theme);
            assert!(
                flagged.is_empty(),
                "тема {}: низкий контраст у {flagged:?}",
                theme.name
            );
        }
    }

    #[test]
    fn ansi_fg_escape_codes() {
        assert_eq!(to_ansi_fg(ColorSpec::Ansi(Color16::Black)), "\u{1b}[30m");
        assert_eq!(to_ansi_fg(ColorSpec::Ansi(Color16::Red)), "\u{1b}[31m");
        assert_eq!(to_ansi_fg(ColorSpec::Ansi(Color16::White)), "\u{1b}[37m");
        assert_eq!(to_ansi_fg(ColorSpec::Ansi(Color16::BrightRed)), "\u{1b}[91m");
        assert_eq!(to_ansi_fg(ColorSpec::Ansi(Color16::BrightWhite)), "\u{1b}[97m");
        assert_eq!(to_ansi_fg(ColorSpec::Rgb(1, 2, 3)), "\u{1b}[38;2;1;2;3m");
        assert_eq!(to_ansi_fg(ColorSpec::Rgb(255, 255, 255)), "\u{1b}[38;2;255;255;255m");
        assert_eq!(to_ansi_fg(ColorSpec::Default), "\u{1b}[39m");
    }

    #[test]
    fn wcag_contrast_known_values() {
        // Белый на чёрном — максимум WCAG: ровно 21.0.
        let white = relative_luminance((255, 255, 255));
        let black = relative_luminance((0, 0, 0));
        assert!((white - 1.0).abs() < 1e-9);
        assert_eq!(black, 0.0);
        assert!((contrast_ratio(white, black) - 21.0).abs() < 1e-9);
        // Одинаковые цвета — ровно 1.0.
        let gray = relative_luminance((128, 128, 128));
        assert!((contrast_ratio(gray, gray) - 1.0).abs() < 1e-9);
        // Порядок аргументов не важен.
        assert_eq!(contrast_ratio(black, white), contrast_ratio(white, black));
    }

    #[test]
    fn contrast_check_flags_dim_gray_on_white() {
        let theme = Theme::new("t")
            .with_role(ThemeRole::PopupBg, ColorSpec::Rgb(255, 255, 255))
            .with_role(ThemeRole::AgentText, ColorSpec::Rgb(200, 200, 200))
            .with_role(ThemeRole::Error, ColorSpec::Rgb(0, 0, 0))
            .with_role(ThemeRole::Dim, ColorSpec::Default);
        let flagged = contrast_check(&theme);
        // Светло-серый текст на белом фоне — единственный нарушитель.
        assert_eq!(flagged.len(), 1);
        let (role, ratio) = flagged[0];
        assert_eq!(role, ThemeRole::AgentText);
        assert!(
            (1.5..=1.8).contains(&ratio),
            "ожидался коэффициент около 1.67, получено {ratio}"
        );
        // Чёрный текст контрастен, Default пропущен, PopupBg не проверяется.
        assert!(!flagged.iter().any(|(r, _)| *r == ThemeRole::Error));
        assert!(!flagged.iter().any(|(r, _)| *r == ThemeRole::Dim));
        assert!(!flagged.iter().any(|(r, _)| *r == ThemeRole::PopupBg));
    }

    #[test]
    fn contrast_check_defaults_to_black_background() {
        // Тема без PopupBg: фон считается чёрным.
        let theme = Theme::new("t").with_role(ThemeRole::AgentText, ColorSpec::Ansi(Color16::Black));
        let flagged = contrast_check(&theme);
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].0, ThemeRole::AgentText);
        assert!((flagged[0].1 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn theme_error_display_is_informative() {
        let err = ThemeError::BadColorSpec {
            role: "accent".to_string(),
            value: "#xyz".to_string(),
        };
        let text = format!("{err}");
        assert!(text.contains("accent"), "в сообщении нет роли: {text}");
        assert!(text.contains("#xyz"), "в сообщении нет значения: {text}");
        assert_eq!(
            ThemeError::UnknownRole("q".to_string()).to_string(),
            "неизвестная роль темы: `q`"
        );
    }
}
