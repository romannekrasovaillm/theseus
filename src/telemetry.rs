//! Метрики сессии «otel-lite» по образцу `codex-rs/otel`, но без внешнего
//! OTLP-экспортёра: данные живут только в памяти процесса и отдаются наружу
//! двумя способами — структурным снапшотом (serde) и текстом в формате
//! Prometheus text exposition.
//!
//! Три типа метрик:
//! - [`Counter`] — монотонно растущий целочисленный счётчик (`inc`/`add`);
//! - [`Gauge`] — мгновенное значение `f64` (`set`/`inc`/`dec`/`add`/`sub`);
//! - [`Histogram`] — распределение наблюдений `f64`: точные `count`/`sum`/
//!   `min`/`max` по всем записям и перцентили (`p50`/`p95`) по выборке.
//!
//! Гистограмма хранит значения в `Vec`, но не более [`RESERVOIR_CAP`] штук:
//! сверх лимита включается резервуарная выборка (algorithm R) — каждое
//! i-е значение (1-based) с вероятностью `CAP / i` замещает случайный элемент
//! резервуара. Поэтому `count`, `sum`, `min` и `max` остаются точными на
//! любом объёме, а перцентили — честной оценкой по равномерной выборке.
//!
//! Имя метрики может нести метки прямо в строке: `"tool.exec{tool=bash}"`.
//! Метки — часть ключа метрики (два имени с разными метками — две разные
//! метрики); разбор строки на базовое имя и `BTreeMap` меток происходит
//! при экспорте ([`parse_metric_name`]). В Prometheus-формате метки
//! выводятся в фигурных скобках, а недопустимые символы базового имени
//! заменяются на `_` (точки превращаются в подчёркивания).
//!
//! Таймер: [`Histogram::timer`] возвращает RAII-guard [`Timer`]; при drop он
//! записывает в гистограмму прошедшее время в миллисекундах — удобно для
//! замеров длительности вызовов инструментов и LLM.
//!
//! Потокобезопасность: все хендлы (`Counter`/`Gauge`/`Histogram`) — дешёвые
//! клонируемые ссылки на общее состояние (`Arc`), реестр можно разделять
//! между потоками. Poisoning мьютексов игнорируется: метрики не должны
//! ронять процесс из-за паники в соседнем потоке.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Максимум значений, хранимых гистограммой в явном виде; сверх лимита
/// работает резервуарная выборка (algorithm R).
pub const RESERVOIR_CAP: usize = 10_000;

/// Берёт мьютекс, игнорируя poisoning: при панике в соседнем потоке читаем
/// данные из отравленного замка, а не падаем вслед за ним.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Детерминированный ГПСЧ (splitmix64) — только для резервуарной выборки,
/// не для криптографии.
#[derive(Debug)]
struct SmallRng(u64);

impl SmallRng {
    /// Инициализация от текущего времени и глобального счётчика: двум
    /// гистограммам, созданным в одну наносекунду, достанутся разные зёрна.
    fn seeded() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        Self(nanos ^ COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed))
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Псевдослучайное число из `[0, n)`; при `n == 0` возвращает 0.
    /// Модулярное смещение для метрик несущественно.
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
}

/// Монотонно растущий целочисленный счётчик (число запросов, токенов,
/// вызовов инструмента). Хендл клонируется дешёво, копии делят состояние.
#[derive(Debug, Clone)]
pub struct Counter {
    value: Arc<AtomicU64>,
}

impl Counter {
    fn new() -> Self {
        Self {
            value: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Увеличить на 1.
    pub fn inc(&self) {
        self.add(1);
    }

    /// Увеличить на `delta`.
    pub fn add(&self, delta: u64) {
        self.value.fetch_add(delta, Ordering::Relaxed);
    }

    /// Текущее значение.
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

/// Мгновенное значение `f64`: может расти и падать (число активных
/// запросов, размер очереди, температура). Хендлы делят состояние.
#[derive(Debug, Clone)]
pub struct Gauge {
    value: Arc<Mutex<f64>>,
}

impl Gauge {
    fn new() -> Self {
        Self {
            value: Arc::new(Mutex::new(0.0)),
        }
    }

    /// Установить значение.
    pub fn set(&self, v: f64) {
        *lock(&self.value) = v;
    }

    /// Увеличить на 1.
    pub fn inc(&self) {
        self.add(1.0);
    }

    /// Уменьшить на 1.
    pub fn dec(&self) {
        self.sub(1.0);
    }

    /// Увеличить на `delta`.
    pub fn add(&self, delta: f64) {
        *lock(&self.value) += delta;
    }

    /// Уменьшить на `delta`.
    pub fn sub(&self, delta: f64) {
        *lock(&self.value) -= delta;
    }

    /// Текущее значение.
    pub fn get(&self) -> f64 {
        *lock(&self.value)
    }
}

/// Внутреннее состояние гистограммы: резервуар значений и точные
/// потоковые статистики по всем записям.
#[derive(Debug)]
struct HistogramData {
    /// резервуар: все значения, пока их <= [`RESERVOIR_CAP`], далее —
    /// равномерная случайная выборка (algorithm R)
    values: Vec<f64>,
    /// всего записано наблюдений (точно, включая вытесненные из резервуара)
    count: u64,
    /// сумма всех записанных значений (точная в пределах f64)
    sum: f64,
    min: Option<f64>,
    max: Option<f64>,
    rng: SmallRng,
}

/// Гистограмма наблюдений `f64` (длительности, размеры ответов).
///
/// `count`/`sum`/`min`/`max` — точные по всем записям; перцентили
/// (`p50`/`p95`/[`Histogram::percentile`]) считаются по резервуару
/// не более [`RESERVOIR_CAP`] значений с линейной интерполяцией.
/// Хендлы делят состояние через `Arc`.
#[derive(Debug, Clone)]
pub struct Histogram {
    inner: Arc<Mutex<HistogramData>>,
}

impl Histogram {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HistogramData {
                values: Vec::new(),
                count: 0,
                sum: 0.0,
                min: None,
                max: None,
                rng: SmallRng::seeded(),
            })),
        }
    }

    /// Записать наблюдение. `count`/`sum`/`min`/`max` обновляются всегда;
    /// в резервуар значение попадает гарантированно, пока тот не заполнен,
    /// далее — с вероятностью `RESERVOIR_CAP / count` (algorithm R).
    /// NaN не ломает `min`/`max` (`f64::min`/`max` игнорируют NaN-операнд).
    pub fn record(&self, value: f64) {
        let mut d = lock(&self.inner);
        d.count += 1;
        d.sum += value;
        d.min = Some(d.min.map_or(value, |m| m.min(value)));
        d.max = Some(d.max.map_or(value, |m| m.max(value)));
        if d.values.len() < RESERVOIR_CAP {
            d.values.push(value);
        } else {
            // algorithm R: i-е значение (i = count, 1-based) с вероятностью
            // CAP / i замещает случайный элемент резервуара.
            let i = d.count;
            let j = d.rng.below(i) as usize;
            if j < RESERVOIR_CAP {
                d.values[j] = value;
            }
        }
    }

    /// Сколько всего наблюдений записано (точно).
    pub fn count(&self) -> u64 {
        lock(&self.inner).count
    }

    /// Сумма всех записанных значений.
    pub fn sum(&self) -> f64 {
        lock(&self.inner).sum
    }

    /// Минимум; `None`, если наблюдений ещё не было.
    pub fn min(&self) -> Option<f64> {
        lock(&self.inner).min
    }

    /// Максимум; `None`, если наблюдений ещё не было.
    pub fn max(&self) -> Option<f64> {
        lock(&self.inner).max
    }

    /// Среднее (`sum / count`); `None` для пустой гистограммы.
    pub fn avg(&self) -> Option<f64> {
        let d = lock(&self.inner);
        if d.count == 0 {
            None
        } else {
            Some(d.sum / d.count as f64)
        }
    }

    /// Сколько значений сейчас хранится в резервуаре (<= [`RESERVOIR_CAP`]).
    pub fn reservoir_len(&self) -> usize {
        lock(&self.inner).values.len()
    }

    /// Перцентиль `p` (в процентах, зажимается в `[0, 100]`) по резервуару
    /// с линейной интерполяцией; `None` для пустой гистограммы.
    pub fn percentile(&self, p: f64) -> Option<f64> {
        percentile_of(&lock(&self.inner).values, p)
    }

    /// Медиана (50-й перцентиль); `None` для пустой гистограммы.
    pub fn p50(&self) -> Option<f64> {
        self.percentile(50.0)
    }

    /// 95-й перцентиль; `None` для пустой гистограммы.
    pub fn p95(&self) -> Option<f64> {
        self.percentile(95.0)
    }

    /// Запустить RAII-таймер: при drop guard запишет в эту гистограмму
    /// прошедшее время в миллисекундах.
    pub fn timer(&self) -> Timer {
        Timer {
            histogram: self.clone(),
            start: Instant::now(),
        }
    }

    /// Полный срез статистик (для снапшота и Prometheus-экспорта).
    fn snapshot(&self) -> HistogramSnapshot {
        let d = lock(&self.inner);
        HistogramSnapshot {
            count: d.count,
            sum: d.sum,
            min: d.min,
            max: d.max,
            avg: if d.count == 0 {
                None
            } else {
                Some(d.sum / d.count as f64)
            },
            p50: percentile_of(&d.values, 50.0),
            p95: percentile_of(&d.values, 95.0),
            reservoir_len: d.values.len(),
        }
    }
}

/// Перцентиль по выборке с линейной интерполяцией между ближайшими рангами
/// (метод numpy «linear»); `None` для пустой выборки. `p` зажимается в
/// `[0, 100]`. Сортировка — `total_cmp`, NaN не приводит к панике.
fn percentile_of(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let p = p.clamp(0.0, 100.0);
    let rank = p / 100.0 * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    let frac = rank - lo as f64;
    Some(sorted[lo] + (sorted[hi] - sorted[lo]) * frac)
}

/// RAII-guard замера длительности: при drop записывает в гистограмму
/// `elapsed` в миллисекундах. Создаётся через [`Histogram::timer`].
#[derive(Debug)]
pub struct Timer {
    histogram: Histogram,
    start: Instant,
}

impl Timer {
    /// Сколько времени прошло с создания таймера.
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Остановить явно и вернуть длительность; запись в гистограмму
    /// произойдёт при drop (ровно один раз).
    pub fn stop(self) -> Duration {
        let elapsed = self.elapsed();
        drop(self);
        elapsed
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.histogram.record(self.start.elapsed().as_secs_f64() * 1000.0);
    }
}

/// Срез статистик одной гистограммы. `min`/`max`/`avg`/`p50`/`p95` —
/// `None`, если наблюдений не было.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistogramSnapshot {
    /// всего записано наблюдений (точно)
    pub count: u64,
    /// сумма всех значений
    pub sum: f64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub avg: Option<f64>,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    /// размер резервуара на момент снапшота (<= [`RESERVOIR_CAP`])
    pub reservoir_len: usize,
}

/// Полный срез метрик реестра: все счётчики, датчики и гистограммы.
/// Ключи — полные имена метрик как регистрировались, включая
/// `{k=v}`-метки. Сериализуется через serde (JSON и др.).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub counters: BTreeMap<String, u64>,
    pub gauges: BTreeMap<String, f64>,
    pub histograms: BTreeMap<String, HistogramSnapshot>,
}

/// Ряды одного санитизированного имени при экспорте: список пар
/// «метки — значение» (у одного базового имени может быть несколько
/// наборов меток).
type SeriesMap<T> = BTreeMap<String, Vec<(BTreeMap<String, String>, T)>>;

/// Реестр метрик сессии: выдаёт хендлы по имени, собирает снапшоты и
/// экспорт. Без глобального состояния — создаётся на сессию и передаётся
/// в точки инструментации. Повторный запрос того же имени возвращает
/// хендл к той же метрике.
#[derive(Debug, Default)]
pub struct MetricsRegistry {
    counters: Mutex<BTreeMap<String, Counter>>,
    gauges: Mutex<BTreeMap<String, Gauge>>,
    histograms: Mutex<BTreeMap<String, Histogram>>,
}

impl MetricsRegistry {
    /// Пустой реестр.
    pub fn new() -> Self {
        Self::default()
    }

    /// Счётчик по имени; при повторном вызове — хендл к тому же счётчику.
    /// Имя может нести метки: `"tool.exec{tool=bash}"`.
    pub fn counter(&self, name: &str) -> Counter {
        lock(&self.counters)
            .entry(name.to_owned())
            .or_insert_with(Counter::new)
            .clone()
    }

    /// Датчик по имени; при повторном вызове — хендл к тому же датчику.
    pub fn gauge(&self, name: &str) -> Gauge {
        lock(&self.gauges)
            .entry(name.to_owned())
            .or_insert_with(Gauge::new)
            .clone()
    }

    /// Гистограмма по имени; при повторном вызове — хендл к той же
    /// гистограмме.
    pub fn histogram(&self, name: &str) -> Histogram {
        lock(&self.histograms)
            .entry(name.to_owned())
            .or_insert_with(Histogram::new)
            .clone()
    }

    /// Структурный срез всех метрик (serde).
    pub fn snapshot(&self) -> MetricsSnapshot {
        let counters = lock(&self.counters)
            .iter()
            .map(|(k, c)| (k.clone(), c.get()))
            .collect();
        let gauges = lock(&self.gauges)
            .iter()
            .map(|(k, g)| (k.clone(), g.get()))
            .collect();
        let histograms = lock(&self.histograms)
            .iter()
            .map(|(k, h)| (k.clone(), h.snapshot()))
            .collect();
        MetricsSnapshot {
            counters,
            gauges,
            histograms,
        }
    }

    /// Экспорт всех метрик в формате Prometheus text exposition:
    /// строки `# TYPE <имя> <counter|gauge|summary>`, затем значения.
    /// Гистограммы выводятся как `summary`: квантили 0.5 и 0.95 (если было
    /// хотя бы одно наблюдение), плюс `<имя>_sum` и `<имя>_count`.
    /// Имена санитизируются ([`sanitize_name`]), метки из `{k=v}`-части
    /// имени выводятся в фигурных скобках. Пустой реестр — пустая строка.
    pub fn export_prometheus(&self) -> String {
        // Схлопываем ряды с одинаковым санитизированным именем, чтобы
        // строка # TYPE встречалась ровно один раз на имя метрики.
        let mut counters: SeriesMap<u64> = BTreeMap::new();
        for (key, counter) in lock(&self.counters).iter() {
            let (base, labels) = parse_metric_name(key);
            counters
                .entry(sanitize_name(&base))
                .or_default()
                .push((labels, counter.get()));
        }
        let mut gauges: SeriesMap<f64> = BTreeMap::new();
        for (key, gauge) in lock(&self.gauges).iter() {
            let (base, labels) = parse_metric_name(key);
            gauges
                .entry(sanitize_name(&base))
                .or_default()
                .push((labels, gauge.get()));
        }
        let mut histograms: SeriesMap<HistogramSnapshot> = BTreeMap::new();
        for (key, histogram) in lock(&self.histograms).iter() {
            let (base, labels) = parse_metric_name(key);
            histograms
                .entry(sanitize_name(&base))
                .or_default()
                .push((labels, histogram.snapshot()));
        }

        let mut out = String::new();
        for (name, series) in &counters {
            let _ = writeln!(out, "# TYPE {name} counter");
            for (labels, value) in series {
                let _ = writeln!(out, "{name}{} {value}", render_labels(labels, None));
            }
        }
        for (name, series) in &gauges {
            let _ = writeln!(out, "# TYPE {name} gauge");
            for (labels, value) in series {
                let _ = writeln!(out, "{name}{} {value}", render_labels(labels, None));
            }
        }
        for (name, series) in &histograms {
            let _ = writeln!(out, "# TYPE {name} summary");
            for (labels, snap) in series {
                if let Some(p50) = snap.p50 {
                    let q = render_labels(labels, Some(("quantile", "0.5")));
                    let _ = writeln!(out, "{name}{q} {p50}");
                }
                if let Some(p95) = snap.p95 {
                    let q = render_labels(labels, Some(("quantile", "0.95")));
                    let _ = writeln!(out, "{name}{q} {p95}");
                }
                let plain = render_labels(labels, None);
                let _ = writeln!(out, "{name}_sum{plain} {}", snap.sum);
                let _ = writeln!(out, "{name}_count{plain} {}", snap.count);
            }
        }
        out
    }
}

/// Разбор полного имени метрики `base{k1=v1,k2=v2}` на базовое имя и метки
/// (отсортированы, т.к. `BTreeMap`). Парсер терпим: пробелы вокруг имён и
/// значений обрезаются, пары без `=` пропускаются; если строка не оканчивается
/// на `}`, всё считается базовым именем без меток.
pub fn parse_metric_name(full: &str) -> (String, BTreeMap<String, String>) {
    let mut labels = BTreeMap::new();
    let Some(open) = full.find('{') else {
        return (full.trim().to_owned(), labels);
    };
    if !full.ends_with('}') {
        return (full.trim().to_owned(), labels);
    }
    let base = full[..open].trim().to_owned();
    for pair in full[open + 1..full.len() - 1].split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        if let Some((k, v)) = pair.split_once('=') {
            labels.insert(k.trim().to_owned(), v.trim().to_owned());
        }
    }
    (base, labels)
}

/// Базовое имя под правило Prometheus `[a-zA-Z_:][a-zA-Z0-9_:]*`:
/// недопустимые символы заменяются на `_`, ведущая цифра получает
/// префикс `_`.
fn sanitize_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == ':' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// Экранирование значения метки по правилам Prometheus: `\`, `"` и перевод
/// строки.
fn escape_label_value(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Рендер меток в `{k="v",k2="v2"}` (пустая строка для пустого набора).
/// `extra` — дополнительная метка, добавляемая последней (квантиль summary).
fn render_labels(labels: &BTreeMap<String, String>, extra: Option<(&str, &str)>) -> String {
    let mut parts: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape_label_value(v)))
        .collect();
    if let Some((k, v)) = extra {
        parts.push(format!("{k}=\"{v}\""));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", parts.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_inc_add_get() {
        let c = Counter::new();
        assert_eq!(c.get(), 0);
        c.inc();
        c.add(41);
        assert_eq!(c.get(), 42);
    }

    #[test]
    fn registry_returns_shared_handles() {
        let reg = MetricsRegistry::new();
        reg.counter("requests").inc();
        reg.counter("requests").add(2);
        // повторный запрос имени — тот же счётчик
        assert_eq!(reg.counter("requests").get(), 3);
        // другое имя — другой счётчик
        assert_eq!(reg.counter("other").get(), 0);
        // метки — часть ключа
        reg.counter("tool.exec{tool=bash}").inc();
        assert_eq!(reg.counter("tool.exec{tool=bash}").get(), 1);
        assert_eq!(reg.counter("tool.exec{tool=sh}").get(), 0);
    }

    #[test]
    fn gauge_set_inc_dec() {
        let g = Gauge::new();
        assert_eq!(g.get(), 0.0);
        g.set(10.0);
        g.inc();
        g.add(0.5);
        g.dec();
        g.sub(0.25);
        assert_eq!(g.get(), 10.25);
        // отрицательные значения допустимы
        g.set(-3.0);
        assert_eq!(g.get(), -3.0);
    }

    #[test]
    fn histogram_basic_stats() {
        let h = Histogram::new();
        for v in [1.0, 2.0, 3.0, 4.0] {
            h.record(v);
        }
        assert_eq!(h.count(), 4);
        assert_eq!(h.sum(), 10.0);
        assert_eq!(h.min(), Some(1.0));
        assert_eq!(h.max(), Some(4.0));
        assert_eq!(h.avg(), Some(2.5));
        assert_eq!(h.reservoir_len(), 4);
    }

    #[test]
    fn histogram_empty_stats() {
        let h = Histogram::new();
        assert_eq!(h.count(), 0);
        assert_eq!(h.sum(), 0.0);
        assert_eq!(h.min(), None);
        assert_eq!(h.max(), None);
        assert_eq!(h.avg(), None);
        assert_eq!(h.p50(), None);
        assert_eq!(h.p95(), None);
        assert_eq!(h.reservoir_len(), 0);
    }

    #[test]
    fn histogram_single_value_percentiles() {
        let h = Histogram::new();
        h.record(7.5);
        assert_eq!(h.p50(), Some(7.5));
        assert_eq!(h.p95(), Some(7.5));
        assert_eq!(h.percentile(0.0), Some(7.5));
        assert_eq!(h.percentile(100.0), Some(7.5));
    }

    #[test]
    fn histogram_percentile_interpolation() {
        let h = Histogram::new();
        for v in 1..=100 {
            h.record(v as f64);
        }
        // линейная интерполяция: p50 = 50.5, p95 = 95.05
        let p50 = h.p50().expect("p50 должен быть");
        let p95 = h.p95().expect("p95 должен быть");
        assert!((p50 - 50.5).abs() < 1e-9, "p50={p50}");
        assert!((p95 - 95.05).abs() < 1e-9, "p95={p95}");
        // p вне [0, 100] зажимается
        assert_eq!(h.percentile(150.0), h.max());
        assert_eq!(h.percentile(-5.0), h.min());
    }

    #[test]
    fn timer_records_elapsed_on_drop() {
        let h = Histogram::new();
        {
            // именно `let _t`, а не `let _` — иначе guard дропнется немедленно
            let _t = h.timer();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(h.count(), 1);
        assert!(h.sum() >= 1.0, "elapsed должно быть > 0, получено {}", h.sum());
        assert!(h.min().expect("min после записи") >= 1.0);
    }

    #[test]
    fn timer_stop_returns_elapsed() {
        let h = Histogram::new();
        let t = h.timer();
        std::thread::sleep(Duration::from_millis(3));
        let elapsed = t.stop();
        assert!(elapsed >= Duration::from_millis(1));
        // запись произошла ровно один раз — при drop
        assert_eq!(h.count(), 1);
    }

    #[test]
    fn reservoir_downsampling_keeps_exact_count() {
        let h = Histogram::new();
        let n = 100_000u64;
        for v in 1..=n {
            h.record(v as f64);
        }
        // точные статистики по всем 100k значений
        assert_eq!(h.count(), n);
        assert_eq!(h.min(), Some(1.0));
        assert_eq!(h.max(), Some(n as f64));
        assert_eq!(h.sum(), 5_000_050_000.0);
        // резервуар ограничен
        assert_eq!(h.reservoir_len(), RESERVOIR_CAP);
        // p95 — приближение: истина 95_000, допуск 2500 (>> 3 сигм выборки)
        let p95 = h.p95().expect("p95 при 100k значений");
        let true_p95 = n as f64 * 0.95;
        assert!(
            (p95 - true_p95).abs() <= 2500.0,
            "p95={p95} слишком далеко от {true_p95}"
        );
    }

    #[test]
    fn parse_name_without_labels() {
        let (base, labels) = parse_metric_name("llm.tokens");
        assert_eq!(base, "llm.tokens");
        assert!(labels.is_empty());
    }

    #[test]
    fn parse_name_with_labels() {
        let (base, labels) = parse_metric_name("tool.exec{tool=bash, mode=fast}");
        assert_eq!(base, "tool.exec");
        assert_eq!(labels.len(), 2);
        assert_eq!(labels.get("tool"), Some(&"bash".to_owned()));
        assert_eq!(labels.get("mode"), Some(&"fast".to_owned()));
    }

    #[test]
    fn parse_name_malformed() {
        // нет закрывающей скобки — всё считается именем
        let (base, labels) = parse_metric_name("broken{tool=bash");
        assert_eq!(base, "broken{tool=bash");
        assert!(labels.is_empty());
        // пустой набор меток
        let (base, labels) = parse_metric_name("m{}");
        assert_eq!(base, "m");
        assert!(labels.is_empty());
        // пары без '=' пропускаются
        let (base, labels) = parse_metric_name("m{a=1,junk,b=2}");
        assert_eq!(base, "m");
        assert_eq!(labels.len(), 2);
    }

    #[test]
    fn sanitize_name_rules() {
        assert_eq!(sanitize_name("tool.exec"), "tool_exec");
        assert_eq!(sanitize_name("ns:ok_name"), "ns:ok_name");
        assert_eq!(sanitize_name("9lives"), "_9lives");
        assert_eq!(sanitize_name("a-b c"), "a_b_c");
    }

    #[test]
    fn prometheus_export_format() {
        let reg = MetricsRegistry::new();
        reg.counter("tool.exec{tool=bash}").add(7);
        reg.gauge("agent.inflight").set(3.0);
        let h = reg.histogram("tool.duration_ms{tool=bash}");
        h.record(10.0);
        h.record(20.0);

        let text = reg.export_prometheus();
        // TYPE-строки с санитизированными именами
        assert!(text.contains("# TYPE tool_exec counter"));
        assert!(text.contains("# TYPE agent_inflight gauge"));
        assert!(text.contains("# TYPE tool_duration_ms summary"));
        // значения и метки
        assert!(text.contains("tool_exec{tool=\"bash\"} 7"));
        assert!(text.contains("agent_inflight 3"));
        assert!(text.contains("tool_duration_ms_sum{tool=\"bash\"} 30"));
        assert!(text.contains("tool_duration_ms_count{tool=\"bash\"} 2"));
        assert!(text.contains("tool_duration_ms{tool=\"bash\",quantile=\"0.95\"}"));

        // каждая строка — валидная строка формата exposition
        let metric_re = regex::Regex::new(
            r#"^[a-zA-Z_:][a-zA-Z0-9_:]*(\{[a-zA-Z_][a-zA-Z0-9_]*="([^"\\]|\\.)*"(,[a-zA-Z_][a-zA-Z0-9_]*="([^"\\]|\\.)*")*\})? [-+0-9.eEnaIf]+$"#,
        )
        .expect("metric regex");
        let type_re = regex::Regex::new(
            r"^# TYPE [a-zA-Z_:][a-zA-Z0-9_:]* (counter|gauge|summary)$",
        )
        .expect("type regex");
        let mut lines = 0;
        for line in text.lines() {
            lines += 1;
            assert!(
                metric_re.is_match(line) || type_re.is_match(line),
                "невалидная строка exposition: {line:?}"
            );
        }
        assert!(lines >= 8, "ожидалось >= 8 строк, получено {lines}");
    }

    #[test]
    fn prometheus_label_escaping() {
        let reg = MetricsRegistry::new();
        reg.counter("m{path=a\"b\\c}").inc();
        let text = reg.export_prometheus();
        assert!(text.contains("m{path=\"a\\\"b\\\\c\"} 1"));
    }

    #[test]
    fn prometheus_export_empty_registry() {
        assert_eq!(MetricsRegistry::new().export_prometheus(), "");
    }

    #[test]
    fn snapshot_contents() {
        let reg = MetricsRegistry::new();
        reg.counter("a").add(5);
        reg.gauge("g").set(1.5);
        reg.histogram("h").record(2.0);
        let snap = reg.snapshot();
        assert_eq!(snap.counters.get("a"), Some(&5));
        assert_eq!(snap.gauges.get("g"), Some(&1.5));
        let hs = snap.histograms.get("h").expect("histogram h");
        assert_eq!(hs.count, 1);
        assert_eq!(hs.min, Some(2.0));
        assert_eq!(hs.max, Some(2.0));
        assert_eq!(hs.p50, Some(2.0));
        assert_eq!(hs.p95, Some(2.0));
        assert_eq!(hs.reservoir_len, 1);
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let reg = MetricsRegistry::new();
        reg.counter("tool.exec{tool=bash}").inc();
        reg.gauge("queue").set(2.5);
        let h = reg.histogram("lat");
        h.record(1.0);
        h.record(3.0);

        let snap = reg.snapshot();
        let json = serde_json::to_string_pretty(&snap).expect("serialize");
        let back: MetricsSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(snap, back);
    }

    #[test]
    fn snapshot_empty_registry() {
        let reg = MetricsRegistry::new();
        let snap = reg.snapshot();
        assert!(snap.counters.is_empty());
        assert!(snap.gauges.is_empty());
        assert!(snap.histograms.is_empty());
        let json = serde_json::to_string(&snap).expect("serialize");
        assert!(json.contains("counters"));
        assert!(json.contains("histograms"));
    }
}
