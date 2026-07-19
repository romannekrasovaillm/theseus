//! Минимальный SemVer для theseus: проверка обновлений и MSRV.
//!
//! Харнессу версии нужны в двух сценариях:
//!
//! * **update_check** — сравнение «текущая версия vs версия на сервере»
//!   и проверка совместимости плагинов/скиллов с ядром (`^1.2`, `~1.2.3`);
//! * **MSRV-проверки** — «тулчейн не старше `rust-version` из
//!   `Cargo.toml`» сводится к [`Version::is_compatible_with`] с
//!   требованием `">=1.85"`.
//!
//! Возможности сознательно минимальные (неполный SemVer 2.0.0):
//!
//! * разбор `1.2.3`, `v1.2.3`, `1.2` (→ `1.2.0`), предрелизы `1.2.3-rc.1`;
//!   ведущие и хвостовые пробелы игнорируются;
//! * build-метаданные (`1.2.3+build`) отклоняются: [`Version`] их не
//!   хранит, а молчаливое отбрасывание сломало бы инвариант roundtrip
//!   «`Display` → `parse` — тождественное отображение»;
//! * порядок по semver.org: предрелиз младше релиза, числовые сегменты
//!   предрелиза сравниваются как числа, числовой сегмент младше
//!   строкового, при равном префиксе короткий список сегментов младше
//!   длинного;
//! * требования совместимости: `^`, `~`, `>=`, `>`, `<=`, `<`, `=` и
//!   голая версия (точное совпадение). В отличие от cargo, предрелизы
//!   из диапазонов не отфильтровываются — чистый порядок версий.
//!
//! Модуль зависит только от `std` и не паникует: все ошибки разбора —
//! через [`ParseError`], переполнение при инкременте версии насыщается
//! (`saturating_add`).

use std::cmp::Ordering;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

// ---------------------------------------------------------------------------
// Ошибки разбора
// ---------------------------------------------------------------------------

/// Ошибка разбора версии или требования совместимости.
///
/// Все варианты несут достаточно контекста для человекочитаемой
/// диагностики: какой компонент не разобран и почему. Сообщения —
/// по-русски, как и весь пользовательский вывод харнесса.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Пустая строка (возможно, после обрезки пробелов или префикса `v`).
    Empty,
    /// Встречен `+`: build-метаданные в [`Version`] не представимы.
    BuildMetadataUnsupported,
    /// Ядро версии содержит не 2–3 числовых компонента.
    CoreComponents {
        /// Сколько компонентов получено на самом деле.
        found: usize,
    },
    /// Пустой числовой компонент ядра (`1..2`, `.2.3`).
    EmptyCoreComponent,
    /// Числовой компонент содержит не-цифры.
    InvalidNumber {
        /// Исходный текст компонента.
        component: String,
    },
    /// Ведущий нуль в числовом компоненте (`01.2.3`) — запрещён SemVer.
    LeadingZero {
        /// Исходный текст компонента.
        component: String,
    },
    /// Числовой компонент не помещается в `u64`.
    NumberOverflow {
        /// Исходный текст компонента.
        component: String,
    },
    /// Дефис есть, а предрелизный суффикс пуст (`1.2.3-`).
    EmptyPreRelease,
    /// Пустой сегмент предрелиза (`1.2.3-rc..1`).
    EmptyPreSegment,
    /// Недопустимый символ в сегменте предрелиза (нужны `[0-9A-Za-z-]`).
    InvalidPreChar {
        /// Исходный текст сегмента.
        segment: String,
    },
    /// Ведущий нуль в числовом сегменте предрелиза (`1.2.3-rc.01`).
    PreLeadingZero {
        /// Исходный текст сегмента.
        segment: String,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "пустая строка версии"),
            Self::BuildMetadataUnsupported => {
                write!(f, "build-метаданные (`+...`) не поддерживаются")
            }
            Self::CoreComponents { found } => write!(
                f,
                "ядро версии: ожидалось 2–3 компонента (major.minor[.patch]), получено {found}"
            ),
            Self::EmptyCoreComponent => {
                write!(f, "пустой числовой компонент ядра версии")
            }
            Self::InvalidNumber { component } => {
                write!(f, "«{component}» — не число (нужны ASCII-цифры)")
            }
            Self::LeadingZero { component } => {
                write!(f, "«{component}»: ведущий нуль запрещён SemVer")
            }
            Self::NumberOverflow { component } => {
                write!(f, "«{component}»: число не помещается в u64")
            }
            Self::EmptyPreRelease => {
                write!(f, "пустой предрелизный суффикс после `-`")
            }
            Self::EmptyPreSegment => write!(f, "пустой сегмент предрелиза"),
            Self::InvalidPreChar { segment } => {
                write!(f, "«{segment}»: недопустимый символ (нужны [0-9A-Za-z-])")
            }
            Self::PreLeadingZero { segment } => {
                write!(f, "«{segment}»: ведущий нуль в числовом сегменте предрелиза")
            }
        }
    }
}

impl Error for ParseError {}

// ---------------------------------------------------------------------------
// Версия
// ---------------------------------------------------------------------------

/// Версия по SemVer: `major.minor.patch[-pre]`.
///
/// Build-метаданных нет (см. документацию модуля): инвариант
/// «`parse` → `Display` → `parse` тождественен» всегда выполняется.
///
/// Поля публичные: версия — прозрачная структура-значение, инвариант
/// (корректность сегментов `pre`) поддерживается конструкторами
/// [`Version::new`] и [`Version::parse`]; ручная сборка через литерал
/// структуры — на совести вызывающего.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// Мажорный компонент: ломающие изменения.
    pub major: u64,
    /// Минорный компонент: обратно-совместимая функциональность.
    pub minor: u64,
    /// Патч-компонент: обратно-совместимые исправления.
    pub patch: u64,
    /// Сегменты предрелиза (например, `["rc", "1"]` для `1.2.3-rc.1`);
    /// пустой вектор — стабильный релиз.
    pub pre: Vec<String>,
}

impl Version {
    /// Создать стабильную версию `major.minor.patch` (без предрелиза).
    #[must_use]
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            pre: Vec::new(),
        }
    }

    /// Разобрать версию из строки.
    ///
    /// Поддерживаемые формы: `1.2.3`, `v1.2.3` (и `V`), `1.2`
    /// (→ `1.2.0`), `1.2.3-rc.1` (сегменты предрелиза — `[0-9A-Za-z-]`,
    /// числовые — без ведущих нулей). Ведущие и хвостовые пробелы
    /// игнорируются. Build-метаданные (`+...`) отклоняются ошибкой
    /// [`ParseError::BuildMetadataUnsupported`].
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let pv = parse_partial(input)?;
        let minor = pv.minor.ok_or(ParseError::CoreComponents { found: 1 })?;
        Ok(Self {
            major: pv.major,
            minor,
            patch: pv.patch.unwrap_or(0),
            pre: pv.pre,
        })
    }

    /// `true`, если версия — предрелиз (список сегментов `pre` не пуст).
    #[must_use]
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }

    /// Проверить, удовлетворяет ли версия требованию `requirement`.
    ///
    /// Операторы требования:
    ///
    /// * `^1.2.3` — совместимость по SemVer: `>=1.2.3 <2.0.0`;
    ///   нулевой префикс «сдвигает» границу вправо: `^0.2.3` → `<0.3.0`,
    ///   `^0.0.3` → `<0.0.4`; неуказанные компоненты — «дикая карта»:
    ///   `^1.2` → `<2.0.0`, `^1` → `<2.0.0`;
    /// * `~1.2.3` — патч-диапазон: `>=1.2.3 <1.3.0`; `~1.2` → `<1.3.0`,
    ///   `~1` → `<2.0.0`;
    /// * `>=`, `>`, `<=`, `<`, `=` — обычные сравнения по порядку SemVer;
    /// * без оператора — точное совпадение (после нормализации
    ///   `1.2` → `1.2.0`).
    ///
    /// Неразбираемое требование даёт `false` (паники нет). В отличие от
    /// cargo, предрелизы не отфильтровываются из диапазонов.
    #[must_use]
    pub fn is_compatible_with(&self, requirement: &str) -> bool {
        parse_requirement(requirement).is_ok_and(|req| req.matches(self))
    }

    /// Следующая мажорная версия: `1.2.3-rc.1` → `2.0.0`.
    ///
    /// Младшие компоненты обнуляются, предрелиз сбрасывается;
    /// переполнение `u64` насыщается. Исходная версия не изменяется.
    #[must_use]
    pub fn bump_major(&self) -> Self {
        Self {
            major: self.major.saturating_add(1),
            minor: 0,
            patch: 0,
            pre: Vec::new(),
        }
    }

    /// Следующая минорная версия: `1.2.3-rc.1` → `1.3.0`.
    ///
    /// Патч обнуляется, предрелиз сбрасывается; переполнение `u64`
    /// насыщается. Исходная версия не изменяется.
    #[must_use]
    pub fn bump_minor(&self) -> Self {
        Self {
            major: self.major,
            minor: self.minor.saturating_add(1),
            patch: 0,
            pre: Vec::new(),
        }
    }

    /// Следующая патч-версия: `1.2.3-rc.1` → `1.2.4`.
    ///
    /// Предрелиз сбрасывается; переполнение `u64` насыщается.
    /// Исходная версия не изменяется.
    #[must_use]
    pub fn bump_patch(&self) -> Self {
        Self {
            major: self.major,
            minor: self.minor,
            patch: self.patch.saturating_add(1),
            pre: Vec::new(),
        }
    }
}

impl fmt::Display for Version {
    /// Каноническая форма: `major.minor.patch` и, при наличии,
    /// `-pre.segments` через точку. Без префикса `v`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

impl FromStr for Version {
    type Err = ParseError;

    /// То же, что [`Version::parse`]: позволяет писать
    /// `"1.2.3".parse::<Version>()`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| cmp_pre(&self.pre, &other.pre))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// ---------------------------------------------------------------------------
// Разбор строки (внутренние помощники)
// ---------------------------------------------------------------------------

/// Частично разобранная версия: `minor`/`patch` могут отсутствовать
/// (требования вроде `^1` или `~1.2`).
#[derive(Debug)]
struct PartialVersion {
    major: u64,
    minor: Option<u64>,
    patch: Option<u64>,
    pre: Vec<String>,
}

impl PartialVersion {
    /// Достроить до полной версии: отсутствующие компоненты → 0.
    fn to_version(&self) -> Version {
        Version {
            major: self.major,
            minor: self.minor.unwrap_or(0),
            patch: self.patch.unwrap_or(0),
            pre: self.pre.clone(),
        }
    }
}

/// Разобрать (возможно неполную) версию: 1–3 числовых компонента ядра,
/// необязательный префикс `v`/`V`, необязательный предрелиз после `-`.
fn parse_partial(input: &str) -> Result<PartialVersion, ParseError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(ParseError::Empty);
    }
    // Build-метаданные в Version не представимы — отклоняем явно.
    if s.contains('+') {
        return Err(ParseError::BuildMetadataUnsupported);
    }
    let s = s
        .strip_prefix('v')
        .or_else(|| s.strip_prefix('V'))
        .unwrap_or(s);
    if s.is_empty() {
        return Err(ParseError::Empty);
    }
    let (core, pre) = match s.split_once('-') {
        Some((core, pre)) => (core, parse_pre(pre)?),
        None => (s, Vec::new()),
    };
    let mut parts = core.split('.');
    // Ядро не бывает пустым: split('.') всегда отдаёт хотя бы один
    // элемент (возможно, пустую строку — её поймает parse_num).
    let major = parse_num(parts.next().unwrap_or_default())?;
    let minor = parts.next().map(parse_num).transpose()?;
    let patch = parts.next().map(parse_num).transpose()?;
    if parts.next().is_some() {
        // 4+ компонентов: досчитаем остаток ради честной диагностики.
        let found = 4 + parts.count();
        return Err(ParseError::CoreComponents { found });
    }
    Ok(PartialVersion {
        major,
        minor,
        patch,
        pre,
    })
}

/// Разобрать числовой компонент ядра: непустой, только ASCII-цифры,
/// без ведущих нулей, в диапазоне `u64`.
fn parse_num(s: &str) -> Result<u64, ParseError> {
    if s.is_empty() {
        return Err(ParseError::EmptyCoreComponent);
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParseError::InvalidNumber {
            component: s.to_string(),
        });
    }
    if s.len() > 1 && s.starts_with('0') {
        return Err(ParseError::LeadingZero {
            component: s.to_string(),
        });
    }
    // Цифры проверены выше — единственная оставшаяся причина сбоя
    // парсинга: переполнение u64.
    s.parse::<u64>().map_err(|_| ParseError::NumberOverflow {
        component: s.to_string(),
    })
}

/// Разобрать предрелизный суффикс (строку после `-`): непустые сегменты
/// через точку из символов `[0-9A-Za-z-]`; числовые — без ведущих нулей.
fn parse_pre(s: &str) -> Result<Vec<String>, ParseError> {
    if s.is_empty() {
        return Err(ParseError::EmptyPreRelease);
    }
    s.split('.').map(parse_pre_segment).collect()
}

/// Проверить и вернуть один сегмент предрелиза.
fn parse_pre_segment(seg: &str) -> Result<String, ParseError> {
    if seg.is_empty() {
        return Err(ParseError::EmptyPreSegment);
    }
    if !seg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(ParseError::InvalidPreChar {
            segment: seg.to_string(),
        });
    }
    if is_ascii_digits(seg) && seg.len() > 1 && seg.starts_with('0') {
        return Err(ParseError::PreLeadingZero {
            segment: seg.to_string(),
        });
    }
    Ok(seg.to_string())
}

/// Все байты строки — ASCII-цифры (пустая строка → `false`).
fn is_ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// Порядок (внутренние помощники)
// ---------------------------------------------------------------------------

/// Сравнить списки сегментов предрелиза: пустой список (стабильный
/// релиз) СТАРШЕ любого предрелиза; иначе — посегментно, при равном
/// префиксе более короткий список младше.
fn cmp_pre(a: &[String], b: &[String]) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a
            .iter()
            .zip(b.iter())
            .map(|(x, y)| cmp_pre_segment(x, y))
            .find(|ord| *ord != Ordering::Equal)
            .unwrap_or_else(|| a.len().cmp(&b.len())),
    }
}

/// Сравнить два сегмента предрелиза по правилам semver.org: числовой
/// младше строкового; два числовых — как числа; два строковых —
/// лексикографически (ASCII).
fn cmp_pre_segment(a: &str, b: &str) -> Ordering {
    match (is_ascii_digits(a), is_ascii_digits(b)) {
        // Ведущие нули запрещены при разборе, поэтому «больше разрядов —
        // больше число», а при равной длине лексикографика совпадает с
        // числовым порядком. Парсить в u64 не нужно — заодно не грозит
        // переполнение на сегментах длиннее 20 цифр.
        (true, true) => a.len().cmp(&b.len()).then_with(|| a.cmp(b)),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => a.cmp(b),
    }
}

// ---------------------------------------------------------------------------
// Требования совместимости
// ---------------------------------------------------------------------------

/// Оператор требования совместимости.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// `^1.2` — совместимость по SemVer (левый ненулевой компонент
    /// фиксирован).
    Caret,
    /// `~1.2.3` — только патч-обновления.
    Tilde,
    /// `>=1.0`.
    GreaterOrEqual,
    /// `>1.0`.
    Greater,
    /// `<=1.0`.
    LessOrEqual,
    /// `<1.0`.
    Less,
    /// `=1.2.3` или голая версия — точное совпадение.
    Exact,
}

/// Разобранное требование: оператор + (возможно неполная) базовая версия.
#[derive(Debug)]
struct Requirement {
    op: Op,
    base: PartialVersion,
}

impl Requirement {
    /// Удовлетворяет ли версия `v` этому требованию.
    fn matches(&self, v: &Version) -> bool {
        let base = self.base.to_version();
        match self.op {
            Op::Exact => v == &base,
            Op::GreaterOrEqual => v >= &base,
            Op::Greater => v > &base,
            Op::LessOrEqual => v <= &base,
            Op::Less => v < &base,
            Op::Caret => v >= &base && v < &self.caret_upper(),
            Op::Tilde => v >= &base && v < &self.tilde_upper(),
        }
    }

    /// Верхняя граница для `^`: инкремент левого ненулевого компонента
    /// (`^1.2.3` → `<2.0.0`, `^0.2.3` → `<0.3.0`, `^0.0.3` → `<0.0.4`);
    /// если все указанные компоненты нулевые, инкрементируется последний
    /// указанный (`^0.0` → `<0.1.0`, `^0` → `<1.0.0`).
    fn caret_upper(&self) -> Version {
        let (major, minor, patch) = if self.base.major > 0 {
            (self.base.major.saturating_add(1), 0, 0)
        } else if self.base.minor.unwrap_or(0) > 0 {
            (0, self.base.minor.unwrap_or(0).saturating_add(1), 0)
        } else if let Some(patch) = self.base.patch {
            (0, 0, patch.saturating_add(1))
        } else if self.base.minor.is_some() {
            (0, 1, 0)
        } else {
            (1, 0, 0)
        };
        Version {
            major,
            minor,
            patch,
            pre: Vec::new(),
        }
    }

    /// Верхняя граница для `~`: `~1.2.3` → `<1.3.0`, `~1.2` → `<1.3.0`,
    /// `~1` → `<2.0.0`.
    fn tilde_upper(&self) -> Version {
        let (major, minor) = match self.base.minor {
            Some(minor) => (self.base.major, minor.saturating_add(1)),
            None => (self.base.major.saturating_add(1), 0),
        };
        Version {
            major,
            minor,
            patch: 0,
            pre: Vec::new(),
        }
    }
}

/// Разобрать требование вида `^1.2`, `~1.2.3`, `>=1.0`, `=1.2.3` или
/// голой версии (точное совпадение).
fn parse_requirement(input: &str) -> Result<Requirement, ParseError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(ParseError::Empty);
    }
    // Двухсимвольные операторы проверяем раньше односимвольных.
    let (op, rest) = if let Some(r) = s.strip_prefix(">=") {
        (Op::GreaterOrEqual, r)
    } else if let Some(r) = s.strip_prefix("<=") {
        (Op::LessOrEqual, r)
    } else if let Some(r) = s.strip_prefix('^') {
        (Op::Caret, r)
    } else if let Some(r) = s.strip_prefix('~') {
        (Op::Tilde, r)
    } else if let Some(r) = s.strip_prefix('>') {
        (Op::Greater, r)
    } else if let Some(r) = s.strip_prefix('<') {
        (Op::Less, r)
    } else if let Some(r) = s.strip_prefix('=') {
        (Op::Exact, r)
    } else {
        (Op::Exact, s)
    };
    Ok(Requirement {
        op,
        base: parse_partial(rest)?,
    })
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Короткий конструктор для тестов: разбор или паника.
    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    /// Разбор полной версии `major.minor.patch` без предрелиза.
    #[test]
    fn parse_full_version() {
        let ver = v("1.2.3");
        assert_eq!(ver.major, 1);
        assert_eq!(ver.minor, 2);
        assert_eq!(ver.patch, 3);
        assert!(ver.pre.is_empty());
        assert!(!ver.is_prerelease());
        // Конструктор new даёт то же значение.
        assert_eq!(ver, Version::new(1, 2, 3));
    }

    /// Все поддерживаемые формы: префикс `v`/`V`, укороченная запись
    /// `1.2` → `1.2.0`, предрелиз с дефисами внутри сегментов, пробелы
    /// по краям.
    #[test]
    fn parse_supported_forms() {
        assert_eq!(v("v1.2.3"), Version::new(1, 2, 3));
        assert_eq!(v("V10.20.30"), Version::new(10, 20, 30));
        assert_eq!(v("1.2"), Version::new(1, 2, 0));
        assert_eq!(v("0.0"), Version::new(0, 0, 0));
        assert_eq!(v(" 1.2.3 "), Version::new(1, 2, 3));

        let rc = v("1.2.3-rc.1");
        assert_eq!(rc.pre, ["rc", "1"]);
        assert!(rc.is_prerelease());

        // Дефисы внутри сегментов — легальны (пример с semver.org).
        let hyphenated = v("1.0.0-x-y-z.--");
        assert_eq!(hyphenated.pre, ["x-y-z", "--"]);
    }

    /// Восемь классов некорректного ввода (плюс граничные варианты)
    /// отклоняются с конкретным вариантом `ParseError`, без паники.
    #[test]
    fn parse_rejects_invalid_input() {
        let cases: &[(&str, ParseError)] = &[
            // 1. Пустая строка.
            ("", ParseError::Empty),
            ("   ", ParseError::Empty),
            ("v", ParseError::Empty),
            // 2. Неверное число компонентов ядра.
            ("1", ParseError::CoreComponents { found: 1 }),
            ("1.2.3.4", ParseError::CoreComponents { found: 4 }),
            ("1.2.3.4.5", ParseError::CoreComponents { found: 5 }),
            // 3. Пустой компонент ядра.
            ("1..2", ParseError::EmptyCoreComponent),
            (".2.3", ParseError::EmptyCoreComponent),
            ("-rc.1", ParseError::EmptyCoreComponent),
            // 4. Не-цифры в числовом компоненте.
            (
                "a.b.c",
                ParseError::InvalidNumber {
                    component: "a".to_string(),
                },
            ),
            (
                "1.2.x",
                ParseError::InvalidNumber {
                    component: "x".to_string(),
                },
            ),
            // 5. Ведущий нуль.
            (
                "01.2.3",
                ParseError::LeadingZero {
                    component: "01".to_string(),
                },
            ),
            // 6. Переполнение u64 (2^64).
            (
                "18446744073709551616.0.0",
                ParseError::NumberOverflow {
                    component: "18446744073709551616".to_string(),
                },
            ),
            // 7. Build-метаданные не поддерживаются.
            ("1.2.3+build.5", ParseError::BuildMetadataUnsupported),
            ("1.2.3-rc+build", ParseError::BuildMetadataUnsupported),
            // 8. Пустой предрелиз и пустой сегмент предрелиза.
            ("1.2.3-", ParseError::EmptyPreRelease),
            ("1.2.3-rc..1", ParseError::EmptyPreSegment),
            // 9. Недопустимый символ в предрелизе.
            (
                "1.2.3-rc_1",
                ParseError::InvalidPreChar {
                    segment: "rc_1".to_string(),
                },
            ),
            // 10. Ведущий нуль в числовом сегменте предрелиза.
            (
                "1.2.3-rc.01",
                ParseError::PreLeadingZero {
                    segment: "01".to_string(),
                },
            ),
        ];
        assert_eq!(cases.len(), 19);
        for (input, expected) in cases {
            assert_eq!(
                Version::parse(input).unwrap_err(),
                *expected,
                "ввод: «{input}»"
            );
        }
    }

    /// `Display` — точное обратное отображение: канонические формы
    /// воспроизводятся символ в символ; неканонические (`v`-префикс,
    /// укороченная запись, пробелы) нормализуются.
    #[test]
    fn display_roundtrip() {
        let canonical = [
            "0.0.0",
            "1.2.3",
            "10.20.30",
            "1.0.0-rc.1",
            "1.0.0-x-y-z.--",
        ];
        for s in canonical {
            let ver = v(s);
            assert_eq!(ver.to_string(), s);
            // Повторный разбор строки даёт ту же версию (roundtrip).
            assert_eq!(v(&ver.to_string()), ver);
        }

        let normalized = [
            ("v1.2.3", "1.2.3"),
            ("1.2", "1.2.0"),
            ("V0.1", "0.1.0"),
            (" 1.2.3 ", "1.2.3"),
        ];
        for (input, want) in normalized {
            assert_eq!(v(input).to_string(), want, "ввод: «{input}»");
        }
    }

    /// Каноническая цепочка порядка с semver.org: предрелизы младше
    /// релиза, `alpha < beta < rc`, числовые сегменты сравниваются как
    /// числа (`beta.2 < beta.11`), `1.2.0 < 1.10.0`.
    #[test]
    fn ordering_matches_semver_org_chain() {
        let ordered = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.2.0",
            "1.10.0",
            "2.0.0",
            "10.0.0",
        ];
        // Каждая следующая версия строго старше предыдущей.
        for pair in ordered.windows(2) {
            let (a, b) = (v(pair[0]), v(pair[1]));
            assert!(a < b, "{a} должна быть младше {b}");
            assert!(b > a);
            assert_ne!(a, b);
        }
        // Сортировка перевёрнутого списка восстанавливает порядок.
        let mut shuffled: Vec<Version> = ordered.iter().rev().map(|s| v(s)).collect();
        shuffled.sort();
        let expected: Vec<Version> = ordered.iter().map(|s| v(s)).collect();
        assert_eq!(shuffled, expected);
    }

    /// Детали порядка предрелизов: предрелиз строго младше релиза;
    /// числовые сегменты — как числа, а не лексикографически (`9 < 10`);
    /// числовой сегмент младше строкового; общий префикс проигрывает
    /// более длинному списку.
    #[test]
    fn ordering_prerelease_rules() {
        assert!(v("1.0.0-rc.1") < v("1.0.0"));
        assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
        // «9 < 10» — лексикографически было бы наоборот.
        assert!(v("1.0.0-rc.9") < v("1.0.0-rc.10"));
        // Числовой сегмент младше строкового.
        assert!(v("1.0.0-9") < v("1.0.0-alpha"));
        // Антисимметрия на числовых сегментах разной длины.
        let (a, b) = (v("1.0.0-beta.2"), v("1.0.0-beta.11"));
        assert!(a < b && b > a && a != b);
    }

    /// `Ord` согласован с `Eq`: одинаковые строки — строго равны;
    /// `FromStr` работает через `str::parse`.
    #[test]
    fn eq_ord_consistency_and_from_str() {
        let a: Version = "1.2.3-rc.1".parse().unwrap();
        let b = v("1.2.3-rc.1");
        assert_eq!(a, b);
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert_eq!(a.partial_cmp(&b), Some(Ordering::Equal));
        assert!("1.2.3.4".parse::<Version>().is_err());
    }

    /// Сообщения ошибок — человекочитаемые, по-русски, с контекстом;
    /// `ParseError` — полноценная `std::error::Error`.
    #[test]
    fn parse_error_display_is_informative() {
        let err = Version::parse("01.2.3").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("01"), "сообщение: {msg}");
        assert!(msg.contains("ведущий нуль"), "сообщение: {msg}");

        fn assert_std_error<T: Error>(_: &T) {}
        assert_std_error(&err);
    }

    /// `^` — совместимость по SemVer: левый ненулевой компонент
    /// фиксирован, неуказанные компоненты — «дикая карта».
    #[test]
    fn compatible_caret() {
        // ^1.2 ≡ >=1.2.0 <2.0.0
        assert!(v("1.2.0").is_compatible_with("^1.2"));
        assert!(v("1.9.9").is_compatible_with("^1.2"));
        // Предрелизы из диапазона не отфильтровываются.
        assert!(v("1.9.9-rc.1").is_compatible_with("^1.2"));
        assert!(!v("1.1.9").is_compatible_with("^1.2"));
        assert!(!v("2.0.0").is_compatible_with("^1.2"));
        // ^0.2.3 ≡ >=0.2.3 <0.3.0
        assert!(v("0.2.3").is_compatible_with("^0.2.3"));
        assert!(v("0.2.9").is_compatible_with("^0.2.3"));
        assert!(!v("0.3.0").is_compatible_with("^0.2.3"));
        assert!(!v("1.0.0").is_compatible_with("^0.2.3"));
        // ^0.0.3 ≡ >=0.0.3 <0.0.4
        assert!(v("0.0.3").is_compatible_with("^0.0.3"));
        assert!(!v("0.0.4").is_compatible_with("^0.0.3"));
        // ^1 — указан только major.
        assert!(v("1.9.9").is_compatible_with("^1"));
        assert!(!v("2.0.0").is_compatible_with("^1"));
        // ^0.0 и ^0 — нулевой префикс до первого указанного компонента.
        assert!(v("0.0.9").is_compatible_with("^0.0"));
        assert!(!v("0.1.0").is_compatible_with("^0.0"));
        assert!(v("0.9.9").is_compatible_with("^0"));
        assert!(!v("1.0.0").is_compatible_with("^0"));
    }

    /// `~` — патч-диапазон: minor фиксирован, patch растёт.
    #[test]
    fn compatible_tilde() {
        // ~1.2.3 ≡ >=1.2.3 <1.3.0
        assert!(v("1.2.3").is_compatible_with("~1.2.3"));
        assert!(v("1.2.9").is_compatible_with("~1.2.3"));
        assert!(!v("1.2.2").is_compatible_with("~1.2.3"));
        assert!(!v("1.3.0").is_compatible_with("~1.2.3"));
        // ~1.2 ≡ >=1.2.0 <1.3.0
        assert!(v("1.2.0").is_compatible_with("~1.2"));
        assert!(!v("1.3.0").is_compatible_with("~1.2"));
        // ~1 ≡ >=1.0.0 <2.0.0
        assert!(v("1.99.0").is_compatible_with("~1"));
        assert!(!v("2.0.0").is_compatible_with("~1"));
    }

    /// Операторы сравнения: `>=`, `>`, `<=`, `<`, `=`. Отдельно —
    /// сценарий MSRV: `>=1.85` против `rust-version` из `Cargo.toml`.
    #[test]
    fn compatible_comparison_ops() {
        // MSRV-проверка: тулчейн 1.85+ подходит, системный 1.75 — нет.
        assert!(v("1.85.0").is_compatible_with(">=1.85"));
        assert!(v("1.90.1").is_compatible_with(">=1.85"));
        assert!(!v("1.75.0").is_compatible_with(">=1.85"));
        // Предрелиз уровня младше самого уровня.
        assert!(!v("1.0.0-rc.1").is_compatible_with(">=1.0"));
        assert!(v("1.0.0-rc.1").is_compatible_with(">=1.0.0-alpha"));
        // Строгие и нестрогие границы.
        assert!(v("1.0.1").is_compatible_with(">1.0"));
        assert!(!v("1.0.0").is_compatible_with(">1.0"));
        assert!(v("2.0.0").is_compatible_with("<=2.0"));
        assert!(!v("2.0.1").is_compatible_with("<=2.0"));
        assert!(v("1.9.9").is_compatible_with("<2.0"));
        assert!(!v("2.0.0").is_compatible_with("<2.0"));
        // Явное равенство учитывает предрелиз.
        assert!(v("1.2.3").is_compatible_with("=1.2.3"));
        assert!(!v("1.2.3-rc.1").is_compatible_with("=1.2.3"));
    }

    /// Требование без оператора — точное совпадение после нормализации
    /// (`1.2` → `1.2.0`).
    #[test]
    fn compatible_bare_requirement_is_exact() {
        assert!(v("1.2.3").is_compatible_with("1.2.3"));
        assert!(!v("1.2.4").is_compatible_with("1.2.3"));
        assert!(v("1.2.0").is_compatible_with("1.2"));
        assert!(!v("1.2.1").is_compatible_with("1.2"));
    }

    /// Неразбираемое требование → `false`, без паники.
    #[test]
    fn compatible_invalid_requirement_is_false() {
        let ver = v("1.2.3");
        let bad = ["", "   ", "^", ">=", "~", ">=abc", "^1.2.3.4", "=>1.0", "1.2.3+b"];
        for req in bad {
            assert!(!ver.is_compatible_with(req), "требование: «{req}»");
        }
    }

    /// Инкременты: обнуляют младшие компоненты и сбрасывают предрелиз;
    /// исходная версия не меняется; переполнение `u64` насыщается.
    #[test]
    fn bump_methods() {
        let ver = v("1.2.3-rc.1");
        assert_eq!(ver.bump_major(), v("2.0.0"));
        assert_eq!(ver.bump_minor(), v("1.3.0"));
        assert_eq!(ver.bump_patch(), v("1.2.4"));
        // Исходная версия не тронута.
        assert_eq!(ver.to_string(), "1.2.3-rc.1");

        // Насыщение на u64::MAX.
        let maxed = Version::new(u64::MAX, u64::MAX, u64::MAX);
        assert_eq!(maxed.bump_patch(), maxed);
        assert_eq!(maxed.bump_major(), Version::new(u64::MAX, 0, 0));
        assert_eq!(maxed.bump_minor(), Version::new(u64::MAX, u64::MAX, 0));
    }
}
