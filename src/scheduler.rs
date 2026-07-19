//! Очередь промптов агента с приоритетами — «mailbox» по образцу Codex.
//!
//! Агентный цикл забирает пользовательские и системные промпты через
//! [`PromptQueue::pop_next`], причём промпт с приоритетом [`Priority::Immediate`]
//! всегда уходит раньше остальных и (по умолчанию) помечается флагом `preempt` —
//! сигналом прервать текущий ход. Внутри одного приоритета — строгий FIFO.
//!
//! Дополнительные правила:
//! - подряд идущие `Normal`-промпты от одного источника склеиваются через `\n`
//!   ([`PromptQueue::merge_adjacent`]) — coalescing быстрых реплик подряд;
//! - элементу можно задать TTL ([`QueuedPrompt::with_ttl`]): просроченные
//!   промпты выбрасываются при pop/drain, время берётся из инжектируемых
//!   часов [`Clock`] (в тестах — ручные часы с явным сдвигом);
//! - [`PromptQueue::stats`] ведёт по каждому приоритету счётчики
//!   pending/enqueued/dequeued/expired/merged с инвариантом
//!   «всё поставленное куда-то делось» ([`LaneStats::accounted`]).

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Приоритет промпта в очереди.
///
/// Порядок перечисления вариантов совпадает с порядком обслуживания:
/// `Immediate` обслуживается первым, `Background` — последним.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Priority {
    /// Немедленная обработка: обгоняет остальные классы и (по умолчанию)
    /// помечается флагом `preempt` — агент должен прервать текущий ход.
    Immediate,
    /// Обычная реплика пользователя или системы.
    Normal,
    /// Фоновая работа: обрабатывается, когда более срочных промптов нет.
    Background,
}

impl Priority {
    /// Все приоритеты в порядке обслуживания (от срочного к фоновому).
    pub const ALL: [Priority; 3] =
        [Priority::Immediate, Priority::Normal, Priority::Background];

    /// Индекс внутренней полосы (lane) очереди.
    const fn lane_index(self) -> usize {
        match self {
            Priority::Immediate => 0,
            Priority::Normal => 1,
            Priority::Background => 2,
        }
    }

    /// Короткое ASCII-имя для логов и статистики.
    pub fn label(self) -> &'static str {
        match self {
            Priority::Immediate => "immediate",
            Priority::Normal => "normal",
            Priority::Background => "background",
        }
    }
}

/// Источник промпта — кто поставил его в очередь.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PromptSource {
    /// Реплика пользователя (ввод в TUI / CLI).
    User,
    /// Системный хук (например, post-tool заметка).
    Hook,
    /// Отложенная задача планировщика (cron).
    Cron,
}

/// Промпт, стоящий в очереди агента.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedPrompt {
    /// Текст промпта (после склейки — несколько реплик через `\n`).
    pub text: String,
    /// Приоритет обслуживания.
    pub priority: Priority,
    /// Момент постановки в миллисекундах по часам очереди.
    ///
    /// Проставляется [`PromptQueue::push`] в момент постановки; значение,
    /// заданное при ручном конструировании, перезаписывается.
    pub enqueued_at: u64,
    /// Источник промпта.
    pub source: PromptSource,
    /// Время жизни: промпт считается просроченным, когда часы очереди
    /// достигают `enqueued_at + ttl` (граница включительно).
    pub ttl: Option<Duration>,
}

impl QueuedPrompt {
    /// Новый промпт без TTL (`enqueued_at` выставит очередь при push).
    pub fn new(text: impl Into<String>, priority: Priority, source: PromptSource) -> Self {
        Self {
            text: text.into(),
            priority,
            enqueued_at: 0,
            source,
            ttl: None,
        }
    }

    /// Builder: задать время жизни (TTL) промпта.
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// Абсолютный дедлайн в миллисекундах часов очереди (насыщающая арифметика).
    pub fn deadline(&self) -> Option<u64> {
        self.ttl.map(|ttl| {
            let ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
            self.enqueued_at.saturating_add(ms)
        })
    }

    /// Просрочен ли промпт в момент `now_ms` (жив, пока `now_ms < deadline`).
    pub fn is_expired_at(&self, now_ms: u64) -> bool {
        self.deadline().is_some_and(|deadline| now_ms >= deadline)
    }
}

/// Источник времени для очереди (миллисекунды, монотонная шкала).
///
/// Инжектируется через [`PromptQueue::with_clock`]: в бою — [`SystemClock`],
/// в тестах — ручные часы с явным сдвигом времени.
pub trait Clock {
    /// Текущее время в миллисекундах.
    fn now_ms(&self) -> u64;
}

/// Системные часы: миллисекунды от UNIX-эпохи.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// Правила переупорядочивания очереди.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReorderRules {
    /// Если включено, извлечённый `Immediate`-промпт возвращается с
    /// `preempt = true` — сигнал агенту прервать текущий ход и обработать
    /// срочный промпт немедленно (модель «steering» из Codex).
    pub preempt_on_immediate: bool,
}

impl Default for ReorderRules {
    fn default() -> Self {
        Self { preempt_on_immediate: true }
    }
}

/// Результат извлечения промпта из очереди.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DequeuedPrompt {
    /// Сам промпт.
    pub prompt: QueuedPrompt,
    /// `true`, если промпт вытесняет текущую обработку агента.
    pub preempt: bool,
}

/// Статистика одного приоритетного класса.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LaneStats {
    /// Сколько промптов сейчас ждут обработки.
    pub pending: usize,
    /// Сколько всего поставлено в очередь.
    pub enqueued: u64,
    /// Сколько извлечено на обработку (включая [`PromptQueue::drain`]).
    pub dequeued: u64,
    /// Сколько выброшено по истечении TTL.
    pub expired: u64,
    /// Сколько промптов поглощено склейкой [`PromptQueue::merge_adjacent`].
    pub merged: u64,
}

impl LaneStats {
    /// Инвариант класса: всё поставленное куда-то делось
    /// (`enqueued = pending + dequeued + expired + merged`).
    pub fn accounted(&self) -> bool {
        self.enqueued
            == self.pending as u64 + self.dequeued + self.expired + self.merged
    }
}

/// Снимок статистики очереди по всем приоритетам.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueStats {
    /// Класс `Immediate`.
    pub immediate: LaneStats,
    /// Класс `Normal`.
    pub normal: LaneStats,
    /// Класс `Background`.
    pub background: LaneStats,
}

impl QueueStats {
    /// Суммарное число ожидающих промптов во всех классах.
    pub fn pending_total(&self) -> usize {
        self.immediate.pending + self.normal.pending + self.background.pending
    }
}

/// Внутренние накопительные счётчики полосы (pending считается по длине).
#[derive(Debug, Default, Clone, Copy)]
struct LaneCounters {
    enqueued: u64,
    dequeued: u64,
    expired: u64,
    merged: u64,
}

/// Очередь промптов с приоритетами, TTL и склейкой соседних.
///
/// Часы инжектируются типом-параметром `C`; по умолчанию — [`SystemClock`].
/// Потокобезопасность не встроена: очередь живёт внутри одного агентного
/// цикла (как mailbox в Codex), внешняя синхронизация — на вызывающем.
pub struct PromptQueue<C: Clock = SystemClock> {
    clock: C,
    rules: ReorderRules,
    lanes: [VecDeque<QueuedPrompt>; 3],
    counters: [LaneCounters; 3],
}

impl PromptQueue<SystemClock> {
    /// Пустая очередь на системных часах с правилами по умолчанию.
    pub fn new() -> Self {
        Self::with_clock(SystemClock)
    }
}

impl Default for PromptQueue<SystemClock> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Clock> PromptQueue<C> {
    /// Пустая очередь на заданных часах.
    pub fn with_clock(clock: C) -> Self {
        Self {
            clock,
            rules: ReorderRules::default(),
            lanes: [VecDeque::new(), VecDeque::new(), VecDeque::new()],
            counters: [LaneCounters::default(); 3],
        }
    }

    /// Текущие правила переупорядочивания.
    pub fn reorder_rules(&self) -> ReorderRules {
        self.rules
    }

    /// Заменить правила переупорядочивания.
    pub fn set_reorder_rules(&mut self, rules: ReorderRules) {
        self.rules = rules;
    }

    /// Мутабельный доступ к часам (например, сдвинуть ручные часы в тесте).
    pub fn clock_mut(&mut self) -> &mut C {
        &mut self.clock
    }

    /// Поставить промпт в очередь; `enqueued_at` выставляется по часам очереди.
    pub fn push(&mut self, mut prompt: QueuedPrompt) {
        prompt.enqueued_at = self.clock.now_ms();
        let idx = prompt.priority.lane_index();
        self.counters[idx].enqueued += 1;
        self.lanes[idx].push_back(prompt);
    }

    /// Извлечь следующий промпт: `Immediate` всегда раньше, внутри класса — FIFO.
    ///
    /// Просроченные (TTL) элементы на вершине полосы перед извлечением
    /// выбрасываются. Если извлечён `Immediate` и правило
    /// `preempt_on_immediate` включено, результат помечается `preempt = true`.
    pub fn pop_next(&mut self) -> Option<DequeuedPrompt> {
        let now = self.clock.now_ms();
        for priority in Priority::ALL {
            let idx = priority.lane_index();
            // протухшие головы полосы выбрасываем до извлечения живого элемента
            while self.lanes[idx].front().is_some_and(|p| p.is_expired_at(now)) {
                self.lanes[idx].pop_front();
                self.counters[idx].expired += 1;
            }
            if let Some(prompt) = self.lanes[idx].pop_front() {
                self.counters[idx].dequeued += 1;
                let preempt =
                    self.rules.preempt_on_immediate && priority == Priority::Immediate;
                return Some(DequeuedPrompt { prompt, preempt });
            }
        }
        None
    }

    /// Посмотреть следующий промпт без извлечения (просроченные пропускаются,
    /// но не выбрасываются — это сделает pop/purge).
    pub fn peek(&self) -> Option<&QueuedPrompt> {
        let now = self.clock.now_ms();
        Priority::ALL.iter().find_map(|priority| {
            self.lanes[priority.lane_index()]
                .iter()
                .find(|prompt| !prompt.is_expired_at(now))
        })
    }

    /// Число ожидающих промптов (все классы; просроченные считаются,
    /// пока их не выбросили pop/purge/drain).
    pub fn len(&self) -> usize {
        self.lanes.iter().map(VecDeque::len).sum()
    }

    /// Пуста ли очередь.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Снять всю очередь в порядке обслуживания (`Immediate` → `Background`).
    ///
    /// Просроченные на момент вызова элементы выбрасываются (учитываются в
    /// `expired`), остальные возвращаются и учитываются в `dequeued`.
    pub fn drain(&mut self) -> Vec<QueuedPrompt> {
        let now = self.clock.now_ms();
        let mut out = Vec::with_capacity(self.len());
        for priority in Priority::ALL {
            let idx = priority.lane_index();
            while let Some(prompt) = self.lanes[idx].pop_front() {
                if prompt.is_expired_at(now) {
                    self.counters[idx].expired += 1;
                } else {
                    self.counters[idx].dequeued += 1;
                    out.push(prompt);
                }
            }
        }
        out
    }

    /// Выбросить все просроченные промпты из всех классов; вернуть их число.
    pub fn purge_expired(&mut self) -> usize {
        let now = self.clock.now_ms();
        let mut total = 0usize;
        for priority in Priority::ALL {
            let idx = priority.lane_index();
            let before = self.lanes[idx].len();
            self.lanes[idx].retain(|prompt| !prompt.is_expired_at(now));
            let dropped = before - self.lanes[idx].len();
            self.counters[idx].expired += dropped as u64;
            total += dropped;
        }
        total
    }

    /// Склеить подряд идущие `Normal`-промпты от одного источника.
    ///
    /// Тексты объединяются через `\n` в первый промпт серии — его место в
    /// очереди и `enqueued_at` сохраняются. TTL объединённого: минимальный
    /// из заданных; если хотя бы у одного промпта серии TTL нет, итоговый
    /// тоже остаётся без TTL (чужой текст не должен протухнуть раньше
    /// времени). Возвращает число поглощённых промптов. Классы `Immediate`
    /// и `Background` не склеиваются.
    pub fn merge_adjacent(&mut self) -> usize {
        let idx = Priority::Normal.lane_index();
        let lane = &mut self.lanes[idx];
        if lane.len() < 2 {
            return 0;
        }
        let items = std::mem::take(lane);
        let mut merged: VecDeque<QueuedPrompt> = VecDeque::with_capacity(items.len());
        let mut absorbed = 0usize;
        for prompt in items {
            // Шаг 1: отдельная проверка — иначе заём back_mut() в ветке
            // склейки пересёкся бы с push_back() в ветке else (borrowck).
            let glue =
                matches!(merged.back(), Some(last) if last.source == prompt.source);
            if !glue {
                merged.push_back(prompt);
                continue;
            }
            // Шаг 2: back_mut() здесь гарантирован — только что видели back().
            if let Some(last) = merged.back_mut() {
                last.text.push('\n');
                last.text.push_str(&prompt.text);
                last.ttl = match (last.ttl, prompt.ttl) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    _ => None,
                };
                absorbed += 1;
            }
        }
        *lane = merged;
        self.counters[idx].merged += absorbed as u64;
        absorbed
    }

    /// Статистика одного приоритетного класса.
    pub fn stats_for(&self, priority: Priority) -> LaneStats {
        let idx = priority.lane_index();
        let counters = self.counters[idx];
        LaneStats {
            pending: self.lanes[idx].len(),
            enqueued: counters.enqueued,
            dequeued: counters.dequeued,
            expired: counters.expired,
            merged: counters.merged,
        }
    }

    /// Статистика по всем приоритетам.
    pub fn stats(&self) -> QueueStats {
        QueueStats {
            immediate: self.stats_for(Priority::Immediate),
            normal: self.stats_for(Priority::Normal),
            background: self.stats_for(Priority::Background),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ручные часы: время двигается только явным `advance`.
    #[derive(Debug)]
    struct ManualClock {
        now: u64,
    }

    impl ManualClock {
        fn advance(&mut self, ms: u64) {
            self.now += ms;
        }
    }

    impl Clock for ManualClock {
        fn now_ms(&self) -> u64 {
            self.now
        }
    }

    /// Очередь на ручных часах, стартующих с нуля.
    fn manual_queue() -> PromptQueue<ManualClock> {
        PromptQueue::with_clock(ManualClock { now: 0 })
    }

    /// Промпт Normal/User с TTL в миллисекундах.
    fn timed(text: &str, ttl_ms: u64) -> QueuedPrompt {
        QueuedPrompt::new(text, Priority::Normal, PromptSource::User)
            .with_ttl(Duration::from_millis(ttl_ms))
    }

    /// Промпт заданного класса/источника с TTL в миллисекундах.
    fn timed_as(text: &str, priority: Priority, source: PromptSource, ttl_ms: u64) -> QueuedPrompt {
        QueuedPrompt::new(text, priority, source).with_ttl(Duration::from_millis(ttl_ms))
    }

    /// Тексты из вектора промптов — для компактных сравнений порядка.
    fn texts(items: Vec<QueuedPrompt>) -> Vec<String> {
        items.into_iter().map(|p| p.text).collect()
    }

    #[test]
    fn empty_queue_behaves() {
        let mut q = manual_queue();
        assert!(q.is_empty());
        assert!(q.pop_next().is_none());
        assert!(q.peek().is_none());
        assert!(q.drain().is_empty());
        assert_eq!(q.merge_adjacent(), 0);
        assert_eq!(q.purge_expired(), 0);
        assert_eq!(q.stats().pending_total(), 0);
    }

    #[test]
    fn system_clock_queue_smoke() {
        let mut q = PromptQueue::new();
        assert!(PromptQueue::default().is_empty());
        q.push(QueuedPrompt::new("x", Priority::Normal, PromptSource::User));
        assert_eq!(q.len(), 1);
        // метка времени проставлена системными часами (ненулевая)
        let stamped = q.peek().map(|p| p.enqueued_at).unwrap_or(0);
        assert!(stamped > 0);
        assert!(q.pop_next().is_some());
    }

    #[test]
    fn push_stamps_enqueued_at_from_queue_clock() {
        let mut q = manual_queue();
        q.clock_mut().advance(1234);
        q.push(QueuedPrompt::new("привет", Priority::Normal, PromptSource::User));
        assert_eq!(q.peek().map(|p| p.enqueued_at), Some(1234));
    }

    #[test]
    fn immediate_goes_first_regardless_of_arrival() {
        let mut q = manual_queue();
        q.push(QueuedPrompt::new("фон", Priority::Background, PromptSource::Cron));
        q.push(QueuedPrompt::new("обычный", Priority::Normal, PromptSource::User));
        q.push(QueuedPrompt::new("срочно", Priority::Immediate, PromptSource::Hook));
        let order: Vec<String> =
            std::iter::from_fn(|| q.pop_next()).map(|d| d.prompt.text).collect();
        assert_eq!(order, ["срочно", "обычный", "фон"]);
    }

    #[test]
    fn fifo_within_same_priority() {
        let mut q = manual_queue();
        for i in 1..=3 {
            q.push(QueuedPrompt::new(format!("n{i}"), Priority::Normal, PromptSource::User));
        }
        for i in 1..=3 {
            let d = q.pop_next().unwrap();
            assert_eq!(d.prompt.text, format!("n{i}"));
            assert!(!d.preempt); // Normal не вытесняет
        }
        assert!(q.pop_next().is_none());
    }

    #[test]
    fn late_immediate_overtakes_earlier_normal() {
        let mut q = manual_queue();
        q.push(QueuedPrompt::new("ранний", Priority::Normal, PromptSource::User));
        q.clock_mut().advance(10);
        q.push(QueuedPrompt::new("поздний-срочный", Priority::Immediate, PromptSource::User));
        let d = q.pop_next().unwrap();
        assert_eq!(d.prompt.text, "поздний-срочный");
        assert!(d.preempt);
    }

    #[test]
    fn preempt_flag_follows_reorder_rules() {
        let mut q = manual_queue();
        // правило по умолчанию: Immediate вытесняет обработку
        assert!(q.reorder_rules().preempt_on_immediate);
        q.push(QueuedPrompt::new("фон", Priority::Background, PromptSource::Cron));
        q.push(QueuedPrompt::new("срочно", Priority::Immediate, PromptSource::User));
        let d = q.pop_next().unwrap();
        assert_eq!(d.prompt.text, "срочно");
        assert!(d.preempt);
        assert!(!q.pop_next().unwrap().preempt); // Background — без вытеснения
        // выключаем правило: Immediate всё ещё первый, но уже без флага
        q.set_reorder_rules(ReorderRules { preempt_on_immediate: false });
        q.push(QueuedPrompt::new("срочно-2", Priority::Immediate, PromptSource::Hook));
        let d = q.pop_next().unwrap();
        assert_eq!(d.prompt.text, "срочно-2");
        assert!(!d.preempt);
    }

    #[test]
    fn merge_adjacent_glues_consecutive_same_source() {
        let mut q = manual_queue();
        q.push(QueuedPrompt::new("a", Priority::Normal, PromptSource::User));
        q.clock_mut().advance(5);
        q.push(QueuedPrompt::new("b", Priority::Normal, PromptSource::User));
        q.push(QueuedPrompt::new("c", Priority::Normal, PromptSource::Hook));
        assert_eq!(q.merge_adjacent(), 1);
        assert_eq!(q.len(), 2);
        let items = q.drain();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "a\nb");
        assert_eq!(items[0].source, PromptSource::User);
        // место и метка времени — от первого промпта серии
        assert_eq!(items[0].enqueued_at, 0);
        assert_eq!(items[1].text, "c");
        assert_eq!(items[1].source, PromptSource::Hook);
    }

    #[test]
    fn merge_adjacent_keeps_different_sources_separate() {
        let mut q = manual_queue();
        q.push(QueuedPrompt::new("u", Priority::Normal, PromptSource::User));
        q.push(QueuedPrompt::new("h", Priority::Normal, PromptSource::Hook));
        q.push(QueuedPrompt::new("c", Priority::Normal, PromptSource::Cron));
        assert_eq!(q.merge_adjacent(), 0);
        assert_eq!(texts(q.drain()), ["u", "h", "c"]);
    }

    #[test]
    fn merge_adjacent_ignores_immediate_and_background() {
        let mut q = manual_queue();
        q.push(QueuedPrompt::new("i1", Priority::Immediate, PromptSource::User));
        q.push(QueuedPrompt::new("i2", Priority::Immediate, PromptSource::User));
        q.push(QueuedPrompt::new("b1", Priority::Background, PromptSource::Cron));
        q.push(QueuedPrompt::new("b2", Priority::Background, PromptSource::Cron));
        assert_eq!(q.merge_adjacent(), 0);
        assert_eq!(q.len(), 4);
    }

    #[test]
    fn merge_adjacent_ttl_none_wins_otherwise_min() {
        let mut q = manual_queue();
        q.push(timed("u1", 100));
        q.push(QueuedPrompt::new("u2", Priority::Normal, PromptSource::User));
        q.push(timed_as("h1", Priority::Normal, PromptSource::Hook, 300));
        q.push(timed_as("h2", Priority::Normal, PromptSource::Hook, 50));
        assert_eq!(q.merge_adjacent(), 2);
        let items = q.drain();
        assert_eq!(items.len(), 2);
        // серия с бессрочным участником осталась бессрочной
        assert_eq!(items[0].ttl, None);
        // серия из двух TTL-промптов получила минимальный TTL
        assert_eq!(items[1].ttl, Some(Duration::from_millis(50)));
    }

    #[test]
    fn merge_adjacent_single_item_is_noop() {
        let mut q = manual_queue();
        q.push(QueuedPrompt::new("один", Priority::Normal, PromptSource::User));
        assert_eq!(q.merge_adjacent(), 0);
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn expired_prompt_dropped_on_pop() {
        let mut q = manual_queue();
        q.push(timed("протухший", 50));
        q.push(timed("свежий", 10_000));
        q.clock_mut().advance(100);
        let d = q.pop_next().unwrap();
        assert_eq!(d.prompt.text, "свежий");
        assert!(q.pop_next().is_none());
        let s = q.stats_for(Priority::Normal);
        assert_eq!(s.expired, 1);
        assert_eq!(s.dequeued, 1);
    }

    #[test]
    fn ttl_boundary_alive_before_deadline_expired_at_deadline() {
        let mut q = manual_queue();
        q.push(timed("живой", 100)); // дедлайн: 0 + 100 = 100
        q.clock_mut().advance(99); // t=99 < 100 — ещё жив
        assert!(q.pop_next().is_some());
        q.push(timed("граница", 100)); // поставлен при t=99, дедлайн 199
        q.clock_mut().advance(100); // t=199 == дедлайн — просрочен (граница включительно)
        assert!(q.pop_next().is_none());
        let s = q.stats_for(Priority::Normal);
        assert_eq!(s.dequeued, 1);
        assert_eq!(s.expired, 1);
    }

    #[test]
    fn prompt_without_ttl_never_expires() {
        let mut q = manual_queue();
        q.push(QueuedPrompt::new("вечный", Priority::Normal, PromptSource::User));
        q.clock_mut().advance(u64::MAX / 2);
        assert!(q.pop_next().is_some());
    }

    #[test]
    fn drain_returns_everything_in_service_order() {
        let mut q = manual_queue();
        q.push(QueuedPrompt::new("n1", Priority::Normal, PromptSource::User));
        q.push(QueuedPrompt::new("b1", Priority::Background, PromptSource::Cron));
        q.push(QueuedPrompt::new("i1", Priority::Immediate, PromptSource::Hook));
        q.push(QueuedPrompt::new("n2", Priority::Normal, PromptSource::User));
        assert_eq!(texts(q.drain()), ["i1", "n1", "n2", "b1"]);
        assert!(q.is_empty());
        let s = q.stats();
        assert_eq!(s.pending_total(), 0);
        assert_eq!(s.immediate.dequeued + s.normal.dequeued + s.background.dequeued, 4);
    }

    #[test]
    fn drain_discards_expired() {
        let mut q = manual_queue();
        q.push(timed("протух", 10));
        q.push(QueuedPrompt::new("живой", Priority::Normal, PromptSource::User));
        q.clock_mut().advance(20);
        assert_eq!(texts(q.drain()), ["живой"]);
        assert_eq!(q.stats_for(Priority::Normal).expired, 1);
    }

    #[test]
    fn peek_skips_expired_without_removing() {
        let mut q = manual_queue();
        q.push(timed("протух", 10));
        q.push(timed("свежий", 1000));
        q.clock_mut().advance(20);
        assert_eq!(q.peek().map(|p| p.text.as_str()), Some("свежий"));
        // peek ничего не извлёк и не выбросил
        assert_eq!(q.len(), 2);
        // а pop выбросил протухшее и отдал свежее
        assert_eq!(q.pop_next().unwrap().prompt.text, "свежий");
        assert!(q.is_empty());
    }

    #[test]
    fn purge_expired_cleans_all_lanes() {
        let mut q = manual_queue();
        q.push(timed("n", 10));
        q.push(timed_as("i", Priority::Immediate, PromptSource::User, 10));
        q.push(timed_as("b", Priority::Background, PromptSource::Cron, 10));
        q.push(timed("n-живой", 10_000));
        q.clock_mut().advance(50);
        assert_eq!(q.purge_expired(), 3);
        assert_eq!(q.len(), 1);
        let s = q.stats();
        assert_eq!(s.immediate.expired + s.normal.expired + s.background.expired, 3);
    }

    #[test]
    fn stats_invariant_holds_per_lane() {
        let mut q = manual_queue();
        q.push(QueuedPrompt::new("m1", Priority::Normal, PromptSource::User));
        q.push(QueuedPrompt::new("m2", Priority::Normal, PromptSource::User));
        // источник Cron — не склеится с User-серией выше
        q.push(timed_as("ttl", Priority::Normal, PromptSource::Cron, 5));
        q.push(QueuedPrompt::new("i1", Priority::Immediate, PromptSource::Hook));
        assert_eq!(q.merge_adjacent(), 1);
        q.clock_mut().advance(10);
        // Immediate извлекается первым
        assert_eq!(q.pop_next().unwrap().prompt.text, "i1");
        // затем склеенный; протухший при этом выбрасывается
        assert_eq!(q.pop_next().unwrap().prompt.text, "m1\nm2");
        assert!(q.pop_next().is_none());
        let n = q.stats_for(Priority::Normal);
        assert_eq!(n.enqueued, 3);
        assert_eq!(n.merged, 1);
        assert_eq!(n.expired, 1);
        assert_eq!(n.dequeued, 1);
        assert_eq!(n.pending, 0);
        assert!(n.accounted());
        let i = q.stats_for(Priority::Immediate);
        assert_eq!((i.enqueued, i.dequeued, i.pending), (1, 1, 0));
        assert!(i.accounted());
        assert!(q.stats().background.accounted());
    }

    #[test]
    fn serde_roundtrip_queued_prompt() {
        let p = QueuedPrompt::new("текст", Priority::Immediate, PromptSource::Hook)
            .with_ttl(Duration::from_millis(250));
        let json = serde_json::to_string(&p).unwrap();
        let back: QueuedPrompt = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn priority_labels_match_service_order() {
        assert_eq!(
            Priority::ALL.map(Priority::label),
            ["immediate", "normal", "background"]
        );
    }
}
