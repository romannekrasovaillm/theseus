//! Кэш префиксов промптов (образец: практики prompt caching у OpenAI/Anthropic).
//!
//! ## Зачем
//!
//! И OpenAI (automatic prompt caching), и Anthropic (`cache_control` breakpoints)
//! не пересчитывают ведущий префикс запроса, если он байт-в-байт совпал с уже
//! виденным: префикс читается из кэша провайдера дешевле и с меньшей латентностью.
//! Чтобы попадать в этот кэш осознанно, агенту нужно знать, какие префиксы он
//! уже отправлял, — этим и занимается данный модуль:
//!
//! - [`build_key`] складывает стабильные секции промпта (модель, системный
//!   промпт, JSON инструментов) в компактный [`CacheKey`] через FNV-1a
//!   ([`fnv1a64`]): смена любого байта секции даёт другой ключ — как и у
//!   провайдера, где кэш привязан к точному байтовому представлению;
//! - [`longest_stable_prefix`] оценивает длину стабильного префикса в байтах:
//!   системный промпт и инструменты стабильны всегда, user-сообщения — никогда;
//! - [`PrefixCache`] хранит записи [`CacheEntry`] (текст префикса + оценка его
//!   стоимости в токенах) как LRU-кэш фиксированной ёмкости и ведёт статистику:
//!   [`PrefixCache::hit_rate`], [`PrefixCache::savings_estimate`],
//!   [`PrefixCache::invalidate_model`].
//!
//! ## Устройство LRU
//!
//! Записи лежат в `HashMap<CacheKey, CacheEntry>`, а порядок обращений — в
//! `BTreeMap<u64, CacheKey>`, где ключ — номер последнего обращения от
//! монотонного счётчика. И `get`, и `put` перевыпускают номер, поэтому
//! вытесняется запись с минимальным номером — действительно least recently
//! used. Все операции O(log n).
//!
//! ## Границы семантики (зафиксированы тестами)
//!
//! - `capacity == 0` — выключенный кэш: `put` холостая, `get` всегда промах;
//! - `put` по существующему ключу обновляет текст и оценку токенов и освежает
//!   порядок — обновлённая запись переживает вытеснение;
//! - статистика монотонна: вытеснение и инвалидация её не сбрасывают;
//! - экономия накапливается только на попаданиях: каждое добавляет
//!   `hit_tokens_est` найденной записи (сложение насыщающее).
//!
//! Модуль чистый: только `std`, без сети, часов и случайности — детерминирован.

use std::collections::{BTreeMap, HashMap};

/// Разделитель секций промпта, который эвристика [`longest_stable_prefix`]
/// учитывает при подсчёте байтов стабильного префикса.
pub const SECTION_SEPARATOR: &str = "\n\n";

// === FNV-1a ===

/// Базис смещения FNV-1a (64 бита) — значение хеша пустого входа.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// Простое число Фаулера-Нолла-Во (64 бита).
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Хеш FNV-1a (64 бита) последовательности байт.
///
/// Детерминированный некриптографический хеш: считается за один проход и даёт
/// хорошее рассеивание на коротких строках, чего достаточно для ключей кэша.
/// Криптографической стойкости не требуется: хеш здесь — индекс, а не защита.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// === Ключ кэша ===

/// Ключ записи кэша: модель плюс хеши стабильных секций промпта.
///
/// Сами секции в ключе не хранятся — только их хеши FNV-1a: ключ компактен,
/// а смена любого байта системного промпта или описания инструментов даёт
/// другой ключ и, значит, «промах» вместо ошибочного попадания в чужой
/// префикс.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// Имя модели (кэш различает модели: префикс одной бесполезен другой).
    pub model: String,
    /// Хеш FNV-1a системного промпта.
    pub system_hash: u64,
    /// Хеш FNV-1a JSON-описания инструментов.
    pub tools_hash: u64,
}

/// Строит [`CacheKey`] из сырых секций промпта.
///
/// - `model` — имя модели, хранится в ключе как есть;
/// - `system` — системный промпт целиком;
/// - `tools_json` — канонический JSON-дескриптор инструментов.
///
/// Функция детерминирована: одинаковые входы дают равные ключи. Заметьте, что
/// ключ чувствителен к точному байтовому представлению: перестановка полей
/// JSON или лишний пробел в системном промпте дают другой хеш — осмысленно,
/// ведь провайдер кэширует префикс тоже побайтово.
pub fn build_key(model: &str, system: &str, tools_json: &str) -> CacheKey {
    CacheKey {
        model: model.to_string(),
        system_hash: fnv1a64(system.as_bytes()),
        tools_hash: fnv1a64(tools_json.as_bytes()),
    }
}

// === Запись кэша ===

/// Запись кэша: закэшированный префикс и оценка его стоимости в токенах.
///
/// Возвращается из [`PrefixCache::get`] по значению (копия): мутация копии не
/// затрагивает кэш.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    /// Текст префикса, который провайдер уже видел с этим ключом.
    pub prefix_text: String,
    /// Оценка числа токенов префикса: столько токенов экономит одно попадание
    /// в кэш (префикс не пересчитывается провайдером заново).
    pub hit_tokens_est: u64,
    /// Порядковый номер последнего обращения (служебная метка LRU).
    last_used: u64,
}

impl CacheEntry {
    /// Порядковый номер последнего обращения к записи: чем меньше номер, тем
    /// «старше» запись в смысле LRU. Номера выдаёт внутренний счётчик кэша,
    /// они монотонно растут в порядке обращений.
    pub fn last_used(&self) -> u64 {
        self.last_used
    }
}

// === Эвристика стабильного префикса ===

/// Оценивает длину стабильного префикса промпта в байтах.
///
/// Модель промпта: секции склеиваются разделителем [`SECTION_SEPARATOR`] —
/// `system ∷ tools ∷ user₁ ∷ user₂ ∷ …`. Эвристика стабильности:
///
/// - системный промпт и описание инструментов стабильны **всегда** — агент не
///   меняет их внутри сессии, поэтому они целиком входят в префикс;
/// - user-сообщения стабильны **никогда** — каждый ход диалога приносит новое
///   содержимое, поэтому даже первое user-сообщение в префикс не входит;
///   стабилен лишь разделитель перед ним (его байты от содержимого не зависят);
/// - пустые секции разделителя не порождают.
///
/// Возвращает число ведущих байтов промпта, гарантированно одинаковых для всех
/// запросов с данными `system` и `tools`, — ориентир для решения, имеет ли
/// смысл ставить точку излома кэша.
pub fn longest_stable_prefix(system: &str, tools: &str, user_messages: &[&str]) -> usize {
    let mut bytes = system.len();
    if !tools.is_empty() {
        if !system.is_empty() {
            bytes += SECTION_SEPARATOR.len();
        }
        bytes += tools.len();
    }
    let has_stable_part = !system.is_empty() || !tools.is_empty();
    if has_stable_part && !user_messages.is_empty() {
        bytes += SECTION_SEPARATOR.len();
    }
    bytes
}

// === LRU-кэш ===

/// LRU-кэш префиксов промптов с фиксированной ёмкостью.
///
/// Устройство: записи лежат в `HashMap<CacheKey, CacheEntry>`, порядок
/// обращений — в `BTreeMap<u64, CacheKey>` с ключом-порядком от монотонного
/// счётчика; «самая старая» запись — с минимальным порядком. И
/// [`PrefixCache::get`], и [`PrefixCache::put`] освежают порядок записи,
/// поэтому при переполнении вытесняется действительно least recently used.
///
/// Параллельно ведётся статистика: попадания/промахи
/// ([`PrefixCache::hit_rate`]) и суммарная оценка сэкономленных токенов
/// ([`PrefixCache::savings_estimate`]). Статистика монотонна: ни вытеснение,
/// ни [`PrefixCache::invalidate_model`] её не сбрасывают.
///
/// `capacity == 0` превращает кэш в выключенный: [`PrefixCache::put`] —
/// холостая операция, [`PrefixCache::get`] всегда промах (но промах считается).
#[derive(Debug)]
pub struct PrefixCache {
    /// Максимальное число записей; 0 — кэш выключен.
    capacity: usize,
    /// Записи по ключу.
    entries: HashMap<CacheKey, CacheEntry>,
    /// Порядок обращений: номер последнего доступа → ключ записи.
    lru: BTreeMap<u64, CacheKey>,
    /// Монотонный счётчик обращений (источник номеров порядка).
    clock: u64,
    /// Попадания за всё время жизни кэша.
    hits: u64,
    /// Промахи за всё время жизни кэша.
    misses: u64,
    /// Суммарная оценка сэкономленных токенов (по попаданиям).
    saved_tokens: u64,
}

impl PrefixCache {
    /// Создаёт пустой кэш ёмкостью `capacity` записей.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            lru: BTreeMap::new(),
            clock: 0,
            hits: 0,
            misses: 0,
            saved_tokens: 0,
        }
    }

    /// Ёмкость кэша (максимальное число записей).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Число записей в кэше сейчас.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true`, если в кэше нет ни одной записи.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Вставляет или обновляет запись и освежает её LRU-порядок.
    ///
    /// - если ключ уже есть — текст и оценка токенов перезаписываются, запись
    ///   становится самой «свежей» (число записей не меняется);
    /// - если ключа нет и кэш полон — перед вставкой вытесняется самая давно
    ///   не использовавшаяся запись;
    /// - при `capacity == 0` операция холостая: ничего не сохраняется.
    pub fn put(&mut self, key: CacheKey, prefix_text: impl Into<String>, hit_tokens_est: u64) {
        if self.capacity == 0 {
            return;
        }
        self.clock += 1;
        if let Some(existing) = self.entries.get_mut(&key) {
            self.lru.remove(&existing.last_used);
            existing.prefix_text = prefix_text.into();
            existing.hit_tokens_est = hit_tokens_est;
            existing.last_used = self.clock;
            self.lru.insert(self.clock, key);
            return;
        }
        while self.entries.len() >= self.capacity {
            // У каждой записи есть отметка порядка, так что при непустом кэше
            // `pop_first` всегда что-то вернёт; ветка `else` — страховка.
            if let Some((_, evicted_key)) = self.lru.pop_first() {
                self.entries.remove(&evicted_key);
            } else {
                break;
            }
        }
        let entry = CacheEntry {
            prefix_text: prefix_text.into(),
            hit_tokens_est,
            last_used: self.clock,
        };
        self.entries.insert(key.clone(), entry);
        self.lru.insert(self.clock, key);
    }

    /// Ищет запись по ключу; при попадании освежает её LRU-порядок.
    ///
    /// Попадание увеличивает счётчик hits и добавляет
    /// [`CacheEntry::hit_tokens_est`] найденной записи в оценку сэкономленных
    /// токенов; промах увеличивает счётчик misses. Возвращает копию записи.
    pub fn get(&mut self, key: &CacheKey) -> Option<CacheEntry> {
        match self.entries.get_mut(key) {
            Some(entry) => {
                self.hits += 1;
                self.clock += 1;
                self.lru.remove(&entry.last_used);
                entry.last_used = self.clock;
                self.saved_tokens = self.saved_tokens.saturating_add(entry.hit_tokens_est);
                self.lru.insert(self.clock, key.clone());
                Some(entry.clone())
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    /// Счётчики попаданий и промахов за всё время жизни кэша: `(hits, misses)`.
    ///
    /// Статистика монотонна: вытеснение и [`PrefixCache::invalidate_model`]
    /// её не сбрасывают. Долю попаданий при необходимости считает вызывающая
    /// сторона: `hits / (hits + misses)` с защитой от деления на ноль.
    pub fn hit_rate(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }

    /// Суммарная оценка токенов, сэкономленных попаданиями.
    ///
    /// Каждое попадание добавляет [`CacheEntry::hit_tokens_est`] найденной
    /// записи (актуальную на момент попадания); сложение насыщающее,
    /// переполнение невозможно. Промахи и инвалидация оценку не меняют.
    pub fn savings_estimate(&self) -> u64 {
        self.saved_tokens
    }

    /// Удаляет все записи указанной модели, возвращает число удалённых.
    ///
    /// Точечная инвалидация при смене системного промпта или набора
    /// инструментов: старые префиксы модели провайдер забудет по TTL, и
    /// держать их в локальном кэше бессмысленно. Записи других моделей и
    /// статистика (`hit_rate`, `savings_estimate`) не затрагиваются.
    pub fn invalidate_model(&mut self, model: &str) -> usize {
        let before = self.entries.len();
        let lru = &mut self.lru;
        self.entries.retain(|key, entry| {
            let keep = key.model != model;
            if !keep {
                lru.remove(&entry.last_used);
            }
            keep
        });
        before - self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Короткая форма сборки ключа в тестах.
    fn key(model: &str, system: &str, tools: &str) -> CacheKey {
        build_key(model, system, tools)
    }

    // === FNV-1a ===

    #[test]
    fn fnv1a64_of_empty_input_is_offset_basis() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn fnv1a64_matches_reference_vectors() {
        // Эталонные значения из спецификации Фаулера-Нолла-Во.
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn fnv1a64_depends_on_every_byte() {
        assert_ne!(fnv1a64(b"abc"), fnv1a64(b"abd"));
        assert_ne!(fnv1a64(b"abc"), fnv1a64(b"abc\0"));
    }

    // === build_key ===

    #[test]
    fn build_key_is_deterministic() {
        let first = key("m1", "system prompt", "[]");
        let second = key("m1", "system prompt", "[]");
        assert_eq!(first, second);
        assert_eq!(first.model, "m1");
        assert_eq!(first.system_hash, fnv1a64(b"system prompt"));
        assert_eq!(first.tools_hash, fnv1a64(b"[]"));
    }

    #[test]
    fn build_key_distinguishes_models() {
        assert_ne!(key("m1", "sys", "[]"), key("m2", "sys", "[]"));
    }

    #[test]
    fn build_key_distinguishes_system_prompts() {
        let a = key("m", "sys A", "[]");
        let b = key("m", "sys B", "[]");
        assert_ne!(a, b);
        assert_ne!(a.system_hash, b.system_hash);
    }

    #[test]
    fn build_key_distinguishes_tools_json() {
        let a = key("m", "sys", "[{\"name\":\"read\"}]");
        let b = key("m", "sys", "[{\"name\":\"write\"}]");
        assert_ne!(a, b);
        assert_ne!(a.tools_hash, b.tools_hash);
        // Хеши системного промпта при этом совпадают.
        assert_eq!(a.system_hash, b.system_hash);
    }

    // === longest_stable_prefix ===

    #[test]
    fn stable_prefix_counts_system_tools_and_separator() {
        // "sys"(3) + "\n\n"(2) + "tools"(5) = 10.
        assert_eq!(longest_stable_prefix("sys", "tools", &[]), 10);
    }

    #[test]
    fn stable_prefix_ignores_user_message_content() {
        let without_users = longest_stable_prefix("sys", "tools", &[]);
        let with_users = longest_stable_prefix("sys", "tools", &["привет", "пока"]);
        // User-сообщения добавляют только стабильный разделитель перед первым.
        assert_eq!(with_users, without_users + SECTION_SEPARATOR.len());
        // Длина самих сообщений роли не играет.
        let long_message = "очень длинное сообщение, которое не должно войти в префикс";
        let with_long = longest_stable_prefix("sys", "tools", &[long_message]);
        assert_eq!(with_users, with_long);
    }

    #[test]
    fn stable_prefix_of_empty_prompt_is_zero() {
        assert_eq!(longest_stable_prefix("", "", &[]), 0);
        // Без стабильной части user-сообщения префикса не дают.
        assert_eq!(longest_stable_prefix("", "", &["hello"]), 0);
    }

    #[test]
    fn stable_prefix_without_system_section() {
        assert_eq!(longest_stable_prefix("", "tools", &[]), 5);
        let with_user = longest_stable_prefix("", "tools", &["u"]);
        assert_eq!(with_user, 5 + SECTION_SEPARATOR.len());
    }

    #[test]
    fn stable_prefix_without_tools_section() {
        assert_eq!(longest_stable_prefix("sys", "", &[]), 3);
        let with_user = longest_stable_prefix("sys", "", &["u"]);
        assert_eq!(with_user, 3 + SECTION_SEPARATOR.len());
    }

    // === базовые put/get и статистика ===

    #[test]
    fn fresh_cache_is_empty_and_has_zero_stats() {
        let cache = PrefixCache::new(4);
        assert!(cache.is_empty());
        assert_eq!(cache.capacity(), 4);
        assert_eq!(cache.hit_rate(), (0, 0));
        assert_eq!(cache.savings_estimate(), 0);
    }

    #[test]
    fn put_then_get_returns_entry_and_counts_hit() {
        let mut cache = PrefixCache::new(4);
        let k = key("m", "sys", "[]");
        cache.put(k.clone(), "prefix text", 120);
        assert_eq!(cache.len(), 1);
        let entry = cache.get(&k).expect("запись только что вставлена");
        assert_eq!(entry.prefix_text, "prefix text");
        assert_eq!(entry.hit_tokens_est, 120);
        assert_eq!(cache.hit_rate(), (1, 0));
    }

    #[test]
    fn get_unknown_key_counts_miss() {
        let mut cache = PrefixCache::new(4);
        assert!(cache.get(&key("m", "sys", "[]")).is_none());
        assert_eq!(cache.hit_rate(), (0, 1));
    }

    #[test]
    fn hit_rate_tracks_hits_and_misses_independently() {
        let mut cache = PrefixCache::new(4);
        let present = key("m", "sys", "[]");
        let absent = key("m", "other", "[]");
        cache.put(present.clone(), "p", 10);
        assert!(cache.get(&present).is_some());
        assert!(cache.get(&absent).is_none());
        assert!(cache.get(&present).is_some());
        assert_eq!(cache.hit_rate(), (2, 1));
    }

    // === LRU ===

    #[test]
    fn lru_evicts_oldest_inserted_when_full() {
        let mut cache = PrefixCache::new(2);
        let a = key("m", "a", "[]");
        let b = key("m", "b", "[]");
        let c = key("m", "c", "[]");
        cache.put(a.clone(), "a", 1);
        cache.put(b.clone(), "b", 1);
        cache.put(c.clone(), "c", 1); // вытесняет a
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&a).is_none());
        assert!(cache.get(&b).is_some());
        assert!(cache.get(&c).is_some());
    }

    #[test]
    fn get_refreshes_recency_and_protects_from_eviction() {
        let mut cache = PrefixCache::new(2);
        let a = key("m", "a", "[]");
        let b = key("m", "b", "[]");
        let c = key("m", "c", "[]");
        cache.put(a.clone(), "a", 1);
        cache.put(b.clone(), "b", 1);
        assert!(cache.get(&a).is_some()); // a теперь свежее b
        cache.put(c.clone(), "c", 1); // вытесняет b, а не a
        assert!(cache.get(&b).is_none());
        assert!(cache.get(&a).is_some());
        assert!(cache.get(&c).is_some());
    }

    #[test]
    fn put_same_key_updates_entry_and_refreshes_recency() {
        let mut cache = PrefixCache::new(2);
        let a = key("m", "a", "[]");
        let b = key("m", "b", "[]");
        let c = key("m", "c", "[]");
        cache.put(a.clone(), "a-v1", 100);
        cache.put(b.clone(), "b", 1);
        cache.put(a.clone(), "a-v2", 200); // обновление, а не вставка
        assert_eq!(cache.len(), 2);
        cache.put(c, "c", 1); // вытесняет b: a только что обновляли
        assert!(cache.get(&b).is_none());
        let entry = cache.get(&a).expect("a должна пережить вытеснение");
        assert_eq!(entry.prefix_text, "a-v2");
        assert_eq!(entry.hit_tokens_est, 200);
    }

    #[test]
    fn capacity_one_keeps_only_newest_entry() {
        let mut cache = PrefixCache::new(1);
        let a = key("m", "a", "[]");
        let b = key("m", "b", "[]");
        cache.put(a.clone(), "a", 1);
        cache.put(b.clone(), "b", 1);
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&a).is_none());
        assert!(cache.get(&b).is_some());
    }

    #[test]
    fn capacity_zero_stores_nothing_but_counts_misses() {
        let mut cache = PrefixCache::new(0);
        let k = key("m", "sys", "[]");
        cache.put(k.clone(), "prefix", 100);
        assert!(cache.is_empty());
        assert!(cache.get(&k).is_none());
        assert_eq!(cache.hit_rate(), (0, 1));
        assert_eq!(cache.savings_estimate(), 0);
    }

    // === invalidate_model ===

    #[test]
    fn invalidate_model_removes_only_matching_entries() {
        let mut cache = PrefixCache::new(8);
        cache.put(key("m1", "a", "[]"), "a1", 1);
        cache.put(key("m1", "b", "[]"), "a2", 1);
        let survivor = key("m2", "a", "[]");
        cache.put(survivor.clone(), "b1", 1);
        let removed = cache.invalidate_model("m1");
        assert_eq!(removed, 2);
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&survivor).is_some());
    }

    #[test]
    fn invalidate_model_with_unknown_model_removes_nothing() {
        let mut cache = PrefixCache::new(8);
        cache.put(key("m1", "a", "[]"), "a", 1);
        assert_eq!(cache.invalidate_model("m2"), 0);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn invalidation_preserves_statistics() {
        let mut cache = PrefixCache::new(8);
        let k = key("m1", "a", "[]");
        cache.put(k.clone(), "a", 100);
        assert!(cache.get(&k).is_some());
        assert!(cache.get(&key("m1", "absent", "[]")).is_none());
        assert_eq!(cache.invalidate_model("m1"), 1);
        assert!(cache.is_empty());
        assert_eq!(cache.hit_rate(), (1, 1));
        assert_eq!(cache.savings_estimate(), 100);
    }

    // === savings_estimate ===

    #[test]
    fn savings_estimate_accumulates_hit_tokens_only_on_hits() {
        let mut cache = PrefixCache::new(8);
        let a = key("m", "a", "[]");
        let b = key("m", "b", "[]");
        cache.put(a.clone(), "a", 100);
        cache.put(b.clone(), "b", 50);
        assert_eq!(cache.savings_estimate(), 0);
        assert!(cache.get(&a).is_some()); // +100
        assert!(cache.get(&a).is_some()); // +100
        assert!(cache.get(&b).is_some()); // +50
        assert!(cache.get(&key("m", "absent", "[]")).is_none()); // +0
        assert_eq!(cache.savings_estimate(), 250);
    }

    #[test]
    fn savings_estimate_uses_updated_tokens_after_reput() {
        let mut cache = PrefixCache::new(8);
        let k = key("m", "a", "[]");
        cache.put(k.clone(), "v1", 100);
        assert!(cache.get(&k).is_some()); // +100
        cache.put(k.clone(), "v2", 40);
        assert!(cache.get(&k).is_some()); // +40 по новой оценке
        assert_eq!(cache.savings_estimate(), 140);
    }

    #[test]
    fn lru_order_numbers_grow_monotonically() {
        let mut cache = PrefixCache::new(8);
        let a = key("m", "a", "[]");
        let b = key("m", "b", "[]");
        cache.put(a.clone(), "a", 1);
        cache.put(b.clone(), "b", 1);
        let entry_a = cache.get(&a).expect("a вставлена");
        let entry_b = cache.get(&b).expect("b вставлена");
        assert!(entry_a.last_used() < entry_b.last_used());
    }
}
