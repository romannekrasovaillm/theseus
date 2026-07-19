//! Матрица повторов (retry) для вызовов внешних LLM-API.
//!
//! Уроки обзора `codex-rs` (`core/src/util.rs::backoff`, `core/src/client.rs`):
//! повторять имеет смысл только то, что может починиться само, — 429
//! (rate limit), 5xx и транспортные сбои сети. Ошибки 401/403 (аутентификация)
//! и 400 (битый запрос) повторять бессмысленно: запрос надо чинить, а не
//! штурмовать API. Задержки между попытками — экспонента `base * 2^n` с капом
//! и джиттером; в отличие от codex (там недетерминированный `rand::rng()`),
//! джиттер здесь детерминированный: инжектируемый `u64`-seed и SplitMix64,
//! поэтому тесты и воспроизведение инцидентов детерминированы.
//!
//! Матрица политик (см. [`RetryPolicy::for_kind`]):
//!
//! | Вид ошибки   | Ретрай? | Попыток | База, мс | Кап, мс | Джиттер |
//! |--------------|---------|---------|----------|---------|---------|
//! | `RateLimit`  | да      | 8       | 2000     | 120000  | 25%     |
//! | `Server5xx`  | да      | 5       | 500      | 30000   | 20%     |
//! | `Network`    | да      | 5       | 250      | 10000   | 20%     |
//! | `Unknown`    | да      | 3       | 1000     | 8000    | 10%     |
//! | `Auth`       | нет     | —       | —        | —       | —       |
//! | `BadRequest` | нет     | —       | —        | —       | —       |
//!
//! Типичный цикл вызова: ошибка → [`classify`] → [`should_retry`] →
//! [`RetryPolicy::delays`] (или [`retry_delays`]) → сон на выданную задержку.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// Класс ошибки API-вызова — решает, ретраить ли и с какой политикой.
///
/// Классификацию выполняет [`classify`]; матрицу повторов задаёт
/// [`RetryPolicy::for_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorKind {
    /// HTTP 429 (Too Many Requests): превышен лимит запросов/токенов.
    /// Повторять с самым терпеливым backoff'ом — лимит обычно скользящий.
    RateLimit,
    /// HTTP 5xx: внутренняя ошибка/перегрузка сервера. Повторять.
    Server5xx,
    /// Транспортный сбой: таймаут, обрыв соединения, DNS. Повторять.
    Network,
    /// HTTP 401/403: аутентификация/авторизация. НЕ повторять — ключ,
    /// права или тариф чинятся вне цикла ретраев.
    Auth,
    /// HTTP 400 и прочие 4xx: запрос некорректен. НЕ повторять —
    /// повтор того же тела даст тот же отказ.
    BadRequest,
    /// Не удалось классифицировать. Повторять осторожно (короткая политика).
    Unknown,
}

impl ErrorKind {
    /// Все варианты классификации — удобно для табличных тестов и метрик.
    pub const ALL: [ErrorKind; 6] = [
        ErrorKind::RateLimit,
        ErrorKind::Server5xx,
        ErrorKind::Network,
        ErrorKind::Auth,
        ErrorKind::BadRequest,
        ErrorKind::Unknown,
    ];

    /// Ретраибелен ли этот класс ошибок (единый источник истины — матрица
    /// [`RetryPolicy::for_kind`], чтобы флаг не расходился с политикой).
    pub fn is_retryable(self) -> bool {
        RetryPolicy::for_kind(self).is_some()
    }

    /// Стабильное машинное имя класса (логи, метрики, трассировка).
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::RateLimit => "rate_limit",
            ErrorKind::Server5xx => "server_5xx",
            ErrorKind::Network => "network",
            ErrorKind::Auth => "auth",
            ErrorKind::BadRequest => "bad_request",
            ErrorKind::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Маркеры rate limit в тексте ошибки (сравнение в lower-case).
const RATE_LIMIT_MARKERS: &[&str] = &[
    "rate limit",
    "rate_limit",
    "ratelimit",
    "too many requests",
    "quota exceeded",
    "throttl", // throttled / throttling
];

/// Маркеры серверных 5xx в тексте ошибки (сравнение в lower-case).
/// Проверяются РАНЬШЕ сетевых, чтобы «gateway timeout» (504) не улетал
/// в `Network` из-за слова «timeout».
const SERVER_MARKERS: &[&str] = &[
    "internal server error",
    "bad gateway",
    "service unavailable",
    "gateway timeout",
    "server error",
    "overloaded",
    "at capacity",
];

/// Маркеры транспортных сбоев в тексте ошибки (сравнение в lower-case).
const NETWORK_MARKERS: &[&str] = &[
    "timed out",
    "timeout",
    "connection refused",
    "connection reset",
    "connection closed",
    "connection error",
    "connect error",
    "dns",
    "name resolution",
    "network unreachable",
    "broken pipe",
    "transport",
    "unexpected eof",
];

/// Маркеры ошибок аутентификации/авторизации (сравнение в lower-case).
const AUTH_MARKERS: &[&str] = &[
    "unauthorized",
    "forbidden",
    "invalid api key",
    "invalid_api_key",
    "authentication",
    "access denied",
    "permission denied",
];

/// Маркеры битого запроса (сравнение в lower-case).
const BAD_REQUEST_MARKERS: &[&str] =
    &["bad request", "invalid request", "malformed", "validation failed"];

/// Классифицирует ошибку API-вызова в [`ErrorKind`].
///
/// Приоритет у HTTP-статуса: если `status` известен, текст игнорируется —
/// статус однозначнее эвристик по строке. Классификация по статусу:
/// 429 → [`ErrorKind::RateLimit`], 401/403 → [`ErrorKind::Auth`],
/// 500..=599 → [`ErrorKind::Server5xx`], прочие 4xx → [`ErrorKind::BadRequest`]
/// (клиентская ошибка: повтор не поможет), всё остальное → [`ErrorKind::Unknown`].
///
/// Если статуса нет (типично для транспортных ошибок, где ответа от сервера
/// не было вовсе), текст `err_text` приводится к lower-case и ищется по
/// маркерам в порядке: rate limit → 5xx → сеть → auth → bad request.
/// Ничего не нашлось → [`ErrorKind::Unknown`].
///
/// Функция предназначена для вызова на уже случившейся ошибке; успешные
/// статусы (2xx/3xx) осмысленно не классифицируются и попадут в `Unknown`.
pub fn classify(status: Option<u16>, err_text: &str) -> ErrorKind {
    if let Some(code) = status {
        return classify_status(code);
    }
    classify_text(err_text)
}

/// Классификация по коду HTTP-статуса.
fn classify_status(code: u16) -> ErrorKind {
    match code {
        429 => ErrorKind::RateLimit,
        401 | 403 => ErrorKind::Auth,
        _ if (500..=599).contains(&code) => ErrorKind::Server5xx,
        _ if (400..=499).contains(&code) => ErrorKind::BadRequest,
        _ => ErrorKind::Unknown,
    }
}

/// Классификация по тексту ошибки (lower-case, эвристики-маркеры).
/// Порядок проверок значим: сначала специфичные (rate limit, 5xx),
/// потом широкие (сеть), потом auth/bad request.
fn classify_text(err_text: &str) -> ErrorKind {
    let text = err_text.to_lowercase();
    if contains_any(&text, RATE_LIMIT_MARKERS) {
        ErrorKind::RateLimit
    } else if contains_any(&text, SERVER_MARKERS) {
        ErrorKind::Server5xx
    } else if contains_any(&text, NETWORK_MARKERS) {
        ErrorKind::Network
    } else if contains_any(&text, AUTH_MARKERS) {
        ErrorKind::Auth
    } else if contains_any(&text, BAD_REQUEST_MARKERS) {
        ErrorKind::BadRequest
    } else {
        ErrorKind::Unknown
    }
}

/// Ищет хотя бы один маркер в строке.
fn contains_any(text: &str, markers: &[&str]) -> bool {
    markers.iter().any(|m| text.contains(m))
}

/// Политика повторов для класса ошибок: сколько раз пробовать и как ждать.
///
/// Значения матрицы — см. [`RetryPolicy::for_kind`]. Структура копируемая и
/// сериализуемая: её можно переопределять из конфига (toml/serde) и дешёво
/// передавать по значению.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Сколько всего попыток допустимо, ВКЛЮЧАЯ первую (не ретрай).
    /// Задержек между попытками, соответственно, `max_attempts - 1`.
    pub max_attempts: u32,
    /// Базовая задержка (мс) — задержка перед первым ретраем до джиттера.
    pub base_ms: u64,
    /// Верхняя граница задержки (мс): экспонента капается до джиттера.
    pub max_ms: u64,
    /// Ширина симметричного джиттера в процентах (0..=100, больше — клампится):
    /// итоговая задержка ∈ `[cap * (1 - p/100), cap * (1 + p/100)]`.
    pub jitter_pct: u8,
}

impl RetryPolicy {
    /// Матрица повторов: политика по классу ошибки.
    ///
    /// Возвращает `None` для [`ErrorKind::Auth`] и [`ErrorKind::BadRequest`] —
    /// эти ошибки не ретраятся никогда. Для остальных классов — статическая
    /// политика по таблице из документации модуля:
    ///
    /// - `RateLimit`: терпеливая (8 попыток, база 2 с, кап 120 с) — лимиты
    ///   обычно скользящие и сбрасываются за десятки секунд;
    /// - `Server5xx`: средняя (5 попыток, база 500 мс, кап 30 с);
    /// - `Network`: быстрая (5 попыток, база 250 мс, кап 10 с) — транспорт
    ///   либо жив, либо нет, долгие паузы не помогают;
    /// - `Unknown`: осторожная (3 попытки, база 1 с, кап 8 с).
    pub const fn for_kind(kind: ErrorKind) -> Option<Self> {
        match kind {
            ErrorKind::RateLimit => Some(RetryPolicy {
                max_attempts: 8,
                base_ms: 2_000,
                max_ms: 120_000,
                jitter_pct: 25,
            }),
            ErrorKind::Server5xx => Some(RetryPolicy {
                max_attempts: 5,
                base_ms: 500,
                max_ms: 30_000,
                jitter_pct: 20,
            }),
            ErrorKind::Network => Some(RetryPolicy {
                max_attempts: 5,
                base_ms: 250,
                max_ms: 10_000,
                jitter_pct: 20,
            }),
            ErrorKind::Unknown => Some(RetryPolicy {
                max_attempts: 3,
                base_ms: 1_000,
                max_ms: 8_000,
                jitter_pct: 10,
            }),
            ErrorKind::Auth | ErrorKind::BadRequest => None,
        }
    }

    /// Итератор задержек между попытками с детерминированным джиттером.
    ///
    /// Выдаёт ровно `max_attempts - 1` задержек (столько пауз нужно между
    /// `max_attempts` попытками); при `max_attempts <= 1` итератор пуст.
    /// `seed` инициализирует SplitMix64-генератор джиттера: одинаковый seed
    /// даёт одинаковую последовательность задержек.
    pub fn delays(self, seed: u64) -> Backoff {
        Backoff::new(self, seed)
    }

    /// Базовая задержка как [`Duration`].
    pub fn base_delay(self) -> Duration {
        Duration::from_millis(self.base_ms)
    }

    /// Кап задержки как [`Duration`] (до джиттера).
    pub fn max_delay(self) -> Duration {
        Duration::from_millis(self.max_ms)
    }
}

/// Решает, стоит ли делать ещё одну попытку после `attempt` провалившихся.
///
/// `attempt` — число уже завершившихся неудачных попыток (нумерация с 1:
/// после первой неудачи `attempt == 1`). Возвращает `true`, только если
/// класс ошибки ретраибелен (см. [`RetryPolicy::for_kind`]) и бюджет попыток
/// не исчерпан: `attempt < policy.max_attempts`.
///
/// Например, при `max_attempts == 5`: после неудач 1..=4 — `true` (ещё есть
/// попытки 2..=5), после 5-й — `false`. Для `Auth`/`BadRequest` — всегда
/// `false`, включая `attempt == 0`.
pub fn should_retry(attempt: u32, kind: ErrorKind) -> bool {
    RetryPolicy::for_kind(kind).is_some_and(|p| attempt < p.max_attempts)
}

/// Детерминированный генератор SplitMix64 — источник джиттера.
///
/// Выбран за тривиальную реализацию без внешних зависимостей, статистическое
/// качество, достаточное для джиттера, и строгую воспроизводимость по seed
/// (в отличие от `rand::rng()` в codex-rs, который делает тесты недетерминированными).
/// Следующее значение — биекция состояния, поэтому разные seed гарантированно
/// дают разные первые значения.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Создаёт генератор с заданным seed (любой `u64`, включая 0).
    pub fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    /// Следующее псевдослучайное `u64` (каноничный SplitMix64:
    /// константы Стаффорда, инкремент «золотого сечения»).
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Итератор задержек backoff'а: экспонента `base * 2^n` с капом и джиттером.
///
/// n — порядковый номер задержки с 0: `base, 2*base, 4*base, ...`, каждое
/// значение капается `max_ms` ДО наложения джиттера, затем сдвигается в
/// пределах ±`jitter_pct`% детерминированным SplitMix64. Итератор конечен:
/// выдаёт ровно `max_attempts - 1` элементов (реализует [`ExactSizeIterator`]).
///
/// Создаётся через [`RetryPolicy::delays`] или [`Backoff::new`].
#[derive(Debug, Clone)]
pub struct Backoff {
    policy: RetryPolicy,
    /// Сколько задержек уже выдано.
    yielded: u32,
    rng: SplitMix64,
}

impl Backoff {
    /// Создаёт итератор задержек по политике и seed джиттера.
    pub fn new(policy: RetryPolicy, seed: u64) -> Self {
        Backoff {
            policy,
            yielded: 0,
            rng: SplitMix64::new(seed),
        }
    }

    /// Сколько задержек осталось выдать.
    fn remaining(&self) -> u32 {
        self.policy
            .max_attempts
            .saturating_sub(1)
            .saturating_sub(self.yielded)
    }
}

impl Iterator for Backoff {
    type Item = Duration;

    fn next(&mut self) -> Option<Duration> {
        if self.remaining() == 0 {
            return None;
        }
        let n = self.yielded;
        self.yielded += 1;
        // Экспонента base * 2^n; степень клампим, умножение сатюрируем —
        // переполнение невозможно даже при экзотических политиках, а кап
        // всё равно обрежет результат.
        let raw = self
            .policy
            .base_ms
            .saturating_mul(2u64.saturating_pow(n.min(62)));
        let capped = raw.min(self.policy.max_ms);
        Some(Duration::from_millis(apply_jitter(
            capped,
            self.policy.jitter_pct,
            &mut self.rng,
        )))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = usize::try_from(self.remaining()).unwrap_or(0);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for Backoff {}

/// Накладывает симметричный джиттер ±`jitter_pct`% на задержку.
///
/// `jitter_pct` клампится к 100. Границы включительны; при нулевом размахе
/// (процент 0, база 0 или целочисленный span 0) джиттер не применяется и rng
/// не тратится — последовательность остаётся предсказуемой. Вся арифметика
/// сатюрирующая: переполнение исключено при любых входах.
fn apply_jitter(base_ms: u64, jitter_pct: u8, rng: &mut SplitMix64) -> u64 {
    let pct = u64::from(jitter_pct.min(100));
    if pct == 0 || base_ms == 0 {
        return base_ms;
    }
    let span = base_ms.saturating_mul(pct) / 100;
    if span == 0 {
        return base_ms;
    }
    let width = span.saturating_mul(2).saturating_add(1);
    let offset = rng.next_u64() % width;
    base_ms.saturating_sub(span).saturating_add(offset)
}

/// Комбинированный хелпер: политика по классу ошибки → её итератор задержек.
///
/// Возвращает `None` для неретраибельных классов (`Auth`, `BadRequest`).
pub fn retry_delays(kind: ErrorKind, seed: u64) -> Option<Backoff> {
    RetryPolicy::for_kind(kind).map(|p| p.delays(seed))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- классификация по HTTP-статусу ----

    #[test]
    fn classifies_429_as_rate_limit() {
        assert_eq!(classify(Some(429), ""), ErrorKind::RateLimit);
    }

    #[test]
    fn classifies_5xx_range_as_server_error() {
        for code in [500u16, 502, 503, 504, 599] {
            assert_eq!(
                classify(Some(code), ""),
                ErrorKind::Server5xx,
                "status: {code}"
            );
        }
    }

    #[test]
    fn classifies_401_and_403_as_auth() {
        assert_eq!(classify(Some(401), ""), ErrorKind::Auth);
        assert_eq!(classify(Some(403), ""), ErrorKind::Auth);
    }

    #[test]
    fn classifies_400_and_other_4xx_as_bad_request() {
        // 400 явно; прочие 4xx — тоже клиентские ошибки без ретрая.
        for code in [400u16, 404, 409, 422, 499] {
            assert_eq!(
                classify(Some(code), ""),
                ErrorKind::BadRequest,
                "status: {code}"
            );
        }
    }

    #[test]
    fn classifies_unusual_status_as_unknown() {
        // Успех/редирект/внерядовые коды — вне матрицы (418 — это 4xx,
        // он по правилу «прочие 4xx» уходит в BadRequest и здесь не проверяется).
        for code in [100u16, 200, 301, 600] {
            assert_eq!(classify(Some(code), ""), ErrorKind::Unknown, "status: {code}");
        }
    }

    // ---- классификация по тексту (статуса нет) ----

    #[test]
    fn classifies_timeout_text_as_network() {
        assert_eq!(classify(None, "request timed out"), ErrorKind::Network);
        assert_eq!(classify(None, "connect timeout"), ErrorKind::Network);
    }

    #[test]
    fn classifies_connection_errors_as_network() {
        for text in [
            "connection refused by peer",
            "Connection Reset by remote host",
            "dns error: name resolution failed",
            "broken pipe",
        ] {
            assert_eq!(classify(None, text), ErrorKind::Network, "text: {text}");
        }
    }

    #[test]
    fn classifies_gateway_timeout_as_server_not_network() {
        // «gateway timeout» содержит «timeout», но это 504 → Server5xx:
        // серверные маркеры проверяются раньше сетевых.
        assert_eq!(classify(None, "504 Gateway Timeout"), ErrorKind::Server5xx);
        assert_eq!(classify(None, "502 Bad Gateway"), ErrorKind::Server5xx);
        assert_eq!(
            classify(None, "503 Service Unavailable: server overloaded"),
            ErrorKind::Server5xx
        );
    }

    #[test]
    fn classifies_rate_limit_text() {
        for text in [
            "Rate limit exceeded for tokens per minute",
            "429 Too Many Requests",
            "quota exceeded, retry later",
            "request was throttled",
        ] {
            assert_eq!(classify(None, text), ErrorKind::RateLimit, "text: {text}");
        }
    }

    #[test]
    fn classifies_auth_text() {
        for text in [
            "401 Unauthorized",
            "invalid api key provided",
            "Authentication failed: token expired",
        ] {
            assert_eq!(classify(None, text), ErrorKind::Auth, "text: {text}");
        }
    }

    #[test]
    fn classifies_bad_request_text() {
        assert_eq!(classify(None, "400 Bad Request"), ErrorKind::BadRequest);
        assert_eq!(
            classify(None, "malformed JSON in request body"),
            ErrorKind::BadRequest
        );
    }

    #[test]
    fn classifies_garbage_and_empty_text_as_unknown() {
        assert_eq!(classify(None, ""), ErrorKind::Unknown);
        assert_eq!(classify(None, "something weird happened"), ErrorKind::Unknown);
    }

    // ---- приоритет и устойчивость классификации ----

    #[test]
    fn status_wins_over_text() {
        // Статус авторитетнее текста: 429 с текстом «bad request» — RateLimit.
        assert_eq!(classify(Some(429), "bad request"), ErrorKind::RateLimit);
        assert_eq!(classify(Some(503), "timeout"), ErrorKind::Server5xx);
        assert_eq!(classify(Some(401), "connection reset"), ErrorKind::Auth);
    }

    #[test]
    fn text_matching_is_case_insensitive() {
        assert_eq!(classify(None, "CONNECTION REFUSED"), ErrorKind::Network);
        assert_eq!(classify(None, "RATE LIMIT EXCEEDED"), ErrorKind::RateLimit);
    }

    // ---- матрица «ретраить / не ретраить» ----

    #[test]
    fn auth_and_bad_request_are_never_retried() {
        for kind in [ErrorKind::Auth, ErrorKind::BadRequest] {
            assert!(RetryPolicy::for_kind(kind).is_none(), "kind: {kind}");
            assert!(!kind.is_retryable(), "kind: {kind}");
            assert!(!should_retry(0, kind), "kind: {kind}");
            assert!(!should_retry(1, kind), "kind: {kind}");
            assert!(retry_delays(kind, 42).is_none(), "kind: {kind}");
        }
    }

    #[test]
    fn retryable_kinds_have_policies() {
        for kind in [
            ErrorKind::RateLimit,
            ErrorKind::Server5xx,
            ErrorKind::Network,
            ErrorKind::Unknown,
        ] {
            assert!(RetryPolicy::for_kind(kind).is_some(), "kind: {kind}");
            assert!(kind.is_retryable(), "kind: {kind}");
            assert!(should_retry(1, kind), "kind: {kind}");
        }
    }

    #[test]
    fn policy_matrix_matches_documented_values() {
        let rl = RetryPolicy::for_kind(ErrorKind::RateLimit).expect("rate limit policy");
        assert_eq!(rl.max_attempts, 8);
        assert_eq!(rl.base_ms, 2_000);
        assert_eq!(rl.max_ms, 120_000);
        assert_eq!(rl.jitter_pct, 25);

        let srv = RetryPolicy::for_kind(ErrorKind::Server5xx).expect("server policy");
        assert_eq!(srv.max_attempts, 5);
        assert_eq!(srv.base_ms, 500);
        assert_eq!(srv.max_ms, 30_000);

        let net = RetryPolicy::for_kind(ErrorKind::Network).expect("network policy");
        assert_eq!(net.max_attempts, 5);
        assert_eq!(net.base_ms, 250);
        assert_eq!(net.max_ms, 10_000);

        let unk = RetryPolicy::for_kind(ErrorKind::Unknown).expect("unknown policy");
        assert_eq!(unk.max_attempts, 3);
        assert_eq!(unk.base_ms, 1_000);
        assert_eq!(unk.max_ms, 8_000);
    }

    #[test]
    fn should_retry_respects_max_attempts_boundaries() {
        let net = RetryPolicy::for_kind(ErrorKind::Network).expect("network policy");
        // До исчерпания бюджета — true, на границе и за ней — false.
        assert!(should_retry(1, ErrorKind::Network));
        assert!(should_retry(net.max_attempts - 1, ErrorKind::Network));
        assert!(!should_retry(net.max_attempts, ErrorKind::Network));
        assert!(!should_retry(net.max_attempts + 10, ErrorKind::Network));
    }

    // ---- Backoff: экспонента, кап, длина ----

    /// Политика без джиттера — задержки точные.
    fn exact_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 5,
            base_ms: 100,
            max_ms: 450,
            jitter_pct: 0,
        }
    }

    #[test]
    fn backoff_yields_max_attempts_minus_one_delays() {
        let policy = exact_policy();
        let delays: Vec<Duration> = policy.delays(42).collect();
        assert_eq!(delays.len(), usize::try_from(policy.max_attempts).unwrap_or(0) - 1);
        // ExactSizeIterator согласован с реальным числом элементов.
        assert_eq!(policy.delays(42).len(), delays.len());
    }

    #[test]
    fn zero_or_one_attempt_policy_yields_no_delays() {
        let zero = RetryPolicy { max_attempts: 0, ..exact_policy() };
        let one = RetryPolicy { max_attempts: 1, ..exact_policy() };
        assert_eq!(zero.delays(1).count(), 0);
        assert_eq!(one.delays(1).count(), 0);
        assert_eq!(zero.delays(1).next(), None);
    }

    #[test]
    fn backoff_without_jitter_is_exact_exponential() {
        // base * 2^n: 100, 200, 400, далее кап 450 (800 → 450).
        let delays: Vec<Duration> = exact_policy().delays(7).collect();
        assert_eq!(
            delays,
            vec![
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(400),
                Duration::from_millis(450),
            ]
        );
    }

    #[test]
    fn backoff_caps_at_max_ms_and_stays_there() {
        let policy = RetryPolicy {
            max_attempts: 8,
            base_ms: 10_000,
            max_ms: 25_000,
            jitter_pct: 0,
        };
        let delays: Vec<u64> = policy.delays(3).map(|d| d.as_millis() as u64).collect();
        assert_eq!(
            delays,
            vec![10_000, 20_000, 25_000, 25_000, 25_000, 25_000, 25_000]
        );
    }

    #[test]
    fn backoff_handles_huge_base_without_overflow() {
        // base рядом с u64::MAX: сатюрирующая арифметика не паникует, кап работает.
        let policy = RetryPolicy {
            max_attempts: 4,
            base_ms: u64::MAX / 2,
            max_ms: 5_000,
            jitter_pct: 0,
        };
        let delays: Vec<Duration> = policy.delays(9).collect();
        assert_eq!(delays.len(), 3);
        assert!(delays.iter().all(|d| *d == Duration::from_millis(5_000)));
    }

    // ---- джиттер ----

    #[test]
    fn jitter_stays_within_pct_bounds() {
        let policy = RetryPolicy {
            max_attempts: 6,
            base_ms: 1_000,
            max_ms: 10_000,
            jitter_pct: 20,
        };
        // Капы до джиттера: 1000, 2000, 4000, 8000, 10000.
        let caps = [1_000u64, 2_000, 4_000, 8_000, 10_000];
        for seed in 0..32u64 {
            for (i, d) in policy.delays(seed).enumerate() {
                let cap = caps[i];
                let span = cap * 20 / 100;
                let ms = d.as_millis() as u64;
                assert!(
                    (cap - span..=cap + span).contains(&ms),
                    "seed: {seed}, step: {i}, delay: {ms}, cap: {cap}"
                );
            }
        }
    }

    #[test]
    fn jitter_actually_varies_delays() {
        let policy = RetryPolicy {
            max_attempts: 4,
            base_ms: 1_000,
            max_ms: 4_000,
            jitter_pct: 25,
        };
        // При ненулевом джиттере хоть какая-то задержка отклонится от капа.
        let exact = [1_000u64, 2_000, 4_000];
        let varied: Vec<u64> = policy.delays(123).map(|d| d.as_millis() as u64).collect();
        assert!(varied.iter().zip(exact.iter()).any(|(a, b)| a != b));
    }

    #[test]
    fn jitter_pct_is_clamped_at_100() {
        // jitter_pct = 200 → кламп к 100: задержка ∈ [0, 2 * cap].
        let policy = RetryPolicy {
            max_attempts: 3,
            base_ms: 1_000,
            max_ms: 2_000,
            jitter_pct: 200,
        };
        let caps = [1_000u64, 2_000];
        for (i, d) in policy.delays(5).enumerate() {
            let ms = d.as_millis() as u64;
            assert!(ms <= 2 * caps[i], "step: {i}, delay: {ms}");
        }
    }

    // ---- детерминизм ----

    #[test]
    fn backoff_is_deterministic_for_same_seed() {
        let policy = RetryPolicy::for_kind(ErrorKind::RateLimit).expect("rate limit policy");
        let a: Vec<Duration> = policy.delays(0xDEAD_BEEF).collect();
        let b: Vec<Duration> = policy.delays(0xDEAD_BEEF).collect();
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn splitmix64_first_output_matches_reference_vector() {
        // Каноничный эталон SplitMix64 для seed 0 (Стаффорд).
        assert_eq!(SplitMix64::new(0).next_u64(), 0xE220_A839_7B1D_CDAF);
    }

    #[test]
    fn splitmix64_streams_differ_for_different_seeds() {
        // Выход — биекция состояния: разные seed гарантированно дают
        // разные первые значения и разные последовательности.
        let mut a = SplitMix64::new(1);
        let mut b = SplitMix64::new(2);
        let seq_a: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        let seq_b: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
        assert_ne!(seq_a, seq_b);
        // А тот же seed — ту же последовательность.
        let mut c = SplitMix64::new(1);
        let seq_c: Vec<u64> = (0..8).map(|_| c.next_u64()).collect();
        assert_eq!(seq_a, seq_c);
    }

    // ---- вспомогательные API ----

    #[test]
    fn retry_delays_combines_lookup_and_backoff() {
        let backoff = retry_delays(ErrorKind::Server5xx, 11).expect("retryable kind");
        assert_eq!(backoff.len(), 4); // 5 попыток → 4 задержки
        assert!(retry_delays(ErrorKind::Auth, 11).is_none());
    }

    #[test]
    fn error_kind_display_matches_as_str() {
        for kind in ErrorKind::ALL {
            assert_eq!(format!("{kind}"), kind.as_str(), "kind: {kind:?}");
            assert!(!kind.as_str().is_empty());
        }
        // Имена стабильны (логи/метрики на них завязаны).
        assert_eq!(ErrorKind::RateLimit.as_str(), "rate_limit");
        assert_eq!(ErrorKind::Unknown.as_str(), "unknown");
    }

    #[test]
    fn error_kind_all_covers_every_variant() {
        // ALL полон и без дублей: 6 уникальных имён.
        let mut names: Vec<&str> = ErrorKind::ALL.iter().map(|k| k.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 6);
    }

    #[test]
    fn serde_roundtrip_for_policy_and_kind() {
        let policy = RetryPolicy::for_kind(ErrorKind::Network).expect("network policy");
        let json = serde_json::to_string(&policy).expect("serialize policy");
        let back: RetryPolicy = serde_json::from_str(&json).expect("deserialize policy");
        assert_eq!(policy, back);

        let kind_json = serde_json::to_string(&ErrorKind::RateLimit).expect("serialize kind");
        assert_eq!(kind_json, "\"RateLimit\"");
        let kind_back: ErrorKind = serde_json::from_str(&kind_json).expect("deserialize kind");
        assert_eq!(kind_back, ErrorKind::RateLimit);
    }

    #[test]
    fn delay_accessors_match_fields() {
        let policy = RetryPolicy::for_kind(ErrorKind::Unknown).expect("unknown policy");
        assert_eq!(policy.base_delay(), Duration::from_millis(policy.base_ms));
        assert_eq!(policy.max_delay(), Duration::from_millis(policy.max_ms));
    }
}
