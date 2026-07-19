//! Отложенные и повторные промпты внутри сессии агента (образец — CronCreate из kimi CLI).
//!
//! Модуль самодостаточен (только `std`): свой 5-полевой cron-парсер
//! («минута час день-месяца месяц день-недели»), григорианский календарь на алгоритме
//! Ховарда Хиннанта (days-from-civil) и планировщик задач с коалесцингом пропущенных
//! срабатываний и детерминированным джиттером.
//!
//! Формы полей: `*`, `*/n`, `n`, `a-b`, `a-b/n` и списки через запятую (`1,15,30`).
//! День недели: 0 и 7 — воскресенье. Как в Vixie cron, когда ограничены и день-месяца,
//! и день-недели, задача срабатывает при совпадении ЛЮБОГО из них (ИЛИ-семантика).
//!
//! ```
//! use theseus::cron::{CivilTime, CronSchedule};
//!
//! let sched = CronSchedule::parse("*/5 * * * *")?;
//! let from = CivilTime::new(2026, 7, 18, 10, 3).unwrap();
//! assert_eq!(sched.next_fire_after(&from), CivilTime::new(2026, 7, 18, 10, 5));
//! # Ok::<(), theseus::cron::CronError>(())
//! ```

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

/// Горизонт поиска следующего срабатывания: «пять лет» в сутках с запасом на високосные дни.
const MAX_SEARCH_DAYS: u32 = 366 * 5 + 2;
/// Максимальный джиттер срабатывания — 15 минут.
const MAX_JITTER_MINUTES: i64 = 15;
/// Период по умолчанию (сутки) для джиттера, когда следующего срабатывания не видно.
const DEFAULT_PERIOD_MINUTES: i64 = 24 * 60;
/// Потолок коалесцинга за один тик (~69 суток для ежеминутной задачи).
const MAX_COALESCED: u32 = 100_000;

/// Календарная дата-время с точностью до минуты (григорианский календарь, без часовых поясов).
///
/// `weekday` вычисляется конструктором [`CivilTime::new`]: 0 — воскресенье … 6 — суббота
/// (cron-конвенция). В сравнении на равенство и порядок поле не участвует.
#[derive(Debug, Clone, Copy)]
pub struct CivilTime {
    /// Год (1..=9999).
    pub year: i32,
    /// Месяц (1..=12).
    pub month: u8,
    /// День месяца (1..=31, проверяется по реальному календарю).
    pub day: u8,
    /// Час (0..=23).
    pub hour: u8,
    /// Минута (0..=59).
    pub minute: u8,
    /// День недели: 0 — воскресенье … 6 — суббота.
    pub weekday: u8,
}

impl CivilTime {
    /// Создаёт валидное календарное время.
    ///
    /// Несуществующие даты отвергаются: 31 февраля, 29 февраля невисокосного года,
    /// месяц 13, час 24 и т.п. дают `None`.
    pub fn new(year: i32, month: u8, day: u8, hour: u8, minute: u8) -> Option<Self> {
        if !(1..=9999).contains(&year) {
            return None;
        }
        let dim = Self::days_in_month(year, month)?;
        if day == 0 || day > dim || hour > 23 || minute > 59 {
            return None;
        }
        let weekday = weekday_from_days(days_from_civil(year, month, day));
        Some(CivilTime { year, month, day, hour, minute, weekday })
    }

    /// Високосный ли год (правила Григорианского календаря).
    pub fn is_leap_year(year: i32) -> bool {
        (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
    }

    /// Число дней в месяце; `None` для месяца вне 1..=12.
    pub fn days_in_month(year: i32, month: u8) -> Option<u8> {
        let days = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if Self::is_leap_year(year) => 29,
            2 => 28,
            _ => return None,
        };
        Some(days)
    }

    /// Сдвиг на `mins` минут в любую сторону с корректным переходом через границы
    /// дней, месяцев, лет и високосные феврали. `None` — лишь при выходе года за 1..=9999.
    pub fn add_minutes(&self, mins: i64) -> Option<Self> {
        let total = self.to_minutes().checked_add(mins)?;
        let (year, month, day) = civil_from_days(total.div_euclid(1440))?;
        let rem = total.rem_euclid(1440);
        Self::new(year, month, day, (rem / 60) as u8, (rem % 60) as u8)
    }

    /// Минуты от эпохи 1970-01-01 — основа сравнения и арифметики.
    fn to_minutes(self) -> i64 {
        days_from_civil(self.year, self.month, self.day) * 1440
            + i64::from(self.hour) * 60
            + i64::from(self.minute)
    }
}

impl fmt::Display for CivilTime {
    /// Формат `YYYY-MM-DD HH:MM`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute
        )
    }
}

impl PartialEq for CivilTime {
    fn eq(&self, other: &Self) -> bool {
        self.to_minutes() == other.to_minutes()
    }
}

impl Eq for CivilTime {}

impl PartialOrd for CivilTime {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CivilTime {
    fn cmp(&self, other: &Self) -> Ordering {
        self.to_minutes().cmp(&other.to_minutes())
    }
}

/// Номер дня от эпохи 1970-01-01 по алгоритму Ховарда Хиннанта (days_from_civil).
fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let mut y = i64::from(year);
    let m = i64::from(month);
    // считаем месяцы с марта: январь и февраль — 11-й и 12-й месяцы «прошлого» года
    y -= i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9).rem_euclid(12); // март = 0 … февраль = 11
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Обратное преобразование: (год, месяц, день) по номеру дня от эпохи.
///
/// `None` — только если год вышел за пределы `i32` (далеко за рамками практики).
fn civil_from_days(z: i64) -> Option<(i32, u8, u8)> {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = i32::try_from(y + i64::from(m <= 2)).ok()?;
    Some((year, u8::try_from(m).ok()?, u8::try_from(d).ok()?))
}

/// День недели по номеру дня: 0 — воскресенье … 6 — суббота (1970-01-01 — четверг).
fn weekday_from_days(z: i64) -> u8 {
    ((z + 4).rem_euclid(7)) as u8
}

/// Число минут между двумя моментами (`to` минус `from`).
fn minutes_between(from: &CivilTime, to: &CivilTime) -> i64 {
    to.to_minutes() - from.to_minutes()
}

/// 00:00 следующих календарных суток.
fn next_day_start(t: &CivilTime) -> Option<CivilTime> {
    let (y, m, d) = civil_from_days(days_from_civil(t.year, t.month, t.day) + 1)?;
    CivilTime::new(y, m, d, 0, 0)
}

/// Ошибка разбора cron-выражения или постановки задачи.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronError {
    /// Полей не пять.
    FieldCount {
        /// Сколько полей получено.
        got: usize,
    },
    /// Пустое поле или пустой элемент списка (`1,,2`).
    EmptyField {
        /// Имя поля («минута», «час», …).
        field: &'static str,
    },
    /// Нечисловой токен.
    InvalidNumber {
        /// Имя поля.
        field: &'static str,
        /// Исходный текст токена.
        text: String,
    },
    /// Число вне допустимого диапазона поля.
    OutOfRange {
        /// Имя поля.
        field: &'static str,
        /// Значение.
        value: u32,
        /// Минимум (включительно).
        min: u32,
        /// Максимум (включительно).
        max: u32,
    },
    /// Обратный диапазон (`5-2`).
    BadRange {
        /// Имя поля.
        field: &'static str,
        /// Левая граница.
        lo: u32,
        /// Правая граница.
        hi: u32,
    },
    /// Шаг `*/0` недопустим.
    ZeroStep {
        /// Имя поля.
        field: &'static str,
    },
    /// Шаг без звёздочки или диапазона (`5/2`) — вне поддерживаемой грамматики.
    StepWithoutRange {
        /// Имя поля.
        field: &'static str,
    },
    /// Корректное выражение без срабатываний в горизонте 5 лет (`0 0 31 2 *`).
    NeverFires {
        /// Исходное выражение.
        expr: String,
    },
}

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CronError::FieldCount { got } => write!(
                f,
                "cron-выражение должно содержать ровно 5 полей (минута час день-месяца месяц день-недели), получено: {got}"
            ),
            CronError::EmptyField { field } => write!(f, "поле «{field}»: пустой элемент"),
            CronError::InvalidNumber { field, text } => {
                write!(f, "поле «{field}»: «{text}» — не число")
            }
            CronError::OutOfRange { field, value, min, max } => {
                write!(f, "поле «{field}»: {value} вне диапазона {min}..={max}")
            }
            CronError::BadRange { field, lo, hi } => {
                write!(f, "поле «{field}»: обратный диапазон {lo}-{hi}")
            }
            CronError::ZeroStep { field } => write!(f, "поле «{field}»: шаг не может быть нулём"),
            CronError::StepWithoutRange { field } => {
                write!(f, "поле «{field}»: шаг допустим только после «*» или диапазона")
            }
            CronError::NeverFires { expr } => {
                write!(f, "выражение «{expr}» не даёт срабатываний в ближайшие 5 лет")
            }
        }
    }
}

impl Error for CronError {}

/// Описание одного поля cron-выражения: имя для ошибок и допустимый диапазон.
struct FieldSpec {
    name: &'static str,
    min: u32,
    max: u32,
}

const MINUTE_SPEC: FieldSpec = FieldSpec { name: "минута", min: 0, max: 59 };
const HOUR_SPEC: FieldSpec = FieldSpec { name: "час", min: 0, max: 23 };
const DOM_SPEC: FieldSpec = FieldSpec { name: "день-месяца", min: 1, max: 31 };
const MONTH_SPEC: FieldSpec = FieldSpec { name: "месяц", min: 1, max: 12 };
const DOW_SPEC: FieldSpec = FieldSpec { name: "день-недели", min: 0, max: 7 };

/// Число с проверкой диапазона поля.
fn parse_num(text: &str, spec: &FieldSpec) -> Result<u32, CronError> {
    let value: u32 = text
        .parse()
        .map_err(|_| CronError::InvalidNumber { field: spec.name, text: text.to_string() })?;
    if !(spec.min..=spec.max).contains(&value) {
        return Err(CronError::OutOfRange { field: spec.name, value, min: spec.min, max: spec.max });
    }
    Ok(value)
}

/// Один элемент поля: `*`, `*/n`, `n`, `a-b` или `a-b/n`. Возвращает битовую маску значений.
fn parse_item(item: &str, spec: &FieldSpec) -> Result<u64, CronError> {
    if item.is_empty() {
        return Err(CronError::EmptyField { field: spec.name });
    }
    let (base, step) = match item.split_once('/') {
        Some((b, s)) => {
            let step: u32 = s
                .parse()
                .map_err(|_| CronError::InvalidNumber { field: spec.name, text: s.to_string() })?;
            if step == 0 {
                return Err(CronError::ZeroStep { field: spec.name });
            }
            if step > spec.max {
                return Err(CronError::OutOfRange { field: spec.name, value: step, min: 1, max: spec.max });
            }
            (b, step)
        }
        None => (item, 1),
    };
    let (lo, hi) = if base == "*" {
        (spec.min, spec.max)
    } else if let Some((a, b)) = base.split_once('-') {
        let lo = parse_num(a, spec)?;
        let hi = parse_num(b, spec)?;
        if lo > hi {
            return Err(CronError::BadRange { field: spec.name, lo, hi });
        }
        (lo, hi)
    } else {
        let v = parse_num(base, spec)?;
        if step != 1 {
            // «5/2» — шаг без звёздочки и без диапазона: не поддерживаем
            return Err(CronError::StepWithoutRange { field: spec.name });
        }
        (v, v)
    };
    let mut bits = 0u64;
    let mut v = lo;
    while v <= hi {
        bits |= 1u64 << v;
        v += step;
    }
    Ok(bits)
}

/// Поле целиком: один или несколько элементов через запятую.
fn parse_field(text: &str, spec: &FieldSpec) -> Result<u64, CronError> {
    if text.is_empty() {
        return Err(CronError::EmptyField { field: spec.name });
    }
    let mut bits = 0u64;
    for item in text.split(',') {
        bits |= parse_item(item, spec)?;
    }
    Ok(bits)
}

/// Установлен ли бит `idx` в маске.
fn bit(bits: u64, idx: u32) -> bool {
    bits & (1u64 << idx) != 0
}

/// Индекс первого установленного бита, начиная с `from`.
fn next_set(bits: u64, from: u32) -> Option<u32> {
    (from..64).find(|&i| bit(bits, i))
}

/// Разобранное cron-расписание: битовые маски допустимых значений каждого поля.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minutes: u64,   // биты 0..=59
    hours: u64,     // биты 0..=23
    dom: u64,       // биты 1..=31
    months: u64,    // биты 1..=12
    dow: u64,       // биты 0..=6 (7 нормализуется в 0 — воскресенье)
    dom_any: bool,  // поле дня-месяца — ровно «*»
    dow_any: bool,  // поле дня-недели — ровно «*»
}

impl CronSchedule {
    /// Разбирает 5-полевое cron-выражение («минута час день-месяца месяц день-недели»).
    ///
    /// # Ошибки
    /// [`CronError`] при любом нарушении грамматики или диапазонов — см. перечисление.
    pub fn parse(expr: &str) -> Result<Self, CronError> {
        expr.parse()
    }

    /// Совпадает ли момент времени с расписанием (точность — минута).
    pub fn matches(&self, t: &CivilTime) -> bool {
        bit(self.minutes, u32::from(t.minute)) && bit(self.hours, u32::from(t.hour)) && self.date_matches(t)
    }

    /// Первое срабатывание строго позже `from`.
    ///
    /// Перебор идёт по реальным календарным суткам, поэтому несуществующие даты
    /// (31 февраля) не генерируются в принципе. Горизонт поиска — 5 лет; если за это
    /// время срабатываний нет, возвращается `None`.
    pub fn next_fire_after(&self, from: &CivilTime) -> Option<CivilTime> {
        let mut cursor = from.add_minutes(1)?;
        for _ in 0..MAX_SEARCH_DAYS {
            if self.date_matches(&cursor) {
                let mut hour_from = u32::from(cursor.hour);
                while let Some(h) = next_set(self.hours, hour_from) {
                    let minute_from =
                        if h == u32::from(cursor.hour) { u32::from(cursor.minute) } else { 0 };
                    if let Some(mi) = next_set(self.minutes, minute_from) {
                        let hour = u8::try_from(h).ok()?;
                        let minute = u8::try_from(mi).ok()?;
                        return CivilTime::new(cursor.year, cursor.month, cursor.day, hour, minute);
                    }
                    hour_from = h + 1;
                }
            }
            cursor = next_day_start(&cursor)?;
        }
        None
    }

    /// Совпадение календарной части (месяц + день-месяца/день-недели).
    ///
    /// ИЛИ-семантика Vixie cron: при ограниченных обоих дневных полях достаточно
    /// совпадения любого из них; «*» означает «не ограничено».
    fn date_matches(&self, t: &CivilTime) -> bool {
        if !bit(self.months, u32::from(t.month)) {
            return false;
        }
        let dom_ok = bit(self.dom, u32::from(t.day));
        let dow_ok = bit(self.dow, u32::from(t.weekday));
        match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (true, false) => dow_ok,
            (false, true) => dom_ok,
            (false, false) => dom_ok || dow_ok,
        }
    }
}

impl FromStr for CronSchedule {
    type Err = CronError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut fields = s.split_whitespace();
        let (Some(mi), Some(ho), Some(dm), Some(mo), Some(dw), None) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            return Err(CronError::FieldCount { got: s.split_whitespace().count() });
        };
        let minutes = parse_field(mi, &MINUTE_SPEC)?;
        let hours = parse_field(ho, &HOUR_SPEC)?;
        let dom = parse_field(dm, &DOM_SPEC)?;
        let months = parse_field(mo, &MONTH_SPEC)?;
        let mut dow = parse_field(dw, &DOW_SPEC)?;
        if dow & (1u64 << 7) != 0 {
            // 7 — синоним воскресенья (0)
            dow = (dow | 1) & !(1u64 << 7);
        }
        Ok(CronSchedule { minutes, hours, dom, months, dow, dom_any: dm == "*", dow_any: dw == "*" })
    }
}

/// SplitMix64 — дешёвый детерминированный хэш для джиттера.
fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Детерминированный сдвиг срабатывания: до 10 % периода, но не более 15 минут.
///
/// Один и тот же `id` при том же периоде всегда даёт один и тот же сдвиг — джиттер
/// стабилен между перезапусками и не «пляшет» от тика к тику. При периоде меньше
/// 10 минут джиттер нулевой (точность времени — минута).
fn jitter_minutes(id: u64, period_minutes: i64) -> i64 {
    let cap = (period_minutes.max(1) / 10).min(MAX_JITTER_MINUTES) as u64;
    (splitmix64(id) % (cap + 1)) as i64
}

/// Расписанное срабатывание плюс джиттер; период оценивается по следующему срабатыванию.
fn effective_fire(id: u64, schedule: &CronSchedule, fire: &CivilTime) -> CivilTime {
    let period = schedule
        .next_fire_after(fire)
        .map_or(DEFAULT_PERIOD_MINUTES, |next| minutes_between(fire, &next));
    fire.add_minutes(jitter_minutes(id, period)).unwrap_or(*fire)
}

/// Запланированная задача: отложенный или повторный промпт агента.
#[derive(Debug, Clone)]
pub struct CronTask {
    /// Идентификатор (выдаёт планировщик, начиная с 1).
    pub id: u64,
    /// Исходное cron-выражение.
    pub expr: String,
    /// Промпт, который получит агент при срабатывании.
    pub prompt: String,
    /// `true` — повторяющаяся; `false` — разовая (снимается после срабатывания).
    pub recurring: bool,
    /// Момент постановки.
    pub created_at: CivilTime,
    schedule: CronSchedule,
    next_fire: CivilTime, // расписанное срабатывание, без джиттера
}

impl CronTask {
    /// Ближайшее расписанное (без джиттера) срабатывание.
    pub fn next_fire(&self) -> CivilTime {
        self.next_fire
    }

    /// Ближайшее срабатывание с учётом детерминированного джиттера.
    pub fn effective_fire(&self) -> CivilTime {
        effective_fire(self.id, &self.schedule, &self.next_fire)
    }
}

/// Сработавшая за тик планировщика задача.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueTask {
    /// Идентификатор задачи.
    pub id: u64,
    /// Сколько срабатываний накопилось (коалесцинг пропущенных): 1 — без пропусков.
    pub coalesced_count: u32,
}

/// Планировщик отложенных и повторных промптов внутри сессии агента.
#[derive(Debug, Default)]
pub struct CronScheduler {
    tasks: BTreeMap<u64, CronTask>,
    next_id: u64,
}

impl CronScheduler {
    /// Пустой планировщик.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ставит задачу и возвращает её id.
    ///
    /// `now` — момент постановки; первое срабатывание ищется строго после него.
    ///
    /// # Ошибки
    /// [`CronError`] из парсера, а также [`CronError::NeverFires`], если у корректного
    /// выражения нет срабатываний в горизонте 5 лет (`0 0 31 2 *` и т.п.).
    pub fn add(&mut self, expr: &str, prompt: &str, recurring: bool, now: &CivilTime) -> Result<u64, CronError> {
        let schedule = CronSchedule::parse(expr)?;
        let first = schedule
            .next_fire_after(now)
            .ok_or_else(|| CronError::NeverFires { expr: expr.to_string() })?;
        self.next_id += 1;
        let id = self.next_id;
        self.tasks.insert(id, CronTask {
            id,
            expr: expr.to_string(),
            prompt: prompt.to_string(),
            recurring,
            created_at: *now,
            schedule,
            next_fire: first,
        });
        Ok(id)
    }

    /// Снимает задачу; `false`, если id не найден.
    pub fn remove(&mut self, id: u64) -> bool {
        self.tasks.remove(&id).is_some()
    }

    /// Все задачи в порядке id.
    pub fn list(&self) -> Vec<&CronTask> {
        self.tasks.values().collect()
    }

    /// Забирает сработавшие на `now` задачи, продвигая повторные на следующее окно.
    ///
    /// Пропущенные срабатывания повторной задачи коалесцируются: возвращается одна
    /// запись, `coalesced_count` которой равен числу накопленных срабатываний. Разовые
    /// задачи снимаются после первого срабатывания. Повторная задача, у которой в
    /// горизонте 5 лет больше нет срабатываний, тоже снимается.
    pub fn due_tasks(&mut self, now: &CivilTime) -> Vec<DueTask> {
        let mut due = Vec::new();
        let mut remove = Vec::new();
        for task in self.tasks.values_mut() {
            let mut count = 0u32;
            let mut pending = task.next_fire;
            let mut exhausted = false;
            let mut budget = MAX_COALESCED;
            while budget > 0 {
                if effective_fire(task.id, &task.schedule, &pending) > *now {
                    break;
                }
                count += 1;
                budget -= 1;
                if !task.recurring {
                    break;
                }
                let Some(next) = task.schedule.next_fire_after(&pending) else {
                    exhausted = true;
                    break;
                };
                pending = next;
            }
            if count == 0 {
                continue;
            }
            due.push(DueTask { id: task.id, coalesced_count: count });
            if !task.recurring || exhausted {
                remove.push(task.id);
            } else {
                task.next_fire = pending;
            }
        }
        for id in remove {
            self.tasks.remove(&id);
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Короткий конструктор валидного времени для тестов.
    fn ct(y: i32, mo: u8, d: u8, h: u8, mi: u8) -> CivilTime {
        CivilTime::new(y, mo, d, h, mi).unwrap()
    }

    /// Битовая маска диапазона с шагом — эталон для проверок парсера.
    fn mask(range: std::ops::RangeInclusive<u32>, step: u32) -> u64 {
        range.step_by(step as usize).fold(0u64, |acc, v| acc | (1u64 << v))
    }

    #[test]
    fn civil_time_rejects_invalid_dates() {
        assert!(CivilTime::new(2023, 2, 29, 0, 0).is_none()); // 2023 — не високосный
        assert!(CivilTime::new(2024, 2, 29, 0, 0).is_some()); // 2024 — високосный
        assert!(CivilTime::new(1900, 2, 29, 0, 0).is_none()); // кратен 100, но не 400
        assert!(CivilTime::new(2000, 2, 29, 0, 0).is_some()); // кратен 400
        assert!(CivilTime::new(2026, 4, 31, 0, 0).is_none()); // в апреле 30 дней
        assert!(CivilTime::new(2026, 13, 1, 0, 0).is_none());
        assert!(CivilTime::new(2026, 0, 1, 0, 0).is_none());
        assert!(CivilTime::new(2026, 1, 0, 0, 0).is_none());
        assert!(CivilTime::new(2026, 1, 1, 24, 0).is_none());
        assert!(CivilTime::new(2026, 1, 1, 0, 60).is_none());
        assert!(CivilTime::new(0, 1, 1, 0, 0).is_none());
        assert!(CivilTime::new(10_000, 1, 1, 0, 0).is_none());
    }

    #[test]
    fn weekday_anchors_match_known_dates() {
        assert_eq!(ct(1970, 1, 1, 0, 0).weekday, 4); // четверг, эпоха Unix
        assert_eq!(ct(2000, 1, 1, 0, 0).weekday, 6); // суббота
        assert_eq!(ct(2024, 2, 29, 0, 0).weekday, 4); // четверг, високосный день
        assert_eq!(ct(2026, 7, 18, 0, 0).weekday, 6); // суббота
        assert_eq!(ct(2026, 7, 19, 0, 0).weekday, 0); // воскресенье
    }

    #[test]
    fn add_minutes_cascades_calendar_boundaries() {
        assert_eq!(ct(2026, 1, 31, 23, 59).add_minutes(2), Some(ct(2026, 2, 1, 0, 1)));
        assert_eq!(ct(2026, 12, 31, 23, 59).add_minutes(1), Some(ct(2027, 1, 1, 0, 0)));
        assert_eq!(ct(2024, 2, 28, 23, 59).add_minutes(1), Some(ct(2024, 2, 29, 0, 0)));
        assert_eq!(ct(2026, 3, 1, 0, 0).add_minutes(-1), Some(ct(2026, 2, 28, 23, 59)));
        assert_eq!(ct(2026, 7, 18, 10, 30).add_minutes(0), Some(ct(2026, 7, 18, 10, 30)));
    }

    #[test]
    fn days_civil_roundtrip() {
        for z in (-40_000i64..40_000).step_by(37) {
            let (y, m, d) = civil_from_days(z).unwrap();
            assert_eq!(days_from_civil(y, m, d), z, "roundtrip для дня {z}");
        }
    }

    #[test]
    fn parser_covers_all_field_forms() {
        let s = CronSchedule::parse("* * * * *").unwrap();
        assert_eq!(s.minutes, mask(0..=59, 1));
        assert_eq!(s.hours, mask(0..=23, 1));
        assert_eq!(s.dom, mask(1..=31, 1));
        assert_eq!(s.months, mask(1..=12, 1));
        assert_eq!(s.dow, mask(0..=6, 1));
        assert!(s.dom_any && s.dow_any);

        assert_eq!(CronSchedule::parse("*/5 * * * *").unwrap().minutes, mask(0..=59, 5));
        assert_eq!(CronSchedule::parse("7 * * * *").unwrap().minutes, mask(7..=7, 1));
        assert_eq!(CronSchedule::parse("1-4 * * * *").unwrap().minutes, mask(1..=4, 1));
        assert_eq!(CronSchedule::parse("1-10/3 * * * *").unwrap().minutes, mask(1..=10, 3));
        assert_eq!(CronSchedule::parse("2,4,6 * * * *").unwrap().minutes, mask(2..=6, 2));
        assert_eq!(
            CronSchedule::parse("0,30,45-50 * * * *").unwrap().minutes,
            mask(0..=0, 1) | mask(30..=30, 1) | mask(45..=50, 1)
        );
    }

    #[test]
    fn parser_maps_dow_seven_to_sunday() {
        let sun0 = CronSchedule::parse("0 12 * * 0").unwrap();
        let sun7 = CronSchedule::parse("0 12 * * 7").unwrap();
        assert_eq!(sun0, sun7);
        assert!(sun7.matches(&ct(2026, 7, 19, 12, 0))); // воскресенье
        assert!(!sun7.matches(&ct(2026, 7, 20, 12, 0))); // понедельник
        let workdays = CronSchedule::parse("0 9 * * 1-5").unwrap();
        assert!(workdays.matches(&ct(2026, 7, 17, 9, 0))); // пятница
        assert!(!workdays.matches(&ct(2026, 7, 18, 9, 0))); // суббота
    }

    #[test]
    fn parser_rejects_invalid_expressions() {
        use CronError::*;
        assert!(matches!(CronSchedule::parse(""), Err(FieldCount { .. })));
        assert!(matches!(CronSchedule::parse("* * *"), Err(FieldCount { .. })));
        assert!(matches!(CronSchedule::parse("* * * * * *"), Err(FieldCount { .. })));
        assert!(matches!(CronSchedule::parse("60 * * * *"), Err(OutOfRange { .. })));
        assert!(matches!(CronSchedule::parse("* 24 * * *"), Err(OutOfRange { .. })));
        assert!(matches!(CronSchedule::parse("* * 0 * *"), Err(OutOfRange { .. })));
        assert!(matches!(CronSchedule::parse("* * * 13 *"), Err(OutOfRange { .. })));
        assert!(matches!(CronSchedule::parse("* * * * 8"), Err(OutOfRange { .. })));
        assert!(matches!(CronSchedule::parse("*/0 * * * *"), Err(ZeroStep { .. })));
        assert!(matches!(CronSchedule::parse("*/99 * * * *"), Err(OutOfRange { .. })));
        assert!(matches!(CronSchedule::parse("5-2 * * * *"), Err(BadRange { .. })));
        assert!(matches!(CronSchedule::parse("abc * * * *"), Err(InvalidNumber { .. })));
        assert!(matches!(CronSchedule::parse("1,,2 * * * *"), Err(EmptyField { .. })));
        assert!(matches!(CronSchedule::parse("5/2 * * * *"), Err(StepWithoutRange { .. })));
        assert!(matches!(CronSchedule::parse("1- * * * *"), Err(InvalidNumber { .. })));
    }

    #[test]
    fn next_fire_every_five_minutes() {
        let s = CronSchedule::parse("*/5 * * * *").unwrap();
        assert_eq!(s.next_fire_after(&ct(2026, 7, 18, 10, 3)), Some(ct(2026, 7, 18, 10, 5)));
        // строго «после»: ровно в момент срабатывания ищется уже следующее
        assert_eq!(s.next_fire_after(&ct(2026, 7, 18, 10, 5)), Some(ct(2026, 7, 18, 10, 10)));
        assert_eq!(s.next_fire_after(&ct(2026, 7, 18, 10, 55)), Some(ct(2026, 7, 18, 11, 0)));
        assert_eq!(s.next_fire_after(&ct(2026, 7, 18, 23, 58)), Some(ct(2026, 7, 19, 0, 0)));
    }

    #[test]
    fn next_fire_daily_and_strictly_after() {
        let s = CronSchedule::parse("30 9 * * *").unwrap();
        assert_eq!(s.next_fire_after(&ct(2026, 7, 18, 9, 29)), Some(ct(2026, 7, 18, 9, 30)));
        assert_eq!(s.next_fire_after(&ct(2026, 7, 18, 9, 30)), Some(ct(2026, 7, 19, 9, 30)));
        assert_eq!(s.next_fire_after(&ct(2026, 7, 18, 10, 0)), Some(ct(2026, 7, 19, 9, 30)));
        // через границу года
        let ny = CronSchedule::parse("0 0 1 1 *").unwrap();
        assert_eq!(ny.next_fire_after(&ct(2026, 6, 15, 12, 0)), Some(ct(2027, 1, 1, 0, 0)));
    }

    #[test]
    fn next_fire_feb29_leap_years() {
        let s = CronSchedule::parse("0 0 29 2 *").unwrap();
        let fire = s.next_fire_after(&ct(2026, 1, 1, 0, 0)).unwrap();
        assert_eq!(fire, ct(2028, 2, 29, 0, 0));
        assert_eq!(fire.weekday, 2); // вторник — день недели согласован с датой
        // строго после самого високосного дня — следующий через 4 года
        assert_eq!(s.next_fire_after(&ct(2028, 2, 29, 0, 0)), Some(ct(2032, 2, 29, 0, 0)));
    }

    #[test]
    fn next_fire_none_for_impossible_dates() {
        // 31 февраля и 31 апреля не существует: парсер их пропускает,
        // а поиск обязан честно вернуть None, а не невалидную дату
        let feb31 = CronSchedule::parse("0 0 31 2 *").unwrap();
        assert_eq!(feb31.next_fire_after(&ct(2026, 1, 1, 0, 0)), None);
        let apr31 = CronSchedule::parse("0 0 31 4 *").unwrap();
        assert_eq!(apr31.next_fire_after(&ct(2026, 1, 1, 0, 0)), None);
    }

    #[test]
    fn next_fire_dom_dow_or_semantics() {
        // ограничены оба дня — срабатывает при совпадении любого (среда = 3)
        let both = CronSchedule::parse("0 12 15 * 3").unwrap();
        assert_eq!(both.next_fire_after(&ct(2026, 7, 18, 0, 0)), Some(ct(2026, 7, 22, 12, 0)));
        // ограничен только день-месяца: 15 июля уже прошло — ждём 15 августа
        let dom = CronSchedule::parse("0 12 15 * *").unwrap();
        assert_eq!(dom.next_fire_after(&ct(2026, 7, 18, 0, 0)), Some(ct(2026, 8, 15, 12, 0)));
        // ограничен только день-недели
        let dow = CronSchedule::parse("0 12 * * 3").unwrap();
        assert_eq!(dow.next_fire_after(&ct(2026, 7, 18, 0, 0)), Some(ct(2026, 7, 22, 12, 0)));
    }

    #[test]
    fn scheduler_add_list_remove() {
        let mut sch = CronScheduler::new();
        let now = ct(2026, 7, 18, 10, 0);
        let id1 = sch.add("*/5 * * * *", "пульс", true, &now).unwrap();
        let id2 = sch.add("0 9 * * 1", "понедельник", false, &now).unwrap();
        assert_eq!((id1, id2), (1, 2));
        let listed = sch.list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].expr, "*/5 * * * *");
        assert_eq!(listed[0].created_at, now);
        assert_eq!(listed[0].next_fire(), ct(2026, 7, 18, 10, 5));
        assert_eq!(listed[1].prompt, "понедельник");
        assert!(!listed[1].recurring);
        assert!(sch.remove(id1));
        assert!(!sch.remove(id1)); // повторно — уже нет
        assert!(!sch.remove(999));
        assert_eq!(sch.list().len(), 1);
    }

    #[test]
    fn scheduler_rejects_never_firing_expression() {
        let mut sch = CronScheduler::new();
        let now = ct(2026, 7, 18, 10, 0);
        let res = sch.add("0 0 31 2 *", "никогда", true, &now);
        assert!(matches!(res, Err(CronError::NeverFires { .. })));
        assert!(sch.list().is_empty());
    }

    #[test]
    fn scheduler_due_tasks_fire_once_and_advance() {
        let mut sch = CronScheduler::new();
        let t0 = ct(2026, 7, 18, 10, 0);
        let id = sch.add("*/1 * * * *", "каждую минуту", true, &t0).unwrap();
        assert!(sch.due_tasks(&t0).is_empty());
        let due = sch.due_tasks(&ct(2026, 7, 18, 10, 1));
        assert_eq!(due, vec![DueTask { id, coalesced_count: 1 }]);
        // повторный опрос того же момента — пусто: задача продвинулась
        assert!(sch.due_tasks(&ct(2026, 7, 18, 10, 1)).is_empty());
        assert_eq!(sch.due_tasks(&ct(2026, 7, 18, 10, 2)).len(), 1);
    }

    #[test]
    fn scheduler_coalesces_missed_fires() {
        let mut sch = CronScheduler::new();
        let t0 = ct(2026, 7, 18, 10, 0);
        let id = sch.add("*/1 * * * *", "минутная", true, &t0).unwrap();
        // «проспали» 10 минут: одна запись со счётчиком 10
        let due = sch.due_tasks(&ct(2026, 7, 18, 10, 10));
        assert_eq!(due, vec![DueTask { id, coalesced_count: 10 }]);
        // долг погашен — повторный опрос пуст
        assert!(sch.due_tasks(&ct(2026, 7, 18, 10, 10)).is_empty());

        let mut sch5 = CronScheduler::new();
        let id5 = sch5.add("*/5 * * * *", "пятиминутная", true, &t0).unwrap();
        let due5 = sch5.due_tasks(&ct(2026, 7, 18, 10, 30));
        assert_eq!(due5, vec![DueTask { id: id5, coalesced_count: 6 }]);
    }

    #[test]
    fn scheduler_one_shot_fires_once_and_is_removed() {
        let mut sch = CronScheduler::new();
        let t0 = ct(2026, 7, 18, 10, 0);
        let id = sch.add("*/5 * * * *", "разовая", false, &t0).unwrap();
        // пропустили несколько окон — разовая всё равно срабатывает ровно один раз
        assert_eq!(sch.due_tasks(&ct(2026, 7, 18, 10, 20)), vec![DueTask { id, coalesced_count: 1 }]);
        assert!(sch.list().is_empty()); // снята после срабатывания
        assert!(sch.due_tasks(&ct(2026, 7, 18, 10, 25)).is_empty());
    }

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        for id in 0..500u64 {
            let j = jitter_minutes(id, 1440);
            assert!((0..=MAX_JITTER_MINUTES).contains(&j), "джиттер {j} вне границ");
            assert_eq!(j, jitter_minutes(id, 1440), "джиттер обязан быть детерминированным");
            assert!((0..=6).contains(&jitter_minutes(id, 60)));
            assert_eq!(jitter_minutes(id, 5), 0, "период < 10 минут — без джиттера");
        }
        // вариативность между id: значения не должны совпадать у всех
        let spread: BTreeSet<i64> = (0..100).map(|id| jitter_minutes(id, 1440)).collect();
        assert!(spread.len() > 5, "подозрительно бедный разброс: {spread:?}");
    }

    #[test]
    fn jitter_shifts_effective_fire_and_due_time() {
        let mut sch = CronScheduler::new();
        let t0 = ct(2026, 7, 18, 10, 0);
        let id = sch.add("0 12 * * *", "ежедневно в полдень", true, &t0).unwrap();
        let (next, eff) = {
            let listed = sch.list();
            (listed[0].next_fire(), listed[0].effective_fire())
        };
        assert_eq!(next, ct(2026, 7, 18, 12, 0));
        // джиттер ежедневной задачи: 0..=15 минут поверх расписанного времени
        let expected = next.add_minutes(jitter_minutes(id, 1440)).unwrap();
        assert_eq!(eff, expected);
        assert!(sch.due_tasks(&eff.add_minutes(-1).unwrap()).is_empty());
        assert_eq!(sch.due_tasks(&eff), vec![DueTask { id, coalesced_count: 1 }]);
    }
}
