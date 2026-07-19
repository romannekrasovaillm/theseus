//! Семантическая дедупликация сообщений для компактификации (v2).
//!
//! Развитие L2-дедупа из `agent/compact.rs` (там — только точное совпадение
//! по fingerprint) в *семантический* поиск дублей по образцу OpenDev ACC:
//! каждое сообщение сжимается в 64-битный simhash, похожие тексты дают малое
//! расстояние Хэмминга и объединяются в кластеры дублей. Из кластера выживает
//! «самый свежий длинный» вариант, остальные помечаются к выбросу. Системные
//! сообщения и последние [`TAIL_PROTECTED`] сообщений истории неприкосновенны;
//! дубликаты tool_result — первые кандидаты на выброс (повторные чтения одних
//! и тех же файлов — главный мусор контекста агента).
//!
//! Модуль самодостаточен: только std. Планировщик ничего не удаляет сам — он
//! возвращает [`DedupPlan`], а окончательное решение принимает вызывающий код.
//!
//! ```
//! use theseus::compact_v2::{hamming, simhash64, DedupPlanner, MsgKind};
//!
//! let a = simhash64("прочитай файл конфигурации и покажи параметры сети");
//! let b = simhash64("прочитай файл конфигурации и покажи параметры сети");
//! assert_eq!(hamming(a, b), 0);
//!
//! let msgs = [
//!     (MsgKind::System, "ты — агент-харнесс"),
//!     (MsgKind::ToolResult, "содержимое файла: а б в г д е ж з и к"),
//!     (MsgKind::ToolResult, "содержимое файла: а б в г д е ж з и к"),
//!     (MsgKind::User, "что в файле?"),
//!     (MsgKind::Assistant, "сейчас посмотрю"),
//!     (MsgKind::Assistant, "готово"),
//! ];
//! let plan = DedupPlanner::new().plan(&msgs);
//! assert_eq!(plan.dropped_len(), 1);
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Порог расстояния Хэмминга по умолчанию для [`DedupPlanner::plan`]:
/// сообщения с simhash на расстоянии не больше этого числа бит считаются
/// семантическими дублями.
pub const DEFAULT_HAMMING_THRESHOLD: u32 = 10;

/// Число сообщений в хвосте истории, которые [`DedupPlanner::plan`] не трогает
/// никогда: свежий контекст нужен агенту целиком.
pub const TAIL_PROTECTED: usize = 4;

/// Минимум слов-токенов, начиная с которого simhash строится по словам;
/// для более коротких текстов слов слишком мало для устойчивой сигнатуры,
/// поэтому используются символьные 3-граммы.
const SHORT_TOKEN_LIMIT: usize = 8;

/// Длина символьной n-граммы для коротких текстов.
const NGRAM_LEN: usize = 3;

/// 64-битный simhash текста (Charikar): частотно-взвешенная сумма хэшей
/// признаков со знаковым накоплением по каждому из 64 бит.
///
/// Признаки — слова в нижнем регистре (частота слова = его вес); если слов
/// меньше порога `SHORT_TOKEN_LIMIT` — символьные 3-граммы нормализованного
/// текста. Хэш признака — FNV-1a с финализатором splitmix64: результат
/// детерминирован между запусками и платформами (в отличие от SipHash из std).
/// Пустой текст даёт сигнатуру 0; два пустых текста — «дубли» друг друга.
pub fn simhash64(text: &str) -> u64 {
    let features = extract_features(text);
    if features.is_empty() {
        return 0;
    }
    // знаковое суммирование: бит признака 1 → +вес, бит 0 → −вес
    let mut acc = [0i64; 64];
    for (feature, weight) in &features {
        let h = feature_hash(feature);
        let w = i64::from(*weight);
        for (bit, slot) in acc.iter_mut().enumerate() {
            if (h >> bit) & 1 == 1 {
                *slot += w;
            } else {
                *slot -= w;
            }
        }
    }
    // бит результата выставлен, если итоговая сумма по нему положительна
    acc.iter().enumerate().fold(0u64, |mut out, (bit, sum)| {
        if *sum > 0 {
            out |= 1 << bit;
        }
        out
    })
}

/// Расстояние Хэмминга между двумя simhash — число различающихся бит (0..=64).
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Сигнатура одного сообщения: индекс в истории, simhash и размер в байтах.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsgSig {
    /// Индекс сообщения в истории диалога.
    pub id: usize,
    /// 64-битный simhash текста (см. [`simhash64`]).
    pub hash: u64,
    /// Размер исходного текста в байтах.
    pub bytes: usize,
}

/// Вид сообщения истории — от него зависят правила защиты в [`DedupPlanner::plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgKind {
    /// Системный промпт: неприкосновенен всегда.
    System,
    /// Реплика пользователя.
    User,
    /// Реплика ассистента (сюда же попадает любая нераспознанная роль).
    Assistant,
    /// Результат инструмента: дубликаты таких сообщений — первые кандидаты
    /// на выброс.
    ToolResult,
}

impl MsgKind {
    /// Маппинг строковой роли API (`system`/`user`/`assistant`/`tool`) в вид
    /// сообщения; неизвестные роли трактуются как обычные реплики ассистента.
    pub fn from_role(role: &str) -> Self {
        match role {
            "system" => Self::System,
            "user" => Self::User,
            "tool" | "tool_result" => Self::ToolResult,
            _ => Self::Assistant,
        }
    }
}

/// План дедупликации: что оставить и что выбросить, с объяснением причин.
#[derive(Debug, Clone, Default)]
pub struct DedupPlan {
    /// Индексы сообщений, которые остаются в истории (все индексы входа минус
    /// выброшенные).
    pub keep: BTreeSet<usize>,
    /// `(индекс, причина)` для каждого выбрасываемого сообщения; дубликаты
    /// tool_result идут первыми, внутри групп — по возрастанию индекса.
    pub drop_with_reason: Vec<(usize, String)>,
}

impl DedupPlan {
    /// Сколько сообщений предлагается выбросить.
    pub fn dropped_len(&self) -> usize {
        self.drop_with_reason.len()
    }

    /// Предлагается ли выбросить сообщение с индексом `idx`.
    pub fn is_dropped(&self, idx: usize) -> bool {
        self.drop_with_reason.iter().any(|(i, _)| *i == idx)
    }
}

/// Планировщик семантической дедупликации: накапливает сигнатуры через
/// [`DedupPlanner::add`] и строит кластеры дублей / итоговый план.
#[derive(Debug, Default)]
pub struct DedupPlanner {
    sigs: Vec<MsgSig>,
}

impl DedupPlanner {
    /// Пустой планировщик без сигнатур.
    pub fn new() -> Self {
        Self::default()
    }

    /// Добавить сообщение с индексом `idx`: вычисляет simhash текста и
    /// запоминает его размер в байтах.
    pub fn add(&mut self, idx: usize, text: &str) {
        self.sigs.push(MsgSig { id: idx, hash: simhash64(text), bytes: text.len() });
    }

    /// Число накопленных сигнатур.
    pub fn len(&self) -> usize {
        self.sigs.len()
    }

    /// Пуст ли планировщик.
    pub fn is_empty(&self) -> bool {
        self.sigs.is_empty()
    }

    /// Кластеры дублей среди накопленных сигнатур: single-linkage объединение
    /// всех пар с `hamming <= threshold_bits`.
    ///
    /// Возвращает строки `(keep, drop, sim)`:
    /// - `keep` — индекс выжившего в кластере, «самый свежий длинный»
    ///   (максимум по паре `(байты, индекс)`);
    /// - `drop` — индекс выбрасываемого дубля;
    /// - `sim` — схожесть с выжившим в совпадающих битах (`64 − hamming`);
    ///   для «цепочных» членов кластера может быть ниже порога — это норма
    ///   для single-linkage.
    ///
    /// Строки отсортированы по `(keep, drop)` — результат детерминирован.
    pub fn duplicates(&self, threshold_bits: u32) -> Vec<(usize, usize, u32)> {
        let n = self.sigs.len();
        let mut parent: Vec<usize> = (0..n).collect();
        for i in 0..n {
            for j in (i + 1)..n {
                if hamming(self.sigs[i].hash, self.sigs[j].hash) <= threshold_bits {
                    union(&mut parent, i, j);
                }
            }
        }
        let mut clusters: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for i in 0..n {
            let root = find(&mut parent, i);
            clusters.entry(root).or_default().push(i);
        }
        let mut rows = Vec::new();
        for members in clusters.values() {
            if members.len() < 2 {
                continue;
            }
            let Some(&keep) = members.iter().max_by_key(|&&m| (self.sigs[m].bytes, self.sigs[m].id))
            else {
                continue;
            };
            for &m in members {
                // keep сам себе не дубль; повторный add того же idx не порождает строку
                if m == keep || self.sigs[m].id == self.sigs[keep].id {
                    continue;
                }
                let sim = 64 - hamming(self.sigs[keep].hash, self.sigs[m].hash);
                rows.push((self.sigs[keep].id, self.sigs[m].id, sim));
            }
        }
        rows.sort_unstable();
        rows
    }

    /// Однопроходный план дедупликации истории `(вид, текст)` с порогом
    /// [`DEFAULT_HAMMING_THRESHOLD`].
    ///
    /// Правила:
    /// - системные сообщения неприкосновенны: участвуют в кластерах как
    ///   возможный «эталон», но никогда не попадают в список выброса;
    /// - последние [`TAIL_PROTECTED`] сообщений истории неприкосновенны;
    /// - дубликаты tool_result — первые в списке выброса, затем остальные
    ///   дубликаты (обе группы — по возрастанию индекса).
    pub fn plan(&self, messages: &[(MsgKind, &str)]) -> DedupPlan {
        let n = messages.len();
        let protected_from = n.saturating_sub(TAIL_PROTECTED);
        let mut inner = DedupPlanner::new();
        for (i, (_, text)) in messages.iter().enumerate() {
            inner.add(i, text);
        }
        let mut tool_drops: Vec<(usize, String)> = Vec::new();
        let mut other_drops: Vec<(usize, String)> = Vec::new();
        for (keep_idx, drop_idx, sim) in inner.duplicates(DEFAULT_HAMMING_THRESHOLD) {
            let (kind, _) = messages[drop_idx];
            if kind == MsgKind::System || drop_idx >= protected_from {
                continue;
            }
            let dist = 64 - sim;
            let prefix = if kind == MsgKind::ToolResult { "дубликат tool_result" } else { "дубликат" };
            let reason =
                format!("{prefix} #{keep_idx}: hamming {dist} ≤ {DEFAULT_HAMMING_THRESHOLD}, схожесть {sim}/64 бит");
            if kind == MsgKind::ToolResult {
                tool_drops.push((drop_idx, reason));
            } else {
                other_drops.push((drop_idx, reason));
            }
        }
        tool_drops.sort_by_key(|(i, _)| *i);
        other_drops.sort_by_key(|(i, _)| *i);
        let mut drop_with_reason = tool_drops;
        drop_with_reason.append(&mut other_drops);
        let keep = (0..n).filter(|i| !drop_with_reason.iter().any(|(d, _)| d == i)).collect();
        DedupPlan { keep, drop_with_reason }
    }
}

/// Грубая оценка экономии в байтах: сумма размеров выбрасываемых сообщений.
/// `sizes[i]` — размер i-го сообщения истории; индекс вне диапазона даёт 0.
/// Заглушки-замены («[dedup] …») не вычитаются — это оценка сверху.
pub fn estimate_savings(plan: &DedupPlan, sizes: &[usize]) -> usize {
    plan.drop_with_reason.iter().map(|(i, _)| sizes.get(*i).copied().unwrap_or(0)).sum()
}

/// Нормализация текста: нижний регистр, только буквы/цифры и одиночные
/// пробелы-разделители (без ведущего и хвостового пробела).
fn normalize(text: &str) -> String {
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

/// Признаки и их частоты: слова нормализованного текста, а для коротких
/// текстов — символьные 3-граммы (у совсем крошечных, короче 3-граммы,
/// единственный признак — вся строка целиком).
fn extract_features(text: &str) -> HashMap<String, u32> {
    let norm = normalize(text);
    let mut freq: HashMap<String, u32> = HashMap::new();
    let words: Vec<&str> = norm.split(' ').filter(|w| !w.is_empty()).collect();
    if words.len() >= SHORT_TOKEN_LIMIT {
        for w in words {
            *freq.entry(w.to_string()).or_insert(0) += 1;
        }
    } else {
        let chars: Vec<char> = norm.chars().collect();
        if chars.len() < NGRAM_LEN {
            if !norm.is_empty() {
                freq.insert(norm, 1);
            }
        } else {
            for gram in chars.windows(NGRAM_LEN) {
                *freq.entry(gram.iter().collect()).or_insert(0) += 1;
            }
        }
    }
    freq
}

/// FNV-1a по байтам признака + финализатор splitmix64 для лавинного эффекта.
/// Полностью детерминирован — не зависит от версии std и платформы.
fn feature_hash(feature: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in feature.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// find с усечением пути, без рекурсии.
fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

/// union по корням: меньший корень подвешивается к большему (детерминизм).
fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        parent[ra.min(rb)] = ra.max(rb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Длинный «технический» текст (26 слов) — эталон для пары схожих.
    const SERVER_TEXT: &str = "агент прочитал файл конфигурации сервера и вывел параметры сетевого интерфейса адреса шлюзы маски подсети таблицу маршрутизации журнал ошибок за последние сутки и статистику подключений базы данных";

    /// Тот же текст с заменой двух слов — семантический дубль эталона.
    const SERVER_TEXT_EDIT: &str = "агент просмотрел файл конфигурации сервера и вывел параметры сетевого интерфейса адреса шлюзы маски подсети таблицу маршрутизации журнал ошибок за прошлые сутки и статистику подключений базы данных";

    /// Совершенно другая тематика (джаз) — дальняя пара для эталона;
    /// общие служебные слова убраны, чтобы расстояние было честно большим.
    const JAZZ_TEXT: &str = "джазовая импровизация строится блюзовых гаммах свинговом ритме синкопах фразировке дыхании ансамбля слухе музыканта диалоге инструментов неожиданных гармонических поворотах тембре";

    #[test]
    fn identical_texts_have_zero_hamming() {
        let a = simhash64(SERVER_TEXT);
        let b = simhash64(SERVER_TEXT);
        assert_eq!(a, b);
        assert_eq!(hamming(a, b), 0);
    }

    #[test]
    fn similar_texts_within_ten_bits() {
        let d = hamming(simhash64(SERVER_TEXT), simhash64(SERVER_TEXT_EDIT));
        assert!(d <= 10, "схожие тексты: ожидалось ≤ 10 бит, получено {d}");
    }

    #[test]
    fn different_texts_beyond_thirty_bits() {
        let d = hamming(simhash64(SERVER_TEXT), simhash64(JAZZ_TEXT));
        assert!(d > 30, "разные тексты: ожидалось > 30 бит, получено {d}");
    }

    #[test]
    fn simhash_is_deterministic_golden() {
        // золотое значение зафиксировано после первого прогона: защита от
        // случайного изменения алгоритма хэширования признаков
        assert_eq!(simhash64("детерминизм"), 0xaff2_c7cb_a85c_8f9b);
        assert_eq!(simhash64("детерминизм"), simhash64("детерминизм"));
        assert_eq!(simhash64(""), 0);
    }

    #[test]
    fn empty_and_tiny_texts() {
        assert_eq!(simhash64(""), 0);
        assert_eq!(simhash64(" \t\n"), 0);
        // крошечный текст (короче 3-граммы) — один признак, хэш ненулевой
        assert_ne!(simhash64("да"), 0);
        // два пустых текста формально дубли: расстояние 0
        assert_eq!(hamming(simhash64(""), simhash64("  ")), 0);
    }

    #[test]
    fn short_texts_use_char_ngrams() {
        // пунктуация срезается нормализацией → полное совпадение
        assert_eq!(hamming(simhash64("ok"), simhash64("ok!")), 0);
        // короткие, но разные по смыслу строки должны различаться заметно
        let d = hamming(simhash64("да, конечно сделаю"), simhash64("нет, никогда не буду"));
        assert!(d > 10, "короткие разные тексты: {d}");
    }

    #[test]
    fn hamming_properties() {
        assert_eq!(hamming(0, 0), 0);
        assert_eq!(hamming(0, u64::MAX), 64);
        assert_eq!(hamming(0b1010, 0b0101), 4);
        // симметрия
        assert_eq!(hamming(42, 7), hamming(7, 42));
    }

    #[test]
    fn msg_kind_from_role_mapping() {
        assert_eq!(MsgKind::from_role("system"), MsgKind::System);
        assert_eq!(MsgKind::from_role("user"), MsgKind::User);
        assert_eq!(MsgKind::from_role("assistant"), MsgKind::Assistant);
        assert_eq!(MsgKind::from_role("tool"), MsgKind::ToolResult);
        assert_eq!(MsgKind::from_role("tool_result"), MsgKind::ToolResult);
        // неизвестная роль → обычная реплика ассистента
        assert_eq!(MsgKind::from_role("что-угодно"), MsgKind::Assistant);
    }

    #[test]
    fn planner_collects_sigs() {
        let mut p = DedupPlanner::new();
        assert!(p.is_empty());
        assert_eq!(p.len(), 0);
        p.add(0, SERVER_TEXT);
        p.add(1, JAZZ_TEXT);
        assert_eq!(p.len(), 2);
        assert!(!p.is_empty());
    }

    #[test]
    fn duplicates_keep_freshest_on_equal_length() {
        let mut p = DedupPlanner::new();
        p.add(0, SERVER_TEXT);
        p.add(1, SERVER_TEXT); // идентичный, но новее
        p.add(2, JAZZ_TEXT); // одиночка — не кластер
        let rows = p.duplicates(DEFAULT_HAMMING_THRESHOLD);
        assert_eq!(rows.len(), 1);
        let (keep, drop, sim) = rows[0];
        // при равной длине выживает более свежий (больший индекс)
        assert_eq!((keep, drop), (1, 0));
        // идентичные тексты — полное совпадение всех 64 бит
        assert_eq!(sim, 64);
    }

    #[test]
    fn duplicates_keep_longer_over_fresher() {
        let longer = format!("{SERVER_TEXT} хвост");
        let mut p = DedupPlanner::new();
        p.add(0, &longer); // длиннее, но старее
        p.add(1, SERVER_TEXT); // короче, но новее
        let rows = p.duplicates(DEFAULT_HAMMING_THRESHOLD);
        assert_eq!(rows.len(), 1, "близкие тексты должны склеиться в кластер");
        // длина важнее свежести
        assert_eq!(rows[0].0, 0);
        assert_eq!(rows[0].1, 1);
    }

    #[test]
    fn duplicates_cluster_of_three() {
        let mut p = DedupPlanner::new();
        p.add(0, SERVER_TEXT);
        p.add(1, SERVER_TEXT);
        p.add(2, SERVER_TEXT_EDIT);
        let rows = p.duplicates(DEFAULT_HAMMING_THRESHOLD);
        // один кластер из трёх → один keep и двое выброшенных
        assert_eq!(rows.len(), 2);
        let keep = rows[0].0;
        assert!(rows.iter().all(|(k, _, _)| *k == keep));
        let mut drops: Vec<usize> = rows.iter().map(|(_, d, _)| *d).collect();
        drops.sort_unstable();
        let expect: Vec<usize> = (0..3).filter(|i| *i != keep).collect();
        assert_eq!(drops, expect);
    }

    #[test]
    fn zero_threshold_means_exact_match_only() {
        let mut p = DedupPlanner::new();
        p.add(0, SERVER_TEXT);
        p.add(1, SERVER_TEXT_EDIT); // близкий, но не идентичный
        p.add(2, SERVER_TEXT);
        let rows = p.duplicates(0);
        assert_eq!(rows.len(), 1, "при пороге 0 клеятся только точные совпадения");
        assert_eq!(rows[0], (2, 0, 64));
    }

    #[test]
    fn plan_protects_system_message() {
        let msgs = [
            (MsgKind::System, SERVER_TEXT),
            (MsgKind::User, SERVER_TEXT), // точный дубль системного
            (MsgKind::User, "первый вопрос пользователя про настройку сети"),
            (MsgKind::Assistant, "ответ ассистента с пояснениями"),
            (MsgKind::User, "уточняющий вопрос"),
            (MsgKind::Assistant, "финальный ответ"),
        ];
        let plan = DedupPlanner::new().plan(&msgs);
        // системный — дубль #1, но выбросить нельзя; #1 при этом выживает сам
        assert!(!plan.is_dropped(0));
        assert!(plan.keep.contains(&0));
        assert_eq!(plan.dropped_len(), 0);
    }

    #[test]
    fn plan_protects_tail_four() {
        let msgs = [
            (MsgKind::User, "старое сообщение истории номер один"),
            (MsgKind::Assistant, SERVER_TEXT), // старый дубль — выбрасываем
            (MsgKind::User, "вопрос два"),
            (MsgKind::Assistant, SERVER_TEXT), // дубль #1, но в защищённом хвосте
            (MsgKind::User, "вопрос три"),
            (MsgKind::Assistant, "ещё один ответ"),
        ];
        let plan = DedupPlanner::new().plan(&msgs);
        // n=6 → хвост с индекса 2; #3 неприкосновенен, зато старый #1 уходит
        assert!(!plan.is_dropped(3));
        assert!(plan.keep.contains(&3));
        assert!(plan.is_dropped(1));
        assert_eq!(plan.keep, BTreeSet::from([0, 2, 3, 4, 5]));
    }

    #[test]
    fn plan_drops_tool_duplicates_first() {
        let msgs = [
            (MsgKind::ToolResult, SERVER_TEXT), // 0: дубль #1 (tool)
            (MsgKind::User, SERVER_TEXT),       // 1: выживает (свежее, та же длина)
            (MsgKind::User, JAZZ_TEXT),       // 2: дубль #3 (user)
            (MsgKind::ToolResult, JAZZ_TEXT), // 3: выживает (свежее)
            (MsgKind::User, "хвост один"),
            (MsgKind::Assistant, "хвост два"),
            (MsgKind::User, "хвост три"),
            (MsgKind::Assistant, "хвост четыре"),
        ];
        let plan = DedupPlanner::new().plan(&msgs);
        assert_eq!(plan.dropped_len(), 2);
        // tool_result-дубль обязан идти первым в списке выброса
        assert_eq!(plan.drop_with_reason[0].0, 0);
        assert!(plan.drop_with_reason[0].1.contains("tool_result"));
        assert_eq!(plan.drop_with_reason[1].0, 2);
        assert!(plan.keep.contains(&1));
        assert!(plan.keep.contains(&3));
    }

    #[test]
    fn plan_empty_and_short_history() {
        let p = DedupPlanner::new();
        let empty = p.plan(&[]);
        assert!(empty.keep.is_empty());
        assert_eq!(empty.dropped_len(), 0);
        // вся история короче защищённого хвоста → даже точные дубли не трогаем
        let msgs =
            [(MsgKind::User, SERVER_TEXT), (MsgKind::User, SERVER_TEXT), (MsgKind::Assistant, "ответ")];
        let plan = p.plan(&msgs);
        assert_eq!(plan.dropped_len(), 0);
        assert_eq!(plan.keep.len(), 3);
    }

    #[test]
    fn savings_sum_dropped_sizes() {
        let plan = DedupPlan {
            keep: BTreeSet::from([1, 2]),
            drop_with_reason: vec![(0, "причина а".to_string()), (3, "причина б".to_string())],
        };
        let sizes = [100, 50, 40, 200];
        assert_eq!(estimate_savings(&plan, &sizes), 300);
        // пустой план → нулевая экономия
        assert_eq!(estimate_savings(&DedupPlan::default(), &sizes), 0);
        // индекс вне диапазона sizes — ноль, а не паника
        let out_of_range =
            DedupPlan { keep: BTreeSet::new(), drop_with_reason: vec![(9, "x".to_string())] };
        assert_eq!(estimate_savings(&out_of_range, &sizes), 0);
    }
}
