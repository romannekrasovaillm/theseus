//! Квоты ресурсов сессии (образец: `codex-rs` limits/quotas).
//!
//! Модуль ограничивает потребление четырёх ресурсов агентной сессии:
//!
//! - [`LimitKind::ApiCalls`] — вызовы API модели;
//! - [`LimitKind::Tokens`] — токены (prompt + completion суммарно);
//! - [`LimitKind::BashSeconds`] — суммарное время bash-команд, секунды;
//! - [`LimitKind::ToolCalls`] — вызовы инструментов.
//!
//! Основные типы:
//!
//! - [`Quota`] — конфигурация лимитов: `Option<u64>` на каждый ресурс,
//!   `None` означает «без лимита». Собирается напрямую или через
//!   [`Quota::from_config`] из списка пар `(имя, значение)`.
//! - [`LimitGuard`] — исполнитель квот: накапливает фактический расход
//!   в `HashMap<LimitKind, u64>` и на каждое потребление атомарно проверяет
//!   границу. Методы `consume_*` возвращают `Result<(), LimitExceeded>`;
//!   при отказе расход **не** засчитывается (check-then-add).
//! - [`LimitExceeded`] — ошибка «квота исчерпана»: вид ресурса, лимит
//!   и фактический расход на момент отказа.
//! - [`LimitEvent`] — предупреждение из [`LimitGuard::check`]: ресурс
//!   израсходован строго больше чем на [`WARN_THRESHOLD_PCT`] процентов.
//!
//! Семантика границ строгая и зафиксирована тестами:
//!
//! - потребление разрешено, пока `used + n <= limit`: лимит срабатывает
//!   ровно на границе — дойти до `used == limit` можно, шаг за неё нельзя;
//! - предупреждение — при `used * 100 > limit * 80`, то есть строго больше
//!   80%: ровно 80% ещё не предупреждение;
//! - нулевой лимит (`Some(0)`) запрещает любое ненулевое потребление;
//! - `None` ничем не ограничивает и не участвует в `check`/`progress`.
//!
//! Модуль чистый: не знает про сеть, процессы и часы. Решение «сколько
//! секунд заняла команда» или «сколько токенов съел ответ модели» принимает
//! вызывающая сторона и сообщает через `consume_*`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// Порог предупреждения в процентах: [`LimitGuard::check`] сообщает
/// о ресурсах, израсходованных строго больше чем на это значение.
pub const WARN_THRESHOLD_PCT: u8 = 80;

// === Вид лимита ===

/// Вид квотируемого ресурса сессии.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    /// Вызовы API модели (каждый запрос к провайдеру).
    ApiCalls,
    /// Потреблённые токены (prompt + completion, суммарно).
    Tokens,
    /// Суммарное время выполнения bash-команд, секунды.
    BashSeconds,
    /// Вызовы инструментов (tool calls).
    ToolCalls,
}

impl LimitKind {
    /// Все виды в стабильном порядке — для детерминированных отчётов
    /// (`check`, `progress` всегда идут в этом порядке).
    pub const ALL: [LimitKind; 4] = [
        LimitKind::ApiCalls,
        LimitKind::Tokens,
        LimitKind::BashSeconds,
        LimitKind::ToolCalls,
    ];

    /// Строковое имя (совпадает с serde-представлением).
    pub fn as_str(self) -> &'static str {
        match self {
            LimitKind::ApiCalls => "api_calls",
            LimitKind::Tokens => "tokens",
            LimitKind::BashSeconds => "bash_seconds",
            LimitKind::ToolCalls => "tool_calls",
        }
    }

    /// Короткая русская метка для отчётов и сообщений об ошибках.
    pub fn label(self) -> &'static str {
        match self {
            LimitKind::ApiCalls => "API-вызовы",
            LimitKind::Tokens => "токены",
            LimitKind::BashSeconds => "секунды bash",
            LimitKind::ToolCalls => "вызовы инструментов",
        }
    }

    /// Разбор имени из конфигурации. Регистр и окаймляющие пробелы
    /// игнорируются; помимо канонических имён принимаются короткие алиасы:
    /// `api`, `token`/`tokens`, `bash`/`bash_secs`, `tool`/`tools`.
    /// Неизвестное имя — `None`.
    pub fn from_name(name: &str) -> Option<LimitKind> {
        match name.trim().to_ascii_lowercase().as_str() {
            "api_calls" | "api" => Some(LimitKind::ApiCalls),
            "tokens" | "token" => Some(LimitKind::Tokens),
            "bash_seconds" | "bash_secs" | "bash" => Some(LimitKind::BashSeconds),
            "tool_calls" | "tools" | "tool" => Some(LimitKind::ToolCalls),
            _ => None,
        }
    }
}

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// === Квота ===

/// Квоты ресурсов на сессию. `None` по полю — ресурс не ограничен;
/// `Some(0)` — ресурс полностью запрещён (любое ненулевое потребление
/// отклоняется).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quota {
    /// Максимум вызовов API модели.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_calls: Option<u64>,
    /// Максимум токенов (prompt + completion суммарно).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// Максимум суммарного времени bash-команд, секунд.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash_seconds: Option<u64>,
    /// Максимум вызовов инструментов.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u64>,
}

impl Quota {
    /// Квота «без лимитов»: все ресурсы не ограничены.
    pub fn unlimited() -> Quota {
        Quota::default()
    }

    /// Все ли ресурсы без лимита.
    pub fn is_unlimited(&self) -> bool {
        self.api_calls.is_none()
            && self.tokens.is_none()
            && self.bash_seconds.is_none()
            && self.tool_calls.is_none()
    }

    /// Лимит по виду ресурса (`None` — не ограничен).
    pub fn limit_for(&self, kind: LimitKind) -> Option<u64> {
        match kind {
            LimitKind::ApiCalls => self.api_calls,
            LimitKind::Tokens => self.tokens,
            LimitKind::BashSeconds => self.bash_seconds,
            LimitKind::ToolCalls => self.tool_calls,
        }
    }

    /// Собрать квоту из списка пар `(имя, значение)` — формат плоского
    /// конфига. Имена разбираются через [`LimitKind::from_name`] (регистр
    /// и пробелы не важны, алиасы допустимы); неизвестные имена молча
    /// пропускаются. При дубликатах побеждает последнее значение.
    pub fn from_config(entries: &[(&str, u64)]) -> Quota {
        let mut quota = Quota::unlimited();
        for &(name, value) in entries {
            let Some(kind) = LimitKind::from_name(name) else {
                continue;
            };
            match kind {
                LimitKind::ApiCalls => quota.api_calls = Some(value),
                LimitKind::Tokens => quota.tokens = Some(value),
                LimitKind::BashSeconds => quota.bash_seconds = Some(value),
                LimitKind::ToolCalls => quota.tool_calls = Some(value),
            }
        }
        quota
    }
}

// === Ошибка превышения ===

/// Ошибка: попытка потребить ресурс сверх квоты отклонена.
///
/// `used` — фактический расход на момент отказа; неудачная попытка
/// в расход **не** засчитана, поэтому `used <= limit` всегда.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitExceeded {
    /// Вид ресурса, по которому исчерпана квота.
    pub kind: LimitKind,
    /// Установленный лимит.
    pub limit: u64,
    /// Израсходовано на момент отказа.
    pub used: u64,
}

impl fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "квота исчерпана: {} ({}) — израсходовано {} из {}",
            self.kind,
            self.kind.label(),
            self.used,
            self.limit
        )
    }
}

impl std::error::Error for LimitExceeded {}

// === Предупреждение ===

/// Предупреждение о приближении к лимиту: ресурс израсходован строго
/// больше чем на [`WARN_THRESHOLD_PCT`] процентов. Событие выдаётся и для
/// полностью израсходованного ресурса (100% > 80%).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitEvent {
    /// Вид ресурса.
    pub kind: LimitKind,
    /// Установленный лимит.
    pub limit: u64,
    /// Текущий расход.
    pub used: u64,
}

impl LimitEvent {
    /// Процент расхода, 0..=100 (целочисленно, с округлением вниз).
    pub fn pct(&self) -> u8 {
        pct_of(self.used, self.limit)
    }

    /// Лимит полностью исчерпан?
    pub fn is_exceeded(&self) -> bool {
        self.used >= self.limit
    }
}

impl fmt::Display for LimitEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "⚠ {}: {}/{} ({}%)",
            self.kind.label(),
            self.used,
            self.limit,
            self.pct()
        )
    }
}

/// Целочисленный процент `used` от `limit`, 0..=100.
/// При нулевом лимите: 0, если ничего не израсходовано, иначе 100.
fn pct_of(used: u64, limit: u64) -> u8 {
    if limit == 0 {
        return if used == 0 { 0 } else { 100 };
    }
    // u128, чтобы не переполниться на больших квотах.
    ((used as u128 * 100) / limit as u128).min(100) as u8
}

// === Страж квот ===

/// Исполнитель квот сессии: хранит [`Quota`] и фактический расход
/// по каждому виду ресурса.
///
/// Все методы потребления атомарны: сначала проверка границы, затем
/// учёт. Отклонённая попытка не меняет состояние.
#[derive(Debug)]
pub struct LimitGuard {
    quota: Quota,
    used: HashMap<LimitKind, u64>,
}

impl LimitGuard {
    /// Страж с заданной квотой и нулевым расходом.
    pub fn new(quota: Quota) -> LimitGuard {
        LimitGuard {
            quota,
            used: HashMap::new(),
        }
    }

    /// Страж без лимитов (все `consume_*` всегда успешны).
    pub fn unlimited() -> LimitGuard {
        LimitGuard::new(Quota::unlimited())
    }

    /// Текущая квота.
    pub fn quota(&self) -> Quota {
        self.quota
    }

    /// Текущий расход по виду ресурса (0, если ещё не потреблялся).
    pub fn used(&self, kind: LimitKind) -> u64 {
        self.used.get(&kind).copied().unwrap_or(0)
    }

    /// Остаток по виду ресурса (`None` — ресурс не ограничен).
    pub fn remaining(&self, kind: LimitKind) -> Option<u64> {
        self.quota
            .limit_for(kind)
            .map(|limit| limit.saturating_sub(self.used(kind)))
    }

    /// Учесть один вызов API модели.
    pub fn consume_api_call(&mut self) -> Result<(), LimitExceeded> {
        self.consume(LimitKind::ApiCalls, 1)
    }

    /// Учесть `n` потреблённых токенов. `n == 0` всегда успешно.
    pub fn consume_tokens(&mut self, n: u64) -> Result<(), LimitExceeded> {
        self.consume(LimitKind::Tokens, n)
    }

    /// Учесть `n` секунд работы bash-команд. `n == 0` всегда успешно.
    pub fn consume_bash_secs(&mut self, n: u64) -> Result<(), LimitExceeded> {
        self.consume(LimitKind::BashSeconds, n)
    }

    /// Учесть один вызов инструмента.
    pub fn consume_tool_call(&mut self) -> Result<(), LimitExceeded> {
        self.consume(LimitKind::ToolCalls, 1)
    }

    /// Общее потребление: разрешено, пока `used + n <= limit`.
    /// При отказе состояние не меняется.
    fn consume(&mut self, kind: LimitKind, n: u64) -> Result<(), LimitExceeded> {
        let used = self.used(kind);
        if let Some(limit) = self.quota.limit_for(kind) {
            // Срабатываем ровно на границе: used == limit ещё допустимо,
            // шаг за неё — отказ. saturating_sub страхует от used > limit.
            if n > limit.saturating_sub(used) {
                return Err(LimitExceeded { kind, limit, used });
            }
        }
        let entry = self.used.entry(kind).or_insert(0);
        *entry = entry.saturating_add(n);
        Ok(())
    }

    /// Предупреждения по всем ограниченным ресурсам, израсходованным
    /// строго больше чем на [`WARN_THRESHOLD_PCT`] процентов.
    /// Порядок — стабильный ([`LimitKind::ALL`]); пустой вектор, если
    /// тревожных ресурсов нет.
    pub fn check(&self) -> Vec<LimitEvent> {
        LimitKind::ALL
            .into_iter()
            .filter_map(|kind| {
                let limit = self.quota.limit_for(kind)?;
                let used = self.used(kind);
                // Строго больше порога: ровно 80% — ещё не предупреждение.
                let over = used as u128 * 100 > limit as u128 * u128::from(WARN_THRESHOLD_PCT);
                over.then_some(LimitEvent { kind, limit, used })
            })
            .collect()
    }

    /// Прогресс по всем ограниченным ресурсам: кортежи
    /// `(вид, расход, лимит, процент 0..=100)` в стабильном порядке
    /// [`LimitKind::ALL`]. Ресурсы без лимита не включаются.
    pub fn progress(&self) -> Vec<(LimitKind, u64, u64, u8)> {
        LimitKind::ALL
            .into_iter()
            .filter_map(|kind| {
                let limit = self.quota.limit_for(kind)?;
                let used = self.used(kind);
                Some((kind, used, limit, pct_of(used, limit)))
            })
            .collect()
    }

    /// Обнулить расход по всем ресурсам (квота не меняется).
    pub fn reset(&mut self) {
        self.used.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Страж из конфигурационных пар — заодно упрощает сборку квот в тестах.
    fn guard_with(entries: &[(&str, u64)]) -> LimitGuard {
        LimitGuard::new(Quota::from_config(entries))
    }

    #[test]
    fn api_calls_boundary_fires_exactly_at_limit() {
        let mut g = guard_with(&[("api_calls", 3)]);
        // Ниже и ровно на границе — ok.
        g.consume_api_call().unwrap();
        g.consume_api_call().unwrap();
        g.consume_api_call().unwrap();
        assert_eq!(g.used(LimitKind::ApiCalls), 3);
        // Шаг за границу — отказ с корректными полями ошибки.
        let e = g.consume_api_call().unwrap_err();
        assert_eq!(
            e,
            LimitExceeded {
                kind: LimitKind::ApiCalls,
                limit: 3,
                used: 3,
            }
        );
    }

    #[test]
    fn tokens_boundary_with_batches() {
        let mut g = guard_with(&[("tokens", 100)]);
        g.consume_tokens(60).unwrap();
        g.consume_tokens(40).unwrap(); // ровно 100 — ещё ok
        assert_eq!(g.used(LimitKind::Tokens), 100);
        assert_eq!(g.remaining(LimitKind::Tokens), Some(0));
        // Даже единица сверх — отказ.
        let e = g.consume_tokens(1).unwrap_err();
        assert_eq!(e.kind, LimitKind::Tokens);
        assert_eq!(e.limit, 100);
        assert_eq!(e.used, 100);
    }

    #[test]
    fn bash_seconds_boundary_fires_exactly_at_limit() {
        let mut g = guard_with(&[("bash_seconds", 30)]);
        g.consume_bash_secs(29).unwrap();
        g.consume_bash_secs(1).unwrap(); // ровно 30
        assert!(g.consume_bash_secs(1).is_err());
        assert_eq!(g.used(LimitKind::BashSeconds), 30);
    }

    #[test]
    fn tool_calls_boundary_fires_exactly_at_limit() {
        let mut g = guard_with(&[("tool_calls", 1)]);
        g.consume_tool_call().unwrap();
        let e = g.consume_tool_call().unwrap_err();
        assert_eq!(e.kind, LimitKind::ToolCalls);
        assert_eq!(e.limit, 1);
        assert_eq!(e.used, 1);
    }

    #[test]
    fn below_limit_consumes_ok_and_accumulates() {
        let mut g = guard_with(&[("tokens", 1000), ("api_calls", 10)]);
        g.consume_tokens(100).unwrap();
        g.consume_tokens(200).unwrap();
        g.consume_api_call().unwrap();
        assert_eq!(g.used(LimitKind::Tokens), 300);
        assert_eq!(g.used(LimitKind::ApiCalls), 1);
        // Не заданные в квоте ресурсы — без лимита.
        assert_eq!(g.remaining(LimitKind::Tokens), Some(700));
        assert_eq!(g.remaining(LimitKind::BashSeconds), None);
        assert!(!g.quota().is_unlimited());
    }

    #[test]
    fn failed_attempt_is_not_counted() {
        let mut g = guard_with(&[("tokens", 10)]);
        g.consume_tokens(7).unwrap();
        // Попытка 7 + 5 = 12 > 10 отклоняется и не засчитывается.
        assert!(g.consume_tokens(5).is_err());
        assert_eq!(g.used(LimitKind::Tokens), 7);
        // После отказа оставшийся лимит по-прежнему доступен.
        g.consume_tokens(3).unwrap();
        assert_eq!(g.used(LimitKind::Tokens), 10);
    }

    #[test]
    fn warn_threshold_is_strict_above_80_pct() {
        let mut g = guard_with(&[("tokens", 100)]);
        g.consume_tokens(79).unwrap();
        assert!(g.check().is_empty(), "79% — ниже порога");
        g.consume_tokens(1).unwrap();
        assert!(g.check().is_empty(), "ровно 80% — ещё не warn");
        g.consume_tokens(1).unwrap();
        let events = g.check();
        assert_eq!(events.len(), 1, "81% — предупреждение");
        let ev = events[0];
        assert_eq!(ev.kind, LimitKind::Tokens);
        assert_eq!(ev.limit, 100);
        assert_eq!(ev.used, 81);
        assert_eq!(ev.pct(), 81);
        assert!(!ev.is_exceeded());
    }

    #[test]
    fn check_reports_only_kinds_over_threshold() {
        let mut g = guard_with(&[("api_calls", 10), ("tokens", 100)]);
        g.consume_api_call().unwrap(); // 10% — молчим
        g.consume_tokens(90).unwrap(); // 90% — warn
        let events = g.check();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, LimitKind::Tokens);
    }

    #[test]
    fn check_includes_fully_consumed_kind() {
        let mut g = guard_with(&[("api_calls", 3)]);
        for _ in 0..3 {
            g.consume_api_call().unwrap();
        }
        let events = g.check();
        assert_eq!(events.len(), 1, "100% > 80% — предупреждение остаётся");
        assert!(events[0].is_exceeded());
        assert_eq!(events[0].pct(), 100);
    }

    #[test]
    fn progress_reports_kind_used_limit_pct() {
        let mut g = guard_with(&[("api_calls", 4), ("tokens", 200)]);
        g.consume_api_call().unwrap();
        g.consume_tokens(50).unwrap();
        let rows = g.progress();
        // Стабильный порядок LimitKind::ALL, pct — целочисленный.
        assert_eq!(
            rows,
            vec![
                (LimitKind::ApiCalls, 1, 4, 25),
                (LimitKind::Tokens, 50, 200, 25),
            ]
        );
    }

    #[test]
    fn progress_skips_unlimited_resources() {
        let mut g = guard_with(&[("tokens", 100)]);
        g.consume_bash_secs(999).unwrap(); // безлимитный ресурс
        let rows = g.progress();
        assert_eq!(rows.len(), 1, "безлимитные ресурсы не показываем");
        assert_eq!(rows[0].0, LimitKind::Tokens);
        assert_eq!(rows[0].3, 0);
    }

    #[test]
    fn reset_clears_all_usage() {
        let mut g = guard_with(&[("api_calls", 2), ("tokens", 100)]);
        g.consume_api_call().unwrap();
        g.consume_tokens(90).unwrap();
        assert_eq!(g.check().len(), 1);
        g.reset();
        for kind in LimitKind::ALL {
            assert_eq!(g.used(kind), 0);
        }
        assert!(g.check().is_empty());
        // Квота не изменилась: лимит снова доступен целиком.
        g.consume_api_call().unwrap();
        g.consume_api_call().unwrap();
        assert!(g.consume_api_call().is_err());
    }

    #[test]
    fn unlimited_quota_never_blocks() {
        let mut g = LimitGuard::unlimited();
        assert!(g.quota().is_unlimited());
        for _ in 0..1000 {
            g.consume_api_call().unwrap();
        }
        g.consume_tokens(1_000_000).unwrap();
        g.consume_bash_secs(86_400).unwrap();
        g.consume_tool_call().unwrap();
        assert_eq!(g.used(LimitKind::ApiCalls), 1000);
        assert_eq!(g.used(LimitKind::Tokens), 1_000_000);
        assert_eq!(g.remaining(LimitKind::Tokens), None);
        assert!(g.check().is_empty(), "без лимита нет и предупреждений");
        assert!(g.progress().is_empty());
    }

    #[test]
    fn from_config_parses_aliases_and_ignores_unknown() {
        let q = Quota::from_config(&[
            ("api", 5),
            (" TOKENS ", 1000),
            ("Bash", 60),
            ("tools", 7),
            ("unknown_resource", 42),
        ]);
        assert_eq!(q.api_calls, Some(5));
        assert_eq!(q.tokens, Some(1000));
        assert_eq!(q.bash_seconds, Some(60));
        assert_eq!(q.tool_calls, Some(7));
    }

    #[test]
    fn from_config_last_duplicate_wins() {
        let q = Quota::from_config(&[("tokens", 10), ("token", 20)]);
        assert_eq!(q.tokens, Some(20));
        // Пустой конфиг — квота без лимитов.
        assert!(Quota::from_config(&[]).is_unlimited());
    }

    #[test]
    fn zero_limit_blocks_any_consumption() {
        let mut g = guard_with(&[("tokens", 0)]);
        // Нулевое потребление допустимо даже при Some(0).
        g.consume_tokens(0).unwrap();
        let e = g.consume_tokens(1).unwrap_err();
        assert_eq!(
            e,
            LimitExceeded {
                kind: LimitKind::Tokens,
                limit: 0,
                used: 0,
            }
        );
        // Прогресс с нулевым лимитом не падает и даёт 0%.
        let rows = g.progress();
        assert_eq!(rows, vec![(LimitKind::Tokens, 0, 0, 0)]);
    }

    #[test]
    fn display_and_std_error_impls() {
        let e = LimitExceeded {
            kind: LimitKind::Tokens,
            limit: 10,
            used: 10,
        };
        let msg = e.to_string();
        assert!(msg.contains("tokens"), "имя вида в сообщении: {msg}");
        assert!(msg.contains("10"), "числа в сообщении: {msg}");
        // LimitExceeded — полноценная std::error::Error.
        fn assert_is_error<T: std::error::Error>(_: &T) {}
        assert_is_error(&e);

        let ev = LimitEvent {
            kind: LimitKind::ApiCalls,
            limit: 4,
            used: 4,
        };
        let text = ev.to_string();
        assert!(text.contains("API-вызовы"), "метка в событии: {text}");
        assert!(text.contains("100%"), "процент в событии: {text}");
    }

    #[test]
    fn serde_roundtrip_quota_and_event() {
        let q = Quota {
            api_calls: Some(5),
            tokens: None,
            bash_seconds: Some(30),
            tool_calls: None,
        };
        let json = serde_json::to_string(&q).unwrap();
        assert!(!json.contains("tokens"), "None-поля не сериализуются");
        let back: Quota = serde_json::from_str(&json).unwrap();
        assert_eq!(q, back);

        let kind = LimitKind::BashSeconds;
        assert_eq!(serde_json::to_string(&kind).unwrap(), "\"bash_seconds\"");
        let parsed: LimitKind = serde_json::from_str("\"bash_seconds\"").unwrap();
        assert_eq!(parsed, kind);
    }
}
