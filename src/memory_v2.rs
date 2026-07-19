//! Структурированная память фактов (v2).
//!
//! Развитие плоского файла фактов из `memory.rs` в типизированное хранилище
//! по мотивам codex-rs/memories: каждый факт — запись с тегами, источником,
//! уверенностью и статистикой обращений. Поверх хранилища — ранжированный
//! поиск, консолидация дублей, затухание неиспользуемых фактов, поиск
//! конфликтующих пар и сериализация в человекочитаемый Markdown с разбором
//! обратно.
//!
//! Модуль самодостаточен: только std + anyhow.
//!
//! ```
//! use theseus::memory_v2::{FactSource, MemoryStore};
//!
//! let mut store = MemoryStore::new();
//! let id = store.add("У пользователя RTX 4080 SUPER", ["gpu"], FactSource::User);
//! assert_eq!(store.get(id).map(|f| f.text.as_str()), Some("У пользователя RTX 4080 SUPER"));
//! assert_eq!(store.search("какая видеокарта rtx").len(), 1);
//! ```

use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fmt::Write as _;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

/// Порог косинусной схожести по умолчанию для [`MemoryStore::conflict_candidates`]:
/// ниже него факты с общими тегами считаются потенциально конфликтующими.
pub const DEFAULT_CONFLICT_THRESHOLD: f32 = 0.3;

/// Вес уверенности в формуле ранжирования поиска (см. [`MemoryStore::search`]).
const CONFIDENCE_WEIGHT: f64 = 0.5;
/// Вес свежести в формуле ранжирования поиска (см. [`MemoryStore::search`]).
const RECENCY_WEIGHT: f64 = 0.2;
/// Период полураспада свежести в днях: факт месячной давности получает
/// половину максимального recency-буста.
const RECENCY_HALF_LIFE_DAYS: f64 = 30.0;

/// Часы хранилища: возвращают «сейчас» в секундах UNIX-времени.
pub type Clock = Box<dyn Fn() -> u64 + Send + Sync>;

/// Источник факта.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactSource {
    /// Извлечён агентом из диалога в рамках сессии.
    Session,
    /// Записан пользователем вручную.
    User,
    /// Порождён циклом консолидации (дедупликация/слияние).
    Consolidation,
}

impl fmt::Display for FactSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            FactSource::Session => "session",
            FactSource::User => "user",
            FactSource::Consolidation => "consolidation",
        };
        f.write_str(s)
    }
}

impl FromStr for FactSource {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "session" => Ok(FactSource::Session),
            "user" => Ok(FactSource::User),
            "consolidation" => Ok(FactSource::Consolidation),
            other => bail!("неизвестный источник факта: «{other}»"),
        }
    }
}

/// Один структурированный факт памяти.
#[derive(Debug, Clone)]
pub struct Fact {
    /// Уникальный id (монотонный, начиная с 1), присваивает хранилище.
    pub id: u64,
    /// Текст факта (обрезан по краям при добавлении).
    pub text: String,
    /// Теги: нижний регистр, отсортированы, без дублей.
    pub tags: BTreeSet<String>,
    /// Источник факта.
    pub source: FactSource,
    /// Момент создания (секунды UNIX-времени, по часам хранилища).
    pub created_at: u64,
    /// Уверенность в факте, 0.0..=1.0.
    pub confidence: f32,
    /// Сколько раз факт реально использовали (см. [`MemoryStore::touch`]).
    pub access_count: u32,
}

impl Fact {
    /// Ключ дедупликации — нормализованный текст.
    fn key(&self) -> String {
        normalize_text(&self.text)
    }
}

/// Хранилище фактов с поиском, консолидацией и затуханием.
///
/// Часы вынесены в параметр [`MemoryStore::with_clock`], чтобы тесты и
/// воспроизводимые прогоны могли подменить время.
pub struct MemoryStore {
    facts: BTreeMap<u64, Fact>,
    next_id: u64,
    clock: Clock,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    /// Пустое хранилище на системных часах.
    pub fn new() -> Self {
        Self::with_clock(Box::new(system_now))
    }

    /// Хранилище на произвольных часах (тесты, воспроизводимость).
    pub fn with_clock(clock: Clock) -> Self {
        MemoryStore { facts: BTreeMap::new(), next_id: 1, clock }
    }

    /// Число фактов в хранилище.
    pub fn len(&self) -> usize {
        self.facts.len()
    }

    /// Пусто ли хранилище.
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }

    /// Добавить факт; возвращает присвоенный id.
    ///
    /// Уверенность нового факта — 1.0, теги нормализуются (обрезка,
    /// нижний регистр), текст обрезается по краям.
    pub fn add<I, S>(&mut self, text: &str, tags: I, source: FactSource) -> u64
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let id = self.next_id;
        self.next_id += 1;
        let fact = Fact {
            id,
            text: text.trim().to_string(),
            tags: normalize_tags(tags),
            source,
            created_at: (self.clock)(),
            confidence: 1.0,
            access_count: 0,
        };
        self.facts.insert(id, fact);
        id
    }

    /// Факт по id.
    pub fn get(&self, id: u64) -> Option<&Fact> {
        self.facts.get(&id)
    }

    /// Изменяемая ссылка на факт (ручная корректировка confidence и т.п.).
    pub fn get_mut(&mut self, id: u64) -> Option<&mut Fact> {
        self.facts.get_mut(&id)
    }

    /// Отметить реальное использование факта: инкремент `access_count`.
    ///
    /// Харнесс обязан вызывать это для фактов, которые действительно ушли
    /// в контекст агента, — иначе [`MemoryStore::decay`] сочтёт их
    /// неиспользуемыми.
    pub fn touch(&mut self, id: u64) {
        if let Some(f) = self.facts.get_mut(&id) {
            f.access_count = f.access_count.saturating_add(1);
        }
    }

    /// Поиск по пересечению слов с бустами по уверенности и свежести.
    ///
    /// Score: `overlap + 0.5 * confidence + 0.2 * recency`, где `overlap` —
    /// доля уникальных слов запроса, встретившихся в тексте факта (факты без
    /// пересечения отбрасываются), `recency = 0.5^(age_days / 30)`. Слова —
    /// буквенно-цифровые токены Unicode длиной от 2 символов. Равный score
    /// разрешается в пользу более свежего id. Поиск не меняет статистику
    /// обращений (см. [`MemoryStore::touch`]).
    pub fn search(&self, query: &str) -> Vec<&Fact> {
        let qset: HashSet<String> = tokenize(query).collect();
        if qset.is_empty() {
            return Vec::new();
        }
        let now = (self.clock)();
        let mut scored: Vec<(f64, &Fact)> = self
            .facts
            .values()
            .filter_map(|f| {
                let words: HashSet<String> = tokenize(&f.text).collect();
                let hits = qset.iter().filter(|w| words.contains(*w)).count();
                if hits == 0 {
                    return None;
                }
                let overlap = hits as f64 / qset.len() as f64;
                let age_days = now.saturating_sub(f.created_at) as f64 / 86_400.0;
                let recency = 0.5f64.powf(age_days / RECENCY_HALF_LIFE_DAYS);
                let score = overlap + CONFIDENCE_WEIGHT * f64::from(f.confidence) + RECENCY_WEIGHT * recency;
                Some((score, f))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal).then_with(|| b.1.id.cmp(&a.1.id))
        });
        scored.into_iter().map(|(_, f)| f).collect()
    }

    /// Консолидировать пачку фактов-кандидатов в хранилище.
    ///
    /// Дедупликация по нормализованному тексту (нижний регистр, вся
    /// пунктуация схлопывается в пробелы): дубли сливаются — теги
    /// объединяются, confidence усредняется. Если такой текст уже есть в
    /// хранилище, слияние идёт в существующий факт с наименьшим id (его id,
    /// source и created_at сохраняются); иначе факт добавляется с источником
    /// [`FactSource::Consolidation`]. Поля id/created_at/access_count
    /// входных фактов игнорируются.
    ///
    /// Возвращает итоговые факты (по одному на уникальный текст) в порядке
    /// первого появления текста во входной пачке.
    pub fn consolidate(&mut self, facts: Vec<Fact>) -> Vec<Fact> {
        // Группировка входных фактов по ключу нормализованного текста.
        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, MergeAcc> = HashMap::new();
        for f in facts {
            let key = normalize_text(&f.text);
            if key.is_empty() {
                continue; // пустые и чисто-пунктуационные тексты пропускаем
            }
            match groups.entry(key) {
                Entry::Occupied(mut o) => o.get_mut().absorb(f),
                Entry::Vacant(v) => {
                    order.push(v.key().clone());
                    v.insert(MergeAcc::from(f));
                }
            }
        }
        // Индекс существующих фактов по ключу: наименьший id — канонический.
        let mut key_to_id: HashMap<String, u64> = HashMap::new();
        for f in self.facts.values() {
            key_to_id.entry(f.key()).or_insert(f.id);
        }
        // Слияние: в существующий факт или новой записью от консолидации.
        let mut out = Vec::with_capacity(order.len());
        for key in order {
            if let Some(acc) = groups.remove(&key) {
                let conf = acc.avg_confidence();
                match key_to_id.get(&key) {
                    Some(&id) => {
                        if let Some(f) = self.facts.get_mut(&id) {
                            f.tags.extend(acc.tags);
                            f.confidence = f.confidence.midpoint(conf);
                            out.push(f.clone());
                        }
                    }
                    None => {
                        let id = self.insert_consolidated(acc.text, acc.tags, conf);
                        if let Some(f) = self.facts.get(&id) {
                            out.push(f.clone());
                        }
                    }
                }
            }
        }
        out
    }

    /// Затухание неиспользуемых фактов (вызывать раз в консолидационный цикл).
    ///
    /// Факты с `access_count == 0` (их не трогали с прошлого цикла) получают
    /// `confidence *= factor`; factor зажимается в 0.0..=1.0. После прохода
    /// счётчики обращений всех фактов сбрасываются. Возвращает число фактов,
    /// чья уверенность прошла через затухание.
    pub fn decay(&mut self, factor: f32) -> usize {
        let factor = factor.clamp(0.0, 1.0);
        let mut decayed = 0;
        for f in self.facts.values_mut() {
            if f.access_count == 0 {
                f.confidence = (f.confidence * factor).clamp(0.0, 1.0);
                decayed += 1;
            }
            f.access_count = 0;
        }
        decayed
    }

    /// Пары фактов-кандидатов на конфликт с порогом по умолчанию
    /// [`DEFAULT_CONFLICT_THRESHOLD`].
    pub fn conflict_candidates(&self) -> Vec<(u64, u64)> {
        self.conflict_candidates_with_threshold(DEFAULT_CONFLICT_THRESHOLD)
    }

    /// Пары `(id, id)` фактов с общими тегами, но низкой схожестью текстов.
    ///
    /// Схожесть — косинусная между мультимножествами слов (точное совпадение
    /// слов, без стемминга). Пара попадает в ответ, если пересечение тегов
    /// непусто, а косинус строго меньше `threshold`. Первый id пары всегда
    /// меньше; пары отсортированы по возрастанию обоих id.
    pub fn conflict_candidates_with_threshold(&self, threshold: f32) -> Vec<(u64, u64)> {
        let facts: Vec<&Fact> = self.facts.values().collect();
        let vectors: Vec<HashMap<String, u32>> = facts.iter().map(|f| word_counts(&f.text)).collect();
        let mut out = Vec::new();
        for (i, a) in facts.iter().enumerate() {
            for (j, b) in facts.iter().enumerate().skip(i + 1) {
                if a.tags.is_disjoint(&b.tags) {
                    continue;
                }
                if cosine(&vectors[i], &vectors[j]) < f64::from(threshold) {
                    out.push((a.id, b.id));
                }
            }
        }
        out
    }

    /// Сериализация всего хранилища в человекочитаемый Markdown.
    ///
    /// Формат блока: заголовок `## Fact <id>`, затем frontmatter между парой
    /// линий `---` (tags/source/created_at/confidence/access_count), затем
    /// сырой текст факта до следующего заголовка. Ограничение: строка вида
    /// `## Fact <число>` внутри текста трактуется как начало нового блока —
    /// такие строки в текстах фактов недопустимы.
    pub fn to_markdown(&self) -> String {
        let n = self.facts.len();
        let mut out = String::from("# Theseus Memory v2\n");
        let _ = writeln!(out, "# facts: {n}");
        for f in self.facts.values() {
            let id = f.id;
            let tags = f.tags.iter().map(String::as_str).collect::<Vec<_>>().join(", ");
            let src = f.source;
            let created = f.created_at;
            let conf = f.confidence;
            let accesses = f.access_count;
            let text = f.text.trim();
            let _ = write!(
                out,
                "\n## Fact {id}\n---\ntags: {tags}\nsource: {src}\ncreated_at: {created}\nconfidence: {conf}\naccess_count: {accesses}\n---\n{text}\n"
            );
        }
        out
    }

    /// Разбор Markdown, произведённого [`MemoryStore::to_markdown`].
    ///
    /// Толерантен к произвольным строкам вне блоков фактов (шапка файла) и
    /// к неизвестным ключам frontmatter. Ошибки: битый id, битые числа в
    /// frontmatter, дублирующиеся id.
    ///
    /// # Errors
    /// Возвращает `Err` при нарушении формата блока `## Fact`.
    pub fn from_markdown(md: &str) -> Result<Self> {
        let mut store = MemoryStore::new();
        let mut cur: Option<ParsedFact> = None;
        for line in md.lines() {
            if let Some(rest) = line.strip_prefix("## Fact ") {
                if let Some(p) = cur.take() {
                    store.insert_parsed(p)?;
                }
                let id: u64 =
                    rest.trim().parse().with_context(|| format!("некорректный id факта: «{rest}»"))?;
                cur = Some(ParsedFact::new(id));
                continue;
            }
            if let Some(p) = cur.as_mut() {
                p.feed(line)?;
            }
            // Строки до первого «## Fact» — комментарии шапки, игнорируем.
        }
        if let Some(p) = cur.take() {
            store.insert_parsed(p)?;
        }
        store.next_id = store.facts.keys().next_back().map_or(1, |max| *max + 1);
        Ok(store)
    }

    /// Вставка разобранного из Markdown факта с проверкой дубля id.
    fn insert_parsed(&mut self, p: ParsedFact) -> Result<()> {
        let id = p.id;
        let fact = Fact {
            id,
            text: p.text.trim().to_string(),
            tags: p.tags,
            source: p.source,
            created_at: p.created_at,
            confidence: p.confidence.clamp(0.0, 1.0),
            access_count: p.access_count,
        };
        match self.facts.entry(id) {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(fact);
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                bail!("дублирующийся id факта: {id}")
            }
        }
    }

    /// Вставка результата консолидации новой записью (свежий id и «сейчас»).
    fn insert_consolidated(&mut self, text: String, tags: BTreeSet<String>, confidence: f32) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let fact = Fact {
            id,
            text,
            tags,
            source: FactSource::Consolidation,
            created_at: (self.clock)(),
            confidence: confidence.clamp(0.0, 1.0),
            access_count: 0,
        };
        self.facts.insert(id, fact);
        id
    }
}

/// Системные часы: секунды UNIX-времени (0 при сбое часов).
fn system_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Разбор текста на слова: буквенно-цифровые токены Unicode в нижнем
/// регистре, длиной от 2 символов (односимвольные — шум).
fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.chars().count() >= 2)
        .map(str::to_lowercase)
}

/// Нормализация текста для дедупликации: нижний регистр, любые
/// не-буквенно-цифровые последовательности схлопываются в один пробел.
fn normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(ch);
        } else {
            pending_space = true;
        }
    }
    out
}

/// Нормализация тегов: обрезка, нижний регистр, отбрасывание пустых.
fn normalize_tags<I, S>(tags: I) -> BTreeSet<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    tags.into_iter()
        .map(|t| t.into().trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Мультимножество слов текста для косинусной схожести.
fn word_counts(text: &str) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    for w in tokenize(text) {
        *counts.entry(w).or_insert(0) += 1;
    }
    counts
}

/// Косинусная схожесть двух мультимножеств слов, 0.0..=1.0.
fn cosine(a: &HashMap<String, u32>, b: &HashMap<String, u32>) -> f64 {
    let (small, big) = if a.len() > b.len() { (b, a) } else { (a, b) };
    let dot: f64 = small
        .iter()
        .filter_map(|(w, c)| big.get(w).map(|c2| f64::from(*c) * f64::from(*c2)))
        .sum();
    let norm = |m: &HashMap<String, u32>| m.values().map(|c| f64::from(*c).powi(2)).sum::<f64>().sqrt();
    let (na, nb) = (norm(a), norm(b));
    if na <= f64::EPSILON || nb <= f64::EPSILON {
        0.0 // пустой вектор слов — схожести нет
    } else {
        dot / (na * nb)
    }
}

/// Аккумулятор слияния дублей при консолидации.
struct MergeAcc {
    /// Текст первого из дублей (обрезанный).
    text: String,
    /// Объединение тегов всех дублей.
    tags: BTreeSet<String>,
    /// Сумма уверенностей для последующего усреднения.
    conf_sum: f64,
    /// Сколько дублей слилось.
    n: u32,
}

impl From<Fact> for MergeAcc {
    fn from(f: Fact) -> Self {
        MergeAcc {
            text: f.text.trim().to_string(),
            tags: normalize_tags(f.tags),
            conf_sum: f64::from(f.confidence.clamp(0.0, 1.0)),
            n: 1,
        }
    }
}

impl MergeAcc {
    /// Поглотить очередной дубль: теги в объединение, confidence в сумму.
    fn absorb(&mut self, f: Fact) {
        self.tags.extend(normalize_tags(f.tags));
        self.conf_sum += f64::from(f.confidence.clamp(0.0, 1.0));
        self.n += 1;
    }

    /// Средняя уверенность по слившимся дублям.
    fn avg_confidence(&self) -> f32 {
        (self.conf_sum / f64::from(self.n)) as f32
    }
}

/// Состояние разбора одного блока `## Fact` из Markdown.
struct ParsedFact {
    id: u64,
    tags: BTreeSet<String>,
    source: FactSource,
    created_at: u64,
    confidence: f32,
    access_count: u32,
    /// Мы между парой линий `---`.
    in_frontmatter: bool,
    /// Вторая `---` уже встретилась: дальше только текст факта.
    frontmatter_closed: bool,
    /// Накопленный текст факта.
    text: String,
}

impl ParsedFact {
    /// Разбор блока с заданным id; поля — значения по умолчанию.
    fn new(id: u64) -> Self {
        ParsedFact {
            id,
            tags: BTreeSet::new(),
            source: FactSource::Session,
            created_at: 0,
            confidence: 1.0,
            access_count: 0,
            in_frontmatter: false,
            frontmatter_closed: false,
            text: String::new(),
        }
    }

    /// Скормить разбору очередную строку блока.
    fn feed(&mut self, line: &str) -> Result<()> {
        if line == "---" && !self.frontmatter_closed {
            self.frontmatter_closed = self.in_frontmatter;
            self.in_frontmatter = !self.in_frontmatter;
            return Ok(());
        }
        if self.in_frontmatter {
            let Some((key, value)) = line.split_once(':') else {
                return Ok(()); // мусор в frontmatter игнорируем
            };
            let value = value.trim();
            match key.trim() {
                "tags" => {
                    self.tags = value
                        .split(',')
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(str::to_lowercase)
                        .collect();
                }
                "source" => self.source = value.parse()?,
                "created_at" => {
                    self.created_at =
                        value.parse().with_context(|| format!("некорректный created_at: «{value}»"))?;
                }
                "confidence" => {
                    self.confidence =
                        value.parse().with_context(|| format!("некорректный confidence: «{value}»"))?;
                }
                "access_count" => {
                    self.access_count =
                        value.parse().with_context(|| format!("некорректный access_count: «{value}»"))?;
                }
                _ => {} // неизвестные ключи — задел на будущие поля
            }
            return Ok(());
        }
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(line);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    const DAY: u64 = 86_400;

    /// Хранилище на ручных часах для детерминированных тестов.
    fn manual_store(t: u64) -> (MemoryStore, Arc<AtomicU64>) {
        let now = Arc::new(AtomicU64::new(t));
        let clock = Arc::clone(&now);
        let store = MemoryStore::with_clock(Box::new(move || clock.load(Ordering::SeqCst)));
        (store, now)
    }

    /// Сравнение f32 с допуском (float_cmp под запретом).
    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    /// Факт-кандидат для consolidate (id/created_at/access_count игнорятся).
    fn draft(text: &str, tags: &[&str], confidence: f32) -> Fact {
        Fact {
            id: 0,
            text: text.to_string(),
            tags: tags.iter().map(ToString::to_string).collect(),
            source: FactSource::Session,
            created_at: 0,
            confidence,
            access_count: 0,
        }
    }

    #[test]
    fn add_assigns_monotonic_ids_and_normalizes() {
        let (mut s, _) = manual_store(1_000);
        let id1 = s.add("  Первый факт  ", ["GPU", " gpu ", "Rust"], FactSource::Session);
        let id2 = s.add("Второй факт", Vec::<String>::new(), FactSource::User);
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(s.len(), 2);
        assert!(!s.is_empty());
        let f = s.get(1).unwrap();
        assert_eq!(f.text, "Первый факт");
        assert_eq!(f.tags.iter().map(String::as_str).collect::<Vec<_>>(), vec!["gpu", "rust"]);
        assert_eq!(f.source, FactSource::Session);
        assert_eq!(f.created_at, 1_000);
        assert!(approx(f.confidence, 1.0));
        assert_eq!(f.access_count, 0);
        assert!(s.get(99).is_none());
    }

    #[test]
    fn search_ranks_by_overlap() {
        let (mut s, _) = manual_store(5_000);
        s.add("rust cargo clippy llvm", ["pl"], FactSource::Session);
        s.add("rust cargo", ["pl"], FactSource::Session);
        s.add("rust borrow checker", ["pl"], FactSource::Session);
        let ids: Vec<u64> = s.search("rust cargo clippy").iter().map(|f| f.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        // Поиск не считается использованием факта.
        assert_eq!(s.get(1).unwrap().access_count, 0);
    }

    #[test]
    fn search_boosts_by_confidence() {
        let (mut s, _) = manual_store(5_000);
        let a = s.add("async runtime tokio executor", ["rust"], FactSource::Session);
        let b = s.add("async runtime tokio executor", ["rust"], FactSource::Session);
        s.get_mut(a).unwrap().confidence = 0.2;
        s.get_mut(b).unwrap().confidence = 0.9;
        let hits = s.search("async tokio");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, b);
        assert_eq!(hits[1].id, a);
    }

    #[test]
    fn search_boosts_by_recency() {
        let (mut s, now) = manual_store(1_000_000);
        let old = s.add("kernel linux scheduler cgroups", ["os"], FactSource::Session);
        now.store(1_000_000 + 120 * DAY, Ordering::SeqCst);
        let new = s.add("kernel linux modules ebpf", ["os"], FactSource::Session);
        assert_eq!(s.get(old).unwrap().created_at, 1_000_000);
        assert_eq!(s.get(new).unwrap().created_at, 1_000_000 + 120 * DAY);
        let hits = s.search("kernel linux");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, new);
        assert_eq!(hits[1].id, old);
    }

    #[test]
    fn search_empty_and_stopword_queries() {
        let (mut s, _) = manual_store(0);
        s.add("rust cargo", Vec::<&str>::new(), FactSource::Session);
        assert!(s.search("").is_empty());
        assert!(s.search("   ").is_empty());
        assert!(s.search("!?.,").is_empty());
        assert!(s.search("я у в").is_empty()); // все «слова» короче 2 букв
        assert!(s.search("haskell ocaml").is_empty()); // нет пересечения
    }

    #[test]
    fn touch_protects_from_decay() {
        let (mut s, _) = manual_store(0);
        let a = s.add("факт а", ["t"], FactSource::Session);
        let b = s.add("факт б", ["t"], FactSource::Session);
        s.touch(a);
        s.touch(a);
        assert_eq!(s.decay(0.5), 1);
        assert!(approx(s.get(a).unwrap().confidence, 1.0));
        assert!(approx(s.get(b).unwrap().confidence, 0.5));
        // Счётчики сброшены: следующий проход бьёт уже по всем.
        assert_eq!(s.get(a).unwrap().access_count, 0);
        assert_eq!(s.decay(0.5), 2);
        assert!(approx(s.get(a).unwrap().confidence, 0.5));
        assert!(approx(s.get(b).unwrap().confidence, 0.25));
    }

    #[test]
    fn decay_clamps_factor() {
        let (mut s, _) = manual_store(0);
        let a = s.add("факт", ["t"], FactSource::User);
        s.get_mut(a).unwrap().confidence = 0.4;
        s.decay(1.7); // зажимается до 1.0 — без изменений
        assert!(approx(s.get(a).unwrap().confidence, 0.4));
        s.decay(-3.0); // зажимается до 0.0 — обнуление
        assert!(approx(s.get(a).unwrap().confidence, 0.0));
    }

    #[test]
    fn consolidate_dedups_merges_tags_and_averages_confidence() {
        let (mut s, _) = manual_store(42);
        let merged = s.consolidate(vec![
            draft("У пользователя RTX 4080 SUPER", &["gpu", "hardware"], 0.8),
            draft("у пользователя rtx 4080 super!", &["GPU", "nvidia"], 1.0),
            draft("Проект theseus живёт в harness-review", &["project"], 0.6),
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!(s.len(), 2);
        let f0 = &merged[0];
        assert_eq!(f0.text, "У пользователя RTX 4080 SUPER"); // первый вариант текста
        assert_eq!(f0.tags.iter().map(String::as_str).collect::<Vec<_>>(), vec!["gpu", "hardware", "nvidia"]);
        assert!(approx(f0.confidence, 0.9));
        assert_eq!(f0.source, FactSource::Consolidation);
        assert_eq!(f0.created_at, 42);
        assert_eq!(merged[1].id, 2);
        assert!(approx(merged[1].confidence, 0.6));
    }

    #[test]
    fn consolidate_merges_into_existing_fact() {
        let (mut s, _) = manual_store(7);
        let id = s.add("Агент использует sandbox landlock", ["sec"], FactSource::User);
        let merged = s.consolidate(vec![draft("агент использует sandbox landlock", &["sec", "linux"], 0.5)]);
        assert_eq!(merged.len(), 1);
        assert_eq!(s.len(), 1); // новых фактов не появилось
        assert_eq!(merged[0].id, id);
        let f = s.get(id).unwrap();
        assert_eq!(f.source, FactSource::User); // источник оригинала сохранён
        assert_eq!(f.tags.iter().map(String::as_str).collect::<Vec<_>>(), vec!["linux", "sec"]);
        assert!(approx(f.confidence, 0.75));
    }

    #[test]
    fn consolidate_skips_empty_texts() {
        let (mut s, _) = manual_store(0);
        let merged = s.consolidate(vec![draft("   ", &["x"], 1.0), draft("!?..", &["x"], 1.0)]);
        assert!(merged.is_empty());
        assert!(s.is_empty());
        // Пропущенные тексты не съедают id.
        assert_eq!(s.add("настоящий факт", Vec::<&str>::new(), FactSource::Session), 1);
    }

    #[test]
    fn conflict_candidates_flags_same_tag_low_similarity() {
        let (mut s, _) = manual_store(0);
        let a = s.add("пользователь любит тёмную тему оформления", ["ui"], FactSource::Session);
        let b = s.add("светлая тема читается лучше днём", ["ui"], FactSource::Session);
        // Тот же текст, что у a, но без общего тега — не конфликт.
        let c = s.add("пользователь любит тёмную тему оформления", ["prefs"], FactSource::Session);
        let pairs = s.conflict_candidates();
        assert_eq!(pairs, vec![(a, b)]);
        assert!(pairs.iter().all(|p| p.0 != c && p.1 != c));
    }

    #[test]
    fn conflict_candidates_ignores_similar_texts() {
        let (mut s, _) = manual_store(0);
        let a = s.add("пользователь любит тёмную тему", ["ui"], FactSource::Session);
        let b = s.add("пользователь любит тёмную тему очень", ["ui"], FactSource::Session);
        // Косинус ~0.89 — выше порога 0.3, конфликта нет...
        assert!(s.conflict_candidates().is_empty());
        // ...но с завышенным порогом пара всплывёт.
        assert_eq!(s.conflict_candidates_with_threshold(1.0), vec![(a, b)]);
    }

    #[test]
    fn markdown_roundtrip() {
        let (mut s, now) = manual_store(1_700_000_000);
        let a = s.add("У пользователя RTX 4080 SUPER 16GB", ["gpu", "hardware"], FactSource::User);
        let b = s.add("Многострочный\nфакт про настройки\nокружения", ["env"], FactSource::Session);
        let c = s.add("Факт от консолидации", ["meta"], FactSource::Consolidation);
        s.get_mut(b).unwrap().confidence = 0.75;
        for _ in 0..4 {
            s.touch(c);
        }
        now.fetch_add(3600, Ordering::SeqCst); // created_at фактов — до сдвига
        let md = s.to_markdown();
        assert!(md.contains("## Fact 1"));
        assert!(md.contains("tags: gpu, hardware"));
        assert!(md.contains("source: consolidation"));
        let mut loaded = MemoryStore::from_markdown(&md).unwrap();
        assert_eq!(loaded.len(), 3);
        for id in [a, b, c] {
            let (x, y) = (s.get(id).unwrap(), loaded.get(id).unwrap());
            assert_eq!(x.text, y.text);
            assert_eq!(x.tags, y.tags);
            assert_eq!(x.source, y.source);
            assert_eq!(x.created_at, y.created_at);
            assert!(approx(x.confidence, y.confidence));
            assert_eq!(x.access_count, y.access_count);
        }
        // Повторная сериализация даёт тот же текст.
        assert_eq!(md, loaded.to_markdown());
        // next_id восстановлен за максимальным id.
        assert_eq!(loaded.add("новый факт", Vec::<&str>::new(), FactSource::Session), 4);
    }

    #[test]
    fn from_markdown_rejects_garbage() {
        assert!(MemoryStore::from_markdown("## Fact xyz\n---\n---\nтекст").is_err());
        let dup = "## Fact 1\n---\n---\nпервый\n\n## Fact 1\n---\n---\nвторой\n";
        assert!(MemoryStore::from_markdown(dup).is_err());
        assert!(MemoryStore::from_markdown("## Fact 1\n---\nconfidence: не-число\n---\nтекст").is_err());
        assert!(MemoryStore::from_markdown("## Fact 1\n---\nsource: alien\n---\nтекст").is_err());
    }

    #[test]
    fn from_markdown_tolerates_empty_and_unknown() {
        let s = MemoryStore::from_markdown("").unwrap();
        assert!(s.is_empty());
        let md = "# какая-то шапка\nпроизвольный текст\n\n## Fact 5\n---\ntags: a, b\nfuture_key: значение\n---\nтекст факта\n";
        let mut s = MemoryStore::from_markdown(md).unwrap();
        let f = s.get(5).unwrap();
        assert_eq!(f.text, "текст факта");
        assert_eq!(f.tags.len(), 2);
        assert_eq!(f.source, FactSource::Session); // дефолт
        assert_eq!(f.created_at, 0); // дефолт
        assert!(approx(f.confidence, 1.0)); // дефолт
        assert_eq!(s.add("ещё один", Vec::<&str>::new(), FactSource::Session), 6);
    }
}
