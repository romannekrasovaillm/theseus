//! Раскладка клавиш TUI (по образцу keybinding-слоёв crossterm у CLI-троицы).
//!
//! Привязки «комбинация клавиш → действие» описаны собственными типами,
//! без зависимости от crossterm: [`KeyStroke`] — клавиша [`KeyId`] плюс
//! модификаторы [`ModSet`], [`Action`] — фиксированный набор действий
//! строки ввода агента, [`KeyMap`] — отсортированная таблица привязок
//! с загрузкой из TOML и поиском терминальных конфликтов.
//!
//! ## Текстовая нотация
//!
//! Комбинация записывается как `Модификатор+Клавиша`; регистр и пробелы
//! вокруг токенов не значимы (`ctrl + r` == `Ctrl+R`):
//!
//! - модификаторы: `ctrl`/`control`/`c`, `alt`/`meta`/`a`, `shift`/`s`;
//! - клавиша — последний токен: одиночный символ (`r`, `7`), именованная
//!   (`enter`/`return`, `esc`/`escape`, `tab`, `backspace`/`bs`, `delete`/`del`,
//!   `insert`/`ins`, `home`, `end`, `pageup`/`pgup`, `pagedown`/`pgdn`,
//!   `up`, `down`, `left`, `right`, `space`, `plus`) или `f1`..=`f24`;
//! - токены до последнего обязаны быть модификаторами, поэтому `c` —
//!   одновременно буква и синоним `ctrl`: в `c+r` это модификатор,
//!   а сама буква записывается просто `c`.
//!
//! ## Терминальные эквивалентности и конфликты
//!
//! Классический терминал (без kitty-протокола) не различает часть
//! комбинаций: `Ctrl+I` приходит как `Tab`, `Ctrl+M` — как `Enter`,
//! `Ctrl+H` — как `Backspace`, `Ctrl+[` — как `Esc`, а `Shift` с буквой
//! приходит символом верхнего регистра. [`KeyStroke::terminal_form`]
//! приводит комбинацию к тому виду, в каком её реально доставит
//! терминал; [`KeyMap::conflicts`] находит группы привязок, схлопывающиеся
//! в одну терминальную форму с разными действиями.
//!
//! ## Конфиг
//!
//! Раскладка загружается из TOML-таблицы вида `"клавиша" = "действие"`
//! (`"Ctrl+C" = "interrupt"`). Пример использования:
//!
//! ```
//! use theseus::keymap::{parse_keystroke, Action, KeyMap};
//!
//! let map = KeyMap::default_map();
//! let stroke = parse_keystroke("Ctrl+C").unwrap();
//! assert_eq!(map.binding_for(stroke), Some(Action::Interrupt));
//! ```

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Набор модификаторов комбинации клавиш (битовая маска Ctrl/Alt/Shift).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ModSet(u8);

impl ModSet {
    /// Пустой набор (без модификаторов).
    pub const NONE: Self = Self(0);
    /// Модификатор Ctrl.
    pub const CTRL: Self = Self(1);
    /// Модификатор Alt (Meta).
    pub const ALT: Self = Self(2);
    /// Модификатор Shift.
    pub const SHIFT: Self = Self(4);

    /// `true`, если набор пуст.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// `true`, если все модификаторы `other` входят в набор.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Объединение наборов.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// `true`, если в наборе есть Ctrl.
    pub const fn ctrl(self) -> bool {
        self.contains(Self::CTRL)
    }

    /// `true`, если в наборе есть Alt.
    pub const fn alt(self) -> bool {
        self.contains(Self::ALT)
    }

    /// `true`, если в наборе есть Shift.
    pub const fn shift(self) -> bool {
        self.contains(Self::SHIFT)
    }
}

impl fmt::Display for ModSet {
    /// Модификаторы через `+` в каноническом порядке Ctrl, Alt, Shift
    /// (хвостовой `+` при необходимости добавляет [`KeyStroke`]).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (flag, name) in [(Self::CTRL, "Ctrl"), (Self::ALT, "Alt"), (Self::SHIFT, "Shift")] {
            if self.contains(flag) {
                if !first {
                    f.write_str("+")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

/// Идентификатор клавиши без учёта модификаторов.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyId {
    /// Печатный символ; буквы нормализуются парсером к нижнему регистру.
    Char(char),
    /// Ввод (Return).
    Enter,
    /// Escape.
    Esc,
    /// Табуляция.
    Tab,
    /// Забой.
    Backspace,
    /// Удаление символа под курсором.
    Delete,
    /// Переключение вставки/замены.
    Insert,
    /// В начало строки.
    Home,
    /// В конец строки.
    End,
    /// Страница вверх.
    PageUp,
    /// Страница вниз.
    PageDown,
    /// Стрелка вверх.
    Up,
    /// Стрелка вниз.
    Down,
    /// Стрелка влево.
    Left,
    /// Стрелка вправо.
    Right,
    /// Функциональная клавиша `F1`..=`F24`.
    F(u8),
}

impl fmt::Display for KeyId {
    /// Каноническая запись клавиши; пробел и `+` записываются словами
    /// (`Space`, `Plus`), чтобы строка оставалась разбираемой парсером.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyId::Char(' ') => f.write_str("Space"),
            KeyId::Char('+') => f.write_str("Plus"),
            KeyId::Char(c) => write!(f, "{c}"),
            KeyId::Enter => f.write_str("Enter"),
            KeyId::Esc => f.write_str("Esc"),
            KeyId::Tab => f.write_str("Tab"),
            KeyId::Backspace => f.write_str("Backspace"),
            KeyId::Delete => f.write_str("Delete"),
            KeyId::Insert => f.write_str("Insert"),
            KeyId::Home => f.write_str("Home"),
            KeyId::End => f.write_str("End"),
            KeyId::PageUp => f.write_str("PageUp"),
            KeyId::PageDown => f.write_str("PageDown"),
            KeyId::Up => f.write_str("Up"),
            KeyId::Down => f.write_str("Down"),
            KeyId::Left => f.write_str("Left"),
            KeyId::Right => f.write_str("Right"),
            KeyId::F(n) => write!(f, "F{n}"),
        }
    }
}

/// Комбинация клавиши: сама клавиша плюс модификаторы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyStroke {
    /// Клавиша.
    pub key: KeyId,
    /// Модификаторы.
    pub mods: ModSet,
}

impl KeyStroke {
    /// Комбинация из клавиши и набора модификаторов.
    pub const fn new(key: KeyId, mods: ModSet) -> Self {
        Self { key, mods }
    }

    /// Клавиша без модификаторов.
    pub const fn plain(key: KeyId) -> Self {
        Self::new(key, ModSet::NONE)
    }

    /// Терминальная каноническая форма: то, что реально доставит
    /// классический терминал (без kitty-протокола).
    ///
    /// `Ctrl+I`/`Ctrl+M`/`Ctrl+H`/`Ctrl+[` — те же control-байты, что
    /// `Tab`/`Enter`/`Backspace`/`Esc` (Shift теряется; Alt идёт отдельным
    /// ESC-префиксом, поэтому при Alt свёртка не делается), а `Shift`+буква
    /// приходит символом верхнего регистра без Shift.
    pub fn terminal_form(self) -> KeyStroke {
        if let KeyId::Char(c) = self.key {
            if self.mods.ctrl() && !self.mods.alt() {
                let named = match c {
                    'i' => Some(KeyId::Tab),
                    'm' => Some(KeyId::Enter),
                    'h' => Some(KeyId::Backspace),
                    '[' => Some(KeyId::Esc),
                    _ => None,
                };
                if let Some(key) = named {
                    return KeyStroke::plain(key);
                }
            }
            if self.mods.shift() && c.is_alphabetic() {
                let upper = c.to_uppercase().next().unwrap_or(c);
                return KeyStroke::new(KeyId::Char(upper), ModSet(self.mods.0 & !ModSet::SHIFT.0));
            }
        }
        self
    }
}

impl fmt::Display for KeyStroke {
    /// Каноническая запись вида `Ctrl+Alt+Shift+F5`; для значений из парсера `parse(display(s)) == s`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.mods.is_empty() {
            write!(f, "{}+", self.mods)?;
        }
        write!(f, "{}", self.key)
    }
}

impl FromStr for KeyStroke {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_keystroke(s)
    }
}

impl Serialize for KeyStroke {
    /// Сериализуется строковой записью (`"Ctrl+R"`), поэтому [`KeyMap`] — плоская таблица.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for KeyStroke {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyStrokeVisitor;

        impl serde::de::Visitor<'_> for KeyStrokeVisitor {
            type Value = KeyStroke;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("строку вида «Ctrl+R»")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<KeyStroke, E> {
                parse_keystroke(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(KeyStrokeVisitor)
    }
}

/// Ошибка разбора текстовой записи комбинации клавиш.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// Описание причины (на русском).
    pub message: String,
}

impl ParseError {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ParseError {}

/// Синонимы модификаторов: `ctrl`/`control`/`c`, `alt`/`meta`/`a`, `shift`/`s`.
fn modifier_for(token: &str) -> Option<ModSet> {
    Some(match token {
        "ctrl" | "control" | "c" => ModSet::CTRL,
        "alt" | "meta" | "a" => ModSet::ALT,
        "shift" | "s" => ModSet::SHIFT,
        _ => return None,
    })
}

/// Добавляет модификатор из токена к набору; пустой, неизвестный или
/// повторный модификатор — ошибка разбора.
fn add_modifier(mods: ModSet, raw: &str) -> Result<ModSet, ParseError> {
    if raw.is_empty() {
        return Err(ParseError::new("пустой модификатор (два «+» подряд или «+» в начале)"));
    }
    let lower = raw.to_lowercase();
    let Some(flag) = modifier_for(&lower) else {
        return Err(ParseError::new(format!(
            "неизвестный модификатор «{raw}»; допустимы ctrl/control/c, alt/meta/a, shift/s"
        )));
    };
    if mods.contains(flag) {
        return Err(ParseError::new(format!("модификатор «{raw}» повторяется")));
    }
    Ok(mods.union(flag))
}

/// Разбирает `fN` (1..=24); `Ok(None)` — токен вообще не похож на F-клавишу.
fn parse_f_key(token: &str) -> Result<Option<KeyId>, ParseError> {
    let Some(digits) = token.strip_prefix('f') else {
        return Ok(None);
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    let n: u8 = digits
        .parse()
        .map_err(|_| ParseError::new(format!("номер F-клавиши «{token}» вне диапазона 1–24")))?;
    if (1..=24).contains(&n) {
        Ok(Some(KeyId::F(n)))
    } else {
        Err(ParseError::new(format!("F-клавиши бывают только f1–f24, а не «{token}»")))
    }
}

/// Разбирает токен клавиши (последний токен нотации) и собирает комбинацию.
fn parse_key_token(raw: &str, mods: ModSet) -> Result<KeyStroke, ParseError> {
    if raw.is_empty() {
        let message = if mods.is_empty() {
            "пустая комбинация клавиш".to_owned()
        } else {
            format!("пропущена клавиша после модификаторов «{mods}»")
        };
        return Err(ParseError::new(message));
    }
    let lower = raw.to_lowercase();
    // Одиночный печатный символ (буквы — в нижнем регистре). Идёт первым:
    // «c»/«a»/«s» — одновременно буквы и синонимы модификаторов, но
    // одиночный токен — это всегда клавиша (модификатором «c» бывает
    // только в непоследней позиции, как в «c+r»).
    let mut chars = raw.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        let normalized = c.to_lowercase().next().unwrap_or(c);
        return Ok(KeyStroke::new(KeyId::Char(normalized), mods));
    }
    // Многосимвольный модификатор в позиции клавиши — опечатка («r+ctrl»).
    if modifier_for(&lower).is_some() {
        return Err(ParseError::new(format!(
            "«{raw}» — модификатор, а не клавиша; клавиша должна быть последним токеном"
        )));
    }
    let key = match lower.as_str() {
        "enter" | "return" | "cr" => KeyId::Enter,
        "esc" | "escape" => KeyId::Esc,
        "tab" => KeyId::Tab,
        "backspace" | "bs" => KeyId::Backspace,
        "delete" | "del" => KeyId::Delete,
        "insert" | "ins" => KeyId::Insert,
        "home" => KeyId::Home,
        "end" => KeyId::End,
        "pageup" | "pgup" => KeyId::PageUp,
        "pagedown" | "pgdn" => KeyId::PageDown,
        "up" => KeyId::Up,
        "down" => KeyId::Down,
        "left" => KeyId::Left,
        "right" => KeyId::Right,
        "space" => KeyId::Char(' '),
        "plus" => KeyId::Char('+'),
        _ => {
            if let Some(f_key) = parse_f_key(&lower)? {
                return Ok(KeyStroke::new(f_key, mods));
            }
            return Err(ParseError::new(format!(
                "неизвестная клавиша «{raw}» (символ, именованная клавиша или f1–f24)"
            )));
        }
    };
    Ok(KeyStroke::new(key, mods))
}

/// Разбирает комбинацию клавиш из текстовой нотации (`Ctrl+R`, `Alt+Enter`,
/// `Shift+Tab`, `F5`, `Esc`, `Up`); регистр и пробелы вокруг `+` не значимы.
///
/// # Ошибки
/// Пустая строка, неизвестный модификатор/клавиша, модификатор без клавиши
/// или в позиции клавиши, повторный модификатор, F-клавиша вне 1..=24.
pub fn parse_keystroke(input: &str) -> Result<KeyStroke, ParseError> {
    let tokens: Vec<&str> = input.split('+').collect();
    // Последний токен — клавиша, все предыдущие — модификаторы.
    // split по '+' всегда возвращает хотя бы один токен, так что len >= 1.
    let last = tokens.len() - 1;
    let mut mods = ModSet::NONE;
    for token in &tokens[..last] {
        mods = add_modifier(mods, token.trim())?;
    }
    parse_key_token(tokens[last].trim(), mods)
}

/// Действие строки ввода агента, к которому привязывается комбинация.
///
/// Serde-представление — snake_case-имя (`"history_prev"`), совпадающее
/// с [`Action::as_str`] / [`Action::from_name`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Отправить введённый промпт.
    Submit,
    /// Отменить текущее редактирование или диалог.
    Cancel,
    /// Прервать выполнение агента.
    Interrupt,
    /// Предыдущая команда из истории.
    HistoryPrev,
    /// Следующая команда из истории.
    HistoryNext,
    /// Автодополнение (команда, путь, слэш-команда).
    Complete,
    /// Прокрутка транскрипта вверх.
    ScrollUp,
    /// Прокрутка транскрипта вниз.
    ScrollDown,
    /// Перевод строки без отправки промпта.
    NewLine,
    /// Выйти из харнесса.
    Quit,
}

impl Action {
    /// Все действия (для проверок полноты раскладки).
    pub const ALL: [Action; 10] = [
        Action::Submit, Action::Cancel, Action::Interrupt, Action::HistoryPrev, Action::HistoryNext,
        Action::Complete, Action::ScrollUp, Action::ScrollDown, Action::NewLine, Action::Quit,
    ];

    /// Каноническое snake_case-имя (совпадает с serde-представлением).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Action::Submit => "submit",
            Action::Cancel => "cancel",
            Action::Interrupt => "interrupt",
            Action::HistoryPrev => "history_prev",
            Action::HistoryNext => "history_next",
            Action::Complete => "complete",
            Action::ScrollUp => "scroll_up",
            Action::ScrollDown => "scroll_down",
            Action::NewLine => "new_line",
            Action::Quit => "quit",
        }
    }

    /// Разбор snake_case-имени (как в конфиге и в serde).
    pub fn from_name(name: &str) -> Option<Action> {
        Some(match name {
            "submit" => Action::Submit,
            "cancel" => Action::Cancel,
            "interrupt" => Action::Interrupt,
            "history_prev" => Action::HistoryPrev,
            "history_next" => Action::HistoryNext,
            "complete" => Action::Complete,
            "scroll_up" => Action::ScrollUp,
            "scroll_down" => Action::ScrollDown,
            "new_line" => Action::NewLine,
            "quit" => Action::Quit,
            _ => return None,
        })
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Ошибка загрузки раскладки из TOML-конфига.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapError {
    /// Номер строки в исходном TOML, если известен: `toml::Value` не хранит
    /// позиций (span), поэтому при разборе через [`KeyMap::from_toml`] — всегда `None`.
    pub line: Option<usize>,
    /// Описание проблемы (на русском).
    pub message: String,
}

impl KeymapError {
    fn new(message: impl Into<String>) -> Self {
        Self { line: None, message: message.into() }
    }
}

impl fmt::Display for KeymapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "строка {line}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for KeymapError {}

/// Таблица привязок «комбинация → действие», отсортированная по комбинации.
/// Serde-представление — сама таблица (`"Ctrl+R" = "submit"`), без обёрток;
/// `Default` — пустая раскладка (встроенные привязки даёт [`KeyMap::default_map`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyMap {
    bindings: BTreeMap<KeyStroke, Action>,
}

impl KeyMap {
    /// Раскладка по умолчанию для строки ввода агента: `Enter` — отправка,
    /// `Esc` — отмена, `Ctrl+C` — прерывание, `Up`/`Down` — история,
    /// `Tab` — дополнение, `PageUp`/`PageDown` — прокрутка, `Alt+Enter` —
    /// новая строка, `Ctrl+Q` — выход. Конфликтов нет.
    pub fn default_map() -> Self {
        let pairs = [
            (KeyStroke::plain(KeyId::Enter), Action::Submit),
            (KeyStroke::plain(KeyId::Esc), Action::Cancel),
            (KeyStroke::new(KeyId::Char('c'), ModSet::CTRL), Action::Interrupt),
            (KeyStroke::plain(KeyId::Up), Action::HistoryPrev),
            (KeyStroke::plain(KeyId::Down), Action::HistoryNext),
            (KeyStroke::plain(KeyId::Tab), Action::Complete),
            (KeyStroke::plain(KeyId::PageUp), Action::ScrollUp),
            (KeyStroke::plain(KeyId::PageDown), Action::ScrollDown),
            (KeyStroke::new(KeyId::Enter, ModSet::ALT), Action::NewLine),
            (KeyStroke::new(KeyId::Char('q'), ModSet::CTRL), Action::Quit),
        ];
        Self { bindings: pairs.into_iter().collect() }
    }

    /// Разбор раскладки из TOML-таблицы вида `"клавиша" = "действие"`.
    ///
    /// Раскладка собирается «с нуля»; слоистость (дефолт плюс переопределения
    /// пользователя) достигается снаружи: взять [`KeyMap::default_map`] и
    /// дописать переопределения через [`KeyMap::insert`]. Разбор не глохнет
    /// на первой проблеме: все битые записи (неизвестная клавиша или действие,
    /// нестроковое значение, дубликат) собираются в один список ошибок.
    pub fn from_toml(value: &toml::Value) -> Result<Self, Vec<KeymapError>> {
        let mut errors = Vec::new();
        let mut bindings = BTreeMap::new();

        let Some(table) = value.as_table() else {
            return Err(vec![KeymapError::new(
                "ожидалась TOML-таблица привязок «\"клавиша\" = \"действие\"»",
            )]);
        };

        for (key_text, action_value) in table {
            let stroke = match parse_keystroke(key_text) {
                Ok(stroke) => stroke,
                Err(err) => {
                    errors.push(KeymapError::new(format!("ключ «{key_text}»: {err}")));
                    continue;
                }
            };
            let Some(action_name) = action_value.as_str() else {
                errors.push(KeymapError::new(format!("привязка «{key_text}»: действие должно быть строкой")));
                continue;
            };
            let Some(action) = Action::from_name(action_name) else {
                let known = Action::ALL.iter().map(Action::as_str).collect::<Vec<_>>().join(", ");
                errors.push(KeymapError::new(format!(
                    "привязка «{key_text}»: неизвестное действие «{action_name}»; допустимые: {known}"
                )));
                continue;
            };
            if let Some(prev) = bindings.insert(stroke, action) {
                errors.push(KeymapError::new(format!("«{stroke}» привязана дважды: «{prev}» и «{action}»")));
            }
        }

        if errors.is_empty() { Ok(Self { bindings }) } else { Err(errors) }
    }

    /// Действие, привязанное к комбинации. Сравнение точное: искать по тому
    /// виду, в котором комбинацию доставил терминал ([`KeyStroke::terminal_form`]);
    /// неоднозначности заранее отсекаются через [`KeyMap::conflicts`].
    pub fn binding_for(&self, stroke: KeyStroke) -> Option<Action> {
        self.bindings.get(&stroke).copied()
    }

    /// Добавляет или перезаписывает привязку; возвращает вытесненное действие.
    pub fn insert(&mut self, stroke: KeyStroke, action: Action) -> Option<Action> {
        self.bindings.insert(stroke, action)
    }

    /// Итератор по привязкам в порядке комбинаций.
    pub fn iter(&self) -> impl Iterator<Item = (KeyStroke, Action)> + '_ {
        self.bindings.iter().map(|(stroke, action)| (*stroke, *action))
    }

    /// Конфликты терминальных эквивалентностей: группы комбинаций, которые
    /// реальный терминал не различает ([`KeyStroke::terminal_form`] одна),
    /// а действия назначены разные. Возвращает пары `(каноническая комбинация,
    /// действия)`, отсортированные по комбинации; действия в группе уникальны.
    /// Две комбинации на одно и то же действие конфликтом не считаются.
    pub fn conflicts(&self) -> Vec<(KeyStroke, Vec<Action>)> {
        let mut by_form: BTreeMap<KeyStroke, BTreeSet<Action>> = BTreeMap::new();
        for (stroke, action) in &self.bindings {
            by_form.entry(stroke.terminal_form()).or_default().insert(*action);
        }
        by_form
            .into_iter()
            .filter(|(_, actions)| actions.len() > 1)
            .map(|(form, actions)| (form, actions.into_iter().collect()))
            .collect()
    }
}

impl Serialize for KeyMap {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.bindings.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for KeyMap {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bindings = BTreeMap::<KeyStroke, Action>::deserialize(deserializer)?;
        Ok(Self { bindings })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Короткий помощник: разобрать комбинацию, которая обязана быть валидной.
    fn stroke(text: &str) -> KeyStroke {
        parse_keystroke(text).unwrap()
    }

    #[test]
    fn parse_single_char_and_spec_examples() {
        assert_eq!(stroke("r"), KeyStroke::plain(KeyId::Char('r')));
        assert_eq!(stroke("R"), KeyStroke::plain(KeyId::Char('r')));
        assert_eq!(stroke("7"), KeyStroke::plain(KeyId::Char('7')));
        assert_eq!(stroke(" c "), KeyStroke::plain(KeyId::Char('c')));
        // Примеры из постановки задачи.
        assert_eq!(stroke("Ctrl+R"), KeyStroke::new(KeyId::Char('r'), ModSet::CTRL));
        assert_eq!(stroke("Alt+Enter"), KeyStroke::new(KeyId::Enter, ModSet::ALT));
        assert_eq!(stroke("Shift+Tab"), KeyStroke::new(KeyId::Tab, ModSet::SHIFT));
        assert_eq!(stroke("F5"), KeyStroke::plain(KeyId::F(5)));
        assert_eq!(stroke("Esc"), KeyStroke::plain(KeyId::Esc));
        assert_eq!(stroke("Up"), KeyStroke::plain(KeyId::Up));
    }

    #[test]
    fn parse_modifiers_synonyms_case_and_spaces() {
        let expected = KeyStroke::new(KeyId::Char('r'), ModSet::CTRL);
        let forms = [
            "Ctrl+R", "ctrl+r", "CONTROL+R", "cTrL+R", "control+r", "Control+R", "c+r", "C+R",
            " ctrl + r ", "Ctrl  +  R",
        ];
        for text in forms {
            assert_eq!(stroke(text), expected, "вариант: {text}");
        }
        let alt = KeyStroke::new(KeyId::Char('r'), ModSet::ALT);
        assert_eq!(stroke("Alt+R"), alt);
        assert_eq!(stroke("a+r"), alt);
        assert_eq!(stroke("Meta+R"), alt);
        assert_eq!(stroke("s+r"), KeyStroke::new(KeyId::Char('r'), ModSet::SHIFT));
        // Несколько модификаторов, порядок в записи не важен.
        let all = ModSet::CTRL.union(ModSet::ALT).union(ModSet::SHIFT);
        assert_eq!(stroke("Ctrl+Alt+Shift+x"), KeyStroke::new(KeyId::Char('x'), all));
        assert_eq!(stroke("shift+ctrl+x"), stroke("Ctrl+Shift+X"));
        assert!(!stroke("x").mods.contains(ModSet::CTRL));
    }

    #[test]
    fn parse_named_keys_and_aliases() {
        let cases: [(&str, KeyId); 23] = [
            ("enter", KeyId::Enter), ("return", KeyId::Enter),
            ("esc", KeyId::Esc), ("escape", KeyId::Esc),
            ("tab", KeyId::Tab), ("backspace", KeyId::Backspace),
            ("bs", KeyId::Backspace), ("delete", KeyId::Delete),
            ("del", KeyId::Delete), ("insert", KeyId::Insert),
            ("ins", KeyId::Insert), ("home", KeyId::Home),
            ("end", KeyId::End), ("pageup", KeyId::PageUp),
            ("pgup", KeyId::PageUp), ("pagedown", KeyId::PageDown),
            ("pgdn", KeyId::PageDown), ("up", KeyId::Up),
            ("down", KeyId::Down), ("left", KeyId::Left),
            ("right", KeyId::Right), ("space", KeyId::Char(' ')),
            ("plus", KeyId::Char('+')),
        ];
        for (text, key) in cases {
            assert_eq!(stroke(text), KeyStroke::plain(key), "вариант: {text}");
        }
    }

    #[test]
    fn parse_function_keys_range() {
        assert_eq!(stroke("F1"), KeyStroke::plain(KeyId::F(1)));
        assert_eq!(stroke("f5"), KeyStroke::plain(KeyId::F(5)));
        assert_eq!(stroke("F12"), KeyStroke::plain(KeyId::F(12)));
        assert_eq!(stroke("Ctrl+F24"), KeyStroke::new(KeyId::F(24), ModSet::CTRL));
        for bad in ["F0", "F25", "F100", "F999", "ff", "f-1"] {
            assert!(parse_keystroke(bad).is_err(), "вариант: {bad}");
        }
    }

    #[test]
    fn parse_errors_are_reported() {
        let forms = [
            "", "   ", "Ctrl+", "+R", "Ctrl++R", "Ctrl", "alt", "Ctrl+Ctrl+R", "ctrl+c+r",
            "R+Ctrl", "Ctrl+Wat", "Wat+R",
        ];
        for bad in forms {
            assert!(parse_keystroke(bad).is_err(), "вариант: {bad:?}");
        }
        let err = parse_keystroke("Ctrl+Wat").unwrap_err();
        assert!(err.message.contains("Wat"), "сообщение: {err}");
        let err = parse_keystroke("R+Ctrl").unwrap_err();
        assert!(err.message.contains("модификатор"), "сообщение: {err}");
        let err = parse_keystroke("ctrl+c+r").unwrap_err();
        assert!(err.message.contains("повторяется"), "сообщение: {err}");
    }

    #[test]
    fn display_roundtrips_through_parse() {
        let strokes = [
            stroke("r"), stroke("Ctrl+R"), stroke("Alt+Enter"), stroke("Shift+Tab"),
            stroke("Ctrl+Alt+Shift+F1"), stroke("space"), stroke("plus"), stroke("F24"),
            stroke("Ctrl+Shift+PageDown"),
        ];
        for original in strokes {
            let text = original.to_string();
            let back = parse_keystroke(&text).unwrap_or_else(|err| panic!("«{text}»: {err}"));
            assert_eq!(back, original, "текстовая форма: {text}");
        }
        assert_eq!(stroke("Ctrl+Alt+F5").to_string(), "Ctrl+Alt+F5");
        assert_eq!(KeyStroke::plain(KeyId::Char('+')).to_string(), "Plus");
        assert_eq!(KeyStroke::plain(KeyId::Char(' ')).to_string(), "Space");
    }

    #[test]
    fn terminal_form_rules() {
        // Классические control-байты: Ctrl+I/M/H/[ == Tab/Enter/Backspace/Esc.
        assert_eq!(stroke("Ctrl+i").terminal_form(), KeyStroke::plain(KeyId::Tab));
        assert_eq!(stroke("Ctrl+M").terminal_form(), KeyStroke::plain(KeyId::Enter));
        assert_eq!(stroke("Ctrl+h").terminal_form(), KeyStroke::plain(KeyId::Backspace));
        assert_eq!(stroke("Ctrl+[").terminal_form(), KeyStroke::plain(KeyId::Esc));
        // Shift теряется в control-байте, а Alt идёт отдельным ESC-префиксом.
        assert_eq!(stroke("Ctrl+Shift+i").terminal_form(), KeyStroke::plain(KeyId::Tab));
        assert_eq!(stroke("Alt+Ctrl+i").terminal_form(), stroke("Alt+Ctrl+i"));
        // Обычные Ctrl-буквы не трогаем.
        assert_eq!(stroke("Ctrl+c").terminal_form(), stroke("Ctrl+c"));
        // Shift+буква — символ верхнего регистра без Shift; Shift+Tab — свой (BackTab).
        assert_eq!(stroke("Shift+r").terminal_form(), KeyStroke::plain(KeyId::Char('R')));
        assert_eq!(stroke("r").terminal_form(), stroke("r"));
        assert_eq!(stroke("Shift+Tab").terminal_form(), stroke("Shift+Tab"));
    }

    #[test]
    fn conflicts_detect_terminal_ambiguity() {
        let mut map = KeyMap::default();
        map.insert(stroke("Tab"), Action::Complete);
        map.insert(stroke("Ctrl+I"), Action::Interrupt);
        map.insert(stroke("Enter"), Action::Submit);
        let conflicts = map.conflicts();
        assert_eq!(conflicts.len(), 1);
        let (form, actions) = &conflicts[0];
        assert_eq!(*form, KeyStroke::plain(KeyId::Tab));
        // Порядок действий — по вариантам перечисления (Interrupt < Complete).
        assert_eq!(actions.as_slice(), &[Action::Interrupt, Action::Complete]);
        // Две комбинации на одно действие конфликтом не считаются.
        let mut map = KeyMap::default();
        map.insert(stroke("Tab"), Action::Complete);
        map.insert(stroke("Ctrl+I"), Action::Complete);
        assert!(map.conflicts().is_empty());
    }

    #[test]
    fn default_map_covers_every_action_without_conflicts() {
        let map = KeyMap::default_map();
        for action in Action::ALL {
            assert!(map.iter().any(|(_, bound)| bound == action), "нет привязки: {action}");
        }
        assert!(map.conflicts().is_empty());
        assert_eq!(map.iter().count(), Action::ALL.len());
    }

    #[test]
    fn from_toml_valid_config() {
        let doc = r#"
            "Enter" = "submit"
            "ctrl+c" = "interrupt"
            "Alt+Enter" = "new_line"
        "#;
        let value: toml::Value = toml::from_str(doc).unwrap();
        let map = KeyMap::from_toml(&value).unwrap();
        assert_eq!(map.iter().count(), 3);
        assert_eq!(map.binding_for(stroke("Enter")), Some(Action::Submit));
        assert_eq!(map.binding_for(stroke("Ctrl+C")), Some(Action::Interrupt));
        assert_eq!(map.binding_for(stroke("Alt+Enter")), Some(Action::NewLine));
        assert_eq!(map.binding_for(stroke("Esc")), None);
    }

    #[test]
    fn from_toml_collects_all_syntax_errors() {
        let doc = r#"
            "Ctrl+" = "submit"
            "Enter" = "unknown_action"
            "Tab" = 42
            "Ctrl+C" = "interrupt"
            "control+c" = "cancel"
        "#;
        let value: toml::Value = toml::from_str(doc).unwrap();
        let errors = KeyMap::from_toml(&value).unwrap_err();
        assert_eq!(errors.len(), 4, "ошибки: {errors:?}");
        assert!(errors[0].message.contains("Ctrl+"), "сообщение: {}", errors[0]);
        assert!(errors.iter().any(|e| e.message.contains("unknown_action")));
        assert!(errors.iter().any(|e| e.message.contains("строкой")));
        assert!(errors.iter().any(|e| e.message.contains("дважды")));
        // Позиции в исходном тексте из toml::Value не восстановить.
        assert!(errors.iter().all(|e| e.line.is_none()));
        // Не-таблица на входе — единственная ошибка.
        let errors = KeyMap::from_toml(&toml::Value::Integer(1)).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("таблица"));
    }

    #[test]
    fn action_names_match_serde_names() {
        for action in Action::ALL {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(json, format!("\"{}\"", action.as_str()));
            assert_eq!(Action::from_name(action.as_str()), Some(action));
            assert_eq!(action.to_string(), action.as_str());
        }
        assert_eq!(Action::from_name("nope"), None);
    }

    #[test]
    fn binding_for_lookup_and_insert_layering() {
        let mut map = KeyMap::default_map();
        assert_eq!(map.binding_for(stroke("Enter")), Some(Action::Submit));
        // Пользовательский слой перекрывает дефолтную привязку.
        let prev = map.insert(stroke("Enter"), Action::NewLine);
        assert_eq!(prev, Some(Action::Submit));
        assert_eq!(map.binding_for(stroke("Enter")), Some(Action::NewLine));
        // Свежая комбинация ничего не вытесняет.
        assert_eq!(map.insert(stroke("Ctrl+B"), Action::Cancel), None);
    }

    #[test]
    fn keystroke_serde_uses_string_form() {
        let original = stroke("Ctrl+Alt+F5");
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(json, "\"Ctrl+Alt+F5\"");
        let back: KeyStroke = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
        assert!(serde_json::from_str::<KeyStroke>("\"Ctrl+\"").is_err());
        // FromStr делегирует тому же парсеру.
        let via_trait: KeyStroke = "Ctrl+R".parse().unwrap();
        assert_eq!(via_trait, parse_keystroke("Ctrl+R").unwrap());
        assert!("Ctrl+".parse::<KeyStroke>().is_err());
    }

    #[test]
    fn keymap_serde_roundtrips() {
        let map = KeyMap::default_map();
        // JSON.
        let json = serde_json::to_string_pretty(&map).unwrap();
        let back: KeyMap = serde_json::from_str(&json).unwrap();
        assert_eq!(map, back);
        // TOML напрямую через serde.
        let text = toml::to_string(&map).unwrap();
        let back: KeyMap = toml::from_str(&text).unwrap();
        assert_eq!(map, back);
        // Сериализованная serde-форма — это ровно формат from_toml.
        let value: toml::Value = toml::from_str(&text).unwrap();
        let back = KeyMap::from_toml(&value).unwrap();
        assert_eq!(map, back);
    }
}
