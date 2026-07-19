//! Поиск по новостным дайджестам и HF-коллекциям библиотеки `recipes_taxonomy`.
//!
//! Два источника данных (константы [`NEWS_ROOT`] и [`HF_ROOT`]):
//!
//! * Новостные дайджесты — markdown-файлы `YYYY-MM-DD_<заголовок>.md` в
//!   подкаталогах-источниках (`AINews`, `Raschka`, ...): [`scan_digests`]
//!   обходит корень на глубину ≤2, [`search_digests`] ранжирует по словам
//!   запроса в заголовке, имени файла и первых [`TEXT_WINDOW_BYTES`] байтах
//!   текста, [`read_digest`] читает файл с обрезкой по числу символов.
//! * HF-коллекции — `collections_data.json` вида `{ "<slug>": { поля } }`:
//!   [`load_collections`] разбирает dict в `Vec<HfCollection>` (дефолты для
//!   отсутствующих полей), [`search_collections`] ранжирует по заголовку,
//!   теме и `items_preview`/`description` с фильтром по провайдеру.
//!
//! Даты — строки ISO `YYYY-MM-DD`, сравниваются лексикографически;
//! арифметика дней — civil-алгоритм Ховарда Хиннанта, без внешних крейтов.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Корень новостных дайджестов (подкаталоги-источники с `YYYY-MM-DD_*.md`).
pub const NEWS_ROOT: &str =
    "/home/roman/Документы/КОД/gigachat/РАЗБОРЫ/recipes_taxonomy/08_Новостные_дайджесты";

/// Корень HF-дайджестов и `collections_data.json`.
pub const HF_ROOT: &str =
    "/home/roman/Документы/КОД/gigachat/РАЗБОРЫ/recipes_taxonomy/09_HF_Коллекции_провайдеров";

/// Длина префикса даты `YYYY-MM-DD` в имени файла.
const DATE_LEN: usize = 10;

/// Сколько байт от начала файла читается для скоринга и сниппета (6 КБ).
const TEXT_WINDOW_BYTES: usize = 6 * 1024;

/// Очки за слово запроса в заголовке дайджеста.
const SCORE_TITLE: u64 = 40;

/// Очки за слово запроса в имени файла.
const SCORE_FILENAME: u64 = 20;

/// Очки за каждое вхождение слова запроса в окно текста дайджеста.
const SCORE_TEXT_PER_MATCH: u64 = 10;

/// Потолок суммарных очков за вхождения в тексте одного дайджеста.
const TEXT_SCORE_CAP: u64 = 100;

/// Максимальная длина сниппета в символах.
const SNIPPET_MAX_CHARS: usize = 200;

/// Очки за слово запроса в заголовке HF-коллекции.
const SCORE_HF_TITLE: u64 = 50;

/// Очки за слово запроса в теме HF-коллекции.
const SCORE_HF_THEME: u64 = 30;

/// Очки за каждое вхождение слова в `items_preview`/`description` коллекции.
const SCORE_HF_TEXT_PER_MATCH: u64 = 10;

/// Одна запись дайджеста: разобранное имя файла плюс путь.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestEntry {
    /// Дата публикации, строка ISO `YYYY-MM-DD` (первые 10 символов имени).
    pub date: String,
    /// Источник: имя родительского каталога или метка корня.
    pub source: String,
    /// Заголовок: остаток имени без даты и `.md`, подчёркивания → пробелы.
    pub title: String,
    /// Полный путь к markdown-файлу.
    pub path: PathBuf,
}

/// Находка поиска по дайджестам: запись, очки и сниппет.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestHit {
    /// Запись, по которой совпал запрос.
    pub entry: DigestEntry,
    /// Итоговые очки ранжирования (см. [`search_digests`]).
    pub score: u64,
    /// Первая строка текста с совпадением, обрезанная до
    /// [`SNIPPET_MAX_CHARS`] символов; пуста, если совпадение только в
    /// заголовке/имени файла.
    pub snippet: String,
}

/// HF-коллекция из `collections_data.json`.
///
/// Все поля опциональны при разборе: отсутствующие получают дефолты
/// (`""`/`0`). Поле `provider_name` в JSON может называться и
/// `provider_name`, и `provider_label` — оба варианта читаются.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HfCollection {
    /// Слаг вида `provider/name-hash`; при пустом значении подставляется
    /// ключ dict из JSON.
    #[serde(default)]
    pub slug: String,
    /// URL коллекции на huggingface.co.
    #[serde(default)]
    pub url: String,
    /// Человекочитаемый заголовок.
    #[serde(default)]
    pub title: String,
    /// Число лайков коллекции.
    #[serde(default)]
    pub upvotes: u64,
    /// Метка последнего обновления (ISO-строка HF).
    #[serde(default)]
    pub last_updated: String,
    /// Число элементов коллекции.
    #[serde(default)]
    pub item_count: u64,
    /// Превью первых элементов одной строкой.
    #[serde(default)]
    pub items_preview: String,
    /// Описание коллекции (может быть пустым).
    #[serde(default)]
    pub description: String,
    /// Тематическая рубрика.
    #[serde(default)]
    pub theme: String,
    /// Оценка релевантности из сборщика дайджестов.
    #[serde(default)]
    pub relevance: u64,
    /// Ключ провайдера (slug-владелец), напр. `deepseek-ai`.
    #[serde(default)]
    pub provider_key: String,
    /// Отображаемое имя провайдера (в JSON также `provider_label`).
    #[serde(default, alias = "provider_label")]
    pub provider_name: String,
}

/// Обходит `root` на глубину до двух уровней и собирает файлы
/// `YYYY-MM-DD_*.md` в список записей.
///
/// Файлы верхнего уровня получают `source = source_label`, файлы
/// подкаталогов — `source` равный имени родительского каталога. Нечитаемые
/// каталоги и файлы с не-UTF-8 именами молча пропускаются. Результат
/// отсортирован новыми датами вперёд (при равной дате — по источнику
/// и заголовку).
pub fn scan_digests(root: &Path, source_label: &str) -> Vec<DigestEntry> {
    let mut entries = Vec::new();
    let Ok(items) = fs::read_dir(root) else {
        return entries;
    };
    for item in items.flatten() {
        let path = item.path();
        if path.is_dir() {
            let source = item.file_name().to_string_lossy().into_owned();
            collect_dated_files(&path, &source, &mut entries);
        } else if let Some(entry) = parse_entry(&path, source_label) {
            entries.push(entry);
        }
    }
    entries.sort_by(|a, b| {
        b.date
            .cmp(&a.date)
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.title.cmp(&b.title))
    });
    entries
}

/// Собирает датированные `.md`-файлы одного каталога (без углубления).
fn collect_dated_files(dir: &Path, source: &str, entries: &mut Vec<DigestEntry>) {
    let Ok(items) = fs::read_dir(dir) else {
        return;
    };
    for item in items.flatten() {
        let path = item.path();
        if !path.is_file() {
            continue;
        }
        if let Some(entry) = parse_entry(&path, source) {
            entries.push(entry);
        }
    }
}

/// Разбирает путь в запись, если имя файла соответствует `YYYY-MM-DD_*.md`.
fn parse_entry(path: &Path, source: &str) -> Option<DigestEntry> {
    let name = path.file_name()?.to_str()?;
    let (date, title) = parse_dated_name(name)?;
    Some(DigestEntry {
        date,
        source: source.to_owned(),
        title,
        path: path.to_path_buf(),
    })
}

/// Разбирает имя `YYYY-MM-DD_<заголовок>.md` в `(дата, заголовок)`.
///
/// Возвращает `None` для файлов без дата-префикса, с невалидной датой
/// (месяц/день вне диапазона), без подчёркивания после даты или с пустым
/// заголовком.
fn parse_dated_name(name: &str) -> Option<(String, String)> {
    let stem = name.strip_suffix(".md")?;
    if stem.len() <= DATE_LEN {
        return None;
    }
    // `get` вместо `split_at`: байт 10 может оказаться внутри UTF-8-символа
    // (например, у файла «без_даты.md») — тогда имя просто не подходит.
    let date = stem.get(..DATE_LEN)?;
    parse_iso_date(date)?;
    let title = stem[DATE_LEN..].strip_prefix('_')?.replace('_', " ");
    if title.is_empty() {
        return None;
    }
    Some((date.to_owned(), title))
}

/// Ищет слова запроса по дайджестам и возвращает находки по убыванию очков.
///
/// Скоринг (на каждое слово запроса): [`SCORE_TITLE`] — слово есть в
/// заголовке, [`SCORE_FILENAME`] — в имени файла,
/// [`SCORE_TEXT_PER_MATCH`] за каждое вхождение в первые
/// [`TEXT_WINDOW_BYTES`] байт текста (суммарный текстовый вклад ограничен
/// [`TEXT_SCORE_CAP`]). Слова короче двух символов отбрасываются; пустой
/// запрос (или из одних коротких слов) даёт пустой результат.
///
/// `days = Some(n)` оставляет записи не старше `n` дней от **максимальной
/// даты в `entries`** (не от «сегодня»): граница `max_date - n` включается.
/// Сравнение дат лексикографическое по ISO-строкам. `None` — без фильтра.
///
/// Сниппет — первая строка окна текста с совпадением, обрезанная до
/// [`SNIPPET_MAX_CHARS`] символов. Равные очки разрешаются по дате (новые
/// вперёд) и заголовку. Нечитаемые файлы считаются пустыми.
pub fn search_digests(
    entries: &[DigestEntry],
    query: &str,
    days: Option<u32>,
    limit: usize,
) -> Vec<DigestHit> {
    let tokens = tokenize(query);
    if tokens.is_empty() || limit == 0 {
        return Vec::new();
    }
    let cutoff = days.and_then(|n| {
        entries
            .iter()
            .map(|e| e.date.as_str())
            .max()
            .and_then(|max_date| shift_date(max_date, -i64::from(n)))
    });
    let mut hits = Vec::new();
    for entry in entries {
        if let Some(cut) = &cutoff {
            if entry.date < *cut {
                continue;
            }
        }
        let title_lower = entry.title.to_lowercase();
        let filename_lower = entry
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let mut score = 0_u64;
        for token in &tokens {
            if title_lower.contains(token.as_str()) {
                score += SCORE_TITLE;
            }
            if filename_lower.contains(token.as_str()) {
                score += SCORE_FILENAME;
            }
        }
        let window = read_text_window(&entry.path);
        let window_lower = window.to_lowercase();
        let mut text_score = 0_u64;
        for token in &tokens {
            text_score += SCORE_TEXT_PER_MATCH * count_occurrences(&window_lower, token);
        }
        score += text_score.min(TEXT_SCORE_CAP);
        if score == 0 {
            continue;
        }
        let snippet = make_snippet(&window, &tokens);
        hits.push(DigestHit {
            entry: entry.clone(),
            score,
            snippet,
        });
    }
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.entry.date.cmp(&a.entry.date))
            .then_with(|| a.entry.title.cmp(&b.entry.title))
    });
    hits.truncate(limit);
    hits
}

/// Читает дайджест целиком и обрезает до `max_chars` **символов** (не байт,
/// юникодные кодпоинты не разрываются).
///
/// # Errors
/// Ошибка ввода-вывода при отсутствующем/нечитаемом или не-UTF-8 файле.
pub fn read_digest(path: &Path, max_chars: usize) -> std::io::Result<String> {
    let text = fs::read_to_string(path)?;
    Ok(text.chars().take(max_chars).collect())
}

/// Загружает `collections_data.json` (dict «slug → поля коллекции»)
/// в список [`HfCollection`], отсортированный по slug.
///
/// Отсутствующие поля получают дефолты; пустой `slug` подставляется
/// из ключа dict.
///
/// # Errors
/// Ошибка чтения файла или разбора JSON.
pub fn load_collections(json_path: &Path) -> Result<Vec<HfCollection>> {
    let raw = fs::read_to_string(json_path)
        .with_context(|| format!("не удалось прочитать {}", json_path.display()))?;
    let map: BTreeMap<String, HfCollection> = serde_json::from_str(&raw)
        .with_context(|| format!("не удалось разобрать JSON в {}", json_path.display()))?;
    Ok(map
        .into_iter()
        .map(|(key, mut col)| {
            if col.slug.is_empty() {
                col.slug = key;
            }
            col
        })
        .collect())
}

/// Ищет слова запроса по HF-коллекциям; возвращает ссылки на совпавшие.
///
/// Скоринг (на каждое слово): [`SCORE_HF_TITLE`] — слово в заголовке,
/// [`SCORE_HF_THEME`] — в теме, [`SCORE_HF_TEXT_PER_MATCH`] за каждое
/// вхождение в `items_preview` + `description` (без потолка).
/// `provider = Some(p)` оставляет коллекции, чей `provider_key` содержит
/// `p` без учёта регистра. Пустой запрос даёт пустой результат.
///
/// Сортировка: очки ↓, затем `upvotes` ↓, затем заголовок по алфавиту.
pub fn search_collections<'a>(
    cols: &'a [HfCollection],
    query: &str,
    provider: Option<&str>,
    limit: usize,
) -> Vec<&'a HfCollection> {
    let tokens = tokenize(query);
    if tokens.is_empty() || limit == 0 {
        return Vec::new();
    }
    let provider_lower = provider.map(str::to_lowercase);
    let mut scored: Vec<(&HfCollection, u64)> = cols
        .iter()
        .filter(|col| {
            provider_lower
                .as_ref()
                .is_none_or(|p| col.provider_key.to_lowercase().contains(p.as_str()))
        })
        .filter_map(|col| {
            let title_lower = col.title.to_lowercase();
            let theme_lower = col.theme.to_lowercase();
            let preview_lower = col.items_preview.to_lowercase();
            let description_lower = col.description.to_lowercase();
            let text_lower = format!("{preview_lower}\n{description_lower}");
            let mut score = 0_u64;
            for token in &tokens {
                if title_lower.contains(token.as_str()) {
                    score += SCORE_HF_TITLE;
                }
                if theme_lower.contains(token.as_str()) {
                    score += SCORE_HF_THEME;
                }
                score += SCORE_HF_TEXT_PER_MATCH * count_occurrences(&text_lower, token);
            }
            (score > 0).then_some((col, score))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.0.upvotes.cmp(&a.0.upvotes))
            .then_with(|| a.0.title.cmp(&b.0.title))
    });
    scored.truncate(limit);
    scored.into_iter().map(|(col, _)| col).collect()
}

/// Разбивает запрос на слова (≥2 буквенно-цифровых символов) в нижнем
/// регистре, без дубликатов.
fn tokenize(query: &str) -> Vec<String> {
    let mut tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= 2)
        .map(str::to_lowercase)
        .collect();
    tokens.sort_unstable();
    tokens.dedup();
    tokens
}

/// Число непересекающихся вхождений `needle` в `haystack`.
fn count_occurrences(haystack: &str, needle: &str) -> u64 {
    haystack.match_indices(needle).count() as u64
}

/// Читает первые [`TEXT_WINDOW_BYTES`] байт файла; ошибки чтения → пустая
/// строка. Граница окна может разрезать UTF-8-последовательность — хвост
/// заменяется lossy-декодированием, на скоринг это не влияет.
fn read_text_window(path: &Path) -> String {
    let Ok(bytes) = fs::read(path) else {
        return String::new();
    };
    let end = bytes.len().min(TEXT_WINDOW_BYTES);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Первая строка окна, содержащая хотя бы один токен (без учёта регистра),
/// обрезанная до [`SNIPPET_MAX_CHARS`] символов; пустая, если совпадений
/// в тексте нет.
fn make_snippet(window: &str, tokens: &[String]) -> String {
    window
        .lines()
        .find(|line| {
            let lower = line.to_lowercase();
            tokens.iter().any(|token| lower.contains(token.as_str()))
        })
        .map_or(String::new(), |line| {
            line.trim().chars().take(SNIPPET_MAX_CHARS).collect()
        })
}

/// Разбирает строгую ISO-дату `YYYY-MM-DD` (ровно 10 ASCII-символов,
/// месяц 1–12, день не больше числа дней в месяце) в `(год, месяц, день)`.
fn parse_iso_date(s: &str) -> Option<(i64, u32, u32)> {
    let b = s.as_bytes();
    if b.len() != DATE_LEN || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let digit = |i: usize| -> Option<u32> {
        let d = b[i].wrapping_sub(b'0');
        (d <= 9).then_some(u32::from(d))
    };
    let year = digit(0)? * 1000 + digit(1)? * 100 + digit(2)? * 10 + digit(3)?;
    let month = digit(5)? * 10 + digit(6)?;
    let day = digit(8)? * 10 + digit(9)?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(i64::from(year), month) {
        return None;
    }
    Some((i64::from(year), month, day))
}

/// Число дней в месяце с учётом високосных лет.
fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        // после проверки диапазона в `parse_iso_date` остаётся февраль
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// Дней от 1970-01-01 до григорианской даты (civil-алгоритм Х. Хиннанта).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let m = i64::from(m);
    let d = i64::from(d);
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11], март = 0
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Григорианская дата по числу дней от 1970-01-01 (обратная к
/// [`days_from_civil`]).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    // m и d по построению лежат в [1, 12] и [1, 31] — приведение безопасно.
    (y, m as u32, d as u32)
}

/// Сдвигает ISO-дату на `delta_days` дней (знак учитывается);
/// `None` для невалидного входа.
fn shift_date(date: &str, delta_days: i64) -> Option<String> {
    let (y, m, d) = parse_iso_date(date)?;
    let (y2, m2, d2) = civil_from_days(days_from_civil(y, m, d) + delta_days);
    Some(format!("{y2:04}-{m2:02}-{d2:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Мини-фикстура временного каталога: убирает за собой в `Drop`.
    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("theseus-digests-{tag}-{pid}-{seq}"));
            fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        /// Пишет файл по относительному пути, создавая родительские каталоги.
        fn write(&self, rel: &str, content: &str) -> PathBuf {
            let path = self.dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// Короткий конструктор коллекции для табличных кейсов.
    fn col(title: &str, upvotes: u64) -> HfCollection {
        HfCollection {
            title: title.to_owned(),
            upvotes,
            ..Default::default()
        }
    }

    #[test]
    fn dated_name_parses_date_and_title() {
        let (date, title) = parse_dated_name("2026-07-10_GPT_5.6_Sol_Terra.md").unwrap();
        assert_eq!(date, "2026-07-10");
        assert_eq!(title, "GPT 5.6 Sol Terra");
    }

    #[test]
    fn dated_name_rejects_mismatched_files() {
        assert!(parse_dated_name("README.md").is_none());
        assert!(parse_dated_name("2026-13-40_плохая_дата.md").is_none());
        assert!(parse_dated_name("2026-07-10.md").is_none()); // нет подчёркивания
        assert!(parse_dated_name("2026-07-10_заметки.txt").is_none()); // не markdown
        assert!(parse_dated_name("2026-07-10_.md").is_none()); // пустой заголовок
        assert!(parse_dated_name("2026-7-1_непадded.md").is_none()); // непаддинговая дата
    }

    #[test]
    fn scan_walks_depth_two_and_labels_sources() {
        let fx = Fixture::new("scan-depth");
        fx.write("2026-07-18_Корневой_дайджест.md", "# корень");
        fx.write("AINews/2026-07-17_Новости_AI.md", "# ai");
        fx.write("AINews/глубже/2026-07-16_Слишком_глубоко.md", "# глубина 3");
        fx.write("AINews/заметки.txt", "не markdown");
        fx.write("Raschka/без_даты.md", "нет префикса");

        let entries = scan_digests(&fx.dir, "корень");
        assert_eq!(entries.len(), 2);
        let pairs: Vec<(&str, &str)> = entries
            .iter()
            .map(|e| (e.title.as_str(), e.source.as_str()))
            .collect();
        assert!(pairs.contains(&("Корневой дайджест", "корень")));
        assert!(pairs.contains(&("Новости AI", "AINews")));
    }

    #[test]
    fn scan_sorts_newest_first() {
        let fx = Fixture::new("scan-sort");
        fx.write("a/2026-07-01_Старый.md", "");
        fx.write("a/2026-07-18_Новый.md", "");
        fx.write("2026-07-10_Средний.md", "");

        let entries = scan_digests(&fx.dir, "корень");
        let dates: Vec<&str> = entries.iter().map(|e| e.date.as_str()).collect();
        assert_eq!(dates, ["2026-07-18", "2026-07-10", "2026-07-01"]);
    }

    #[test]
    fn search_ranks_title_hit_above_text_hit() {
        let fx = Fixture::new("search-rank");
        fx.write("2026-07-18_Нейросети_и_харнессы.md", "текст без мишени");
        fx.write("2026-07-17_Прочее.md", "нейросети упомянуты один раз");

        let entries = scan_digests(&fx.dir, "тест");
        let hits = search_digests(&entries, "нейросети", None, 10);
        assert_eq!(hits.len(), 2);
        // заголовок (40) + имя файла (20) против 10 за одно вхождение в текст
        assert_eq!(hits[0].entry.title, "Нейросети и харнессы");
        assert_eq!(hits[0].score, 60);
        assert_eq!(hits[1].score, 10);
    }

    #[test]
    fn search_caps_text_score_at_100() {
        let fx = Fixture::new("search-cap");
        let body = "токен ".repeat(30);
        fx.write("2026-07-18_Без_мишени.md", &body);

        let entries = scan_digests(&fx.dir, "тест");
        let hits = search_digests(&entries, "токен", None, 10);
        assert_eq!(hits.len(), 1);
        // 30 вхождений × 10 очков, но текстовый вклад ограничен сотней
        assert_eq!(hits[0].score, 100);
    }

    #[test]
    fn search_days_filter_keeps_boundary_date() {
        let fx = Fixture::new("search-days");
        fx.write("2026-07-18_Мишень_новая.md", "");
        fx.write("2026-07-11_Мишень_граница.md", ""); // ровно 7 дней до максимума
        fx.write("2026-07-10_Мишень_старая.md", "");

        let entries = scan_digests(&fx.dir, "тест");
        let hits = search_digests(&entries, "мишень", Some(7), 10);
        let titles: Vec<&str> = hits.iter().map(|h| h.entry.title.as_str()).collect();
        assert_eq!(titles.len(), 2);
        assert!(titles.contains(&"Мишень новая"));
        assert!(titles.contains(&"Мишень граница")); // граница включается
    }

    #[test]
    fn search_empty_query_yields_nothing() {
        let fx = Fixture::new("search-empty");
        fx.write("2026-07-18_Что_угодно.md", "любой текст");
        let entries = scan_digests(&fx.dir, "тест");
        assert!(search_digests(&entries, "", None, 10).is_empty());
        // слова короче двух символов отбрасываются → запрос пустеет
        assert!(search_digests(&entries, "x", None, 10).is_empty());
        assert!(search_digests(&entries, "  --  ", None, 10).is_empty());
    }

    #[test]
    fn snippet_takes_first_matching_line() {
        let fx = Fixture::new("snippet-line");
        fx.write(
            "2026-07-18_Пустой_заголовок.md",
            "первая строка без совпадений\nвторая строка про агентов\nтретья тоже про агентов",
        );
        let entries = scan_digests(&fx.dir, "тест");
        let hits = search_digests(&entries, "агентов", None, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "вторая строка про агентов");
    }

    #[test]
    fn snippet_truncates_long_line_to_200_chars() {
        let fx = Fixture::new("snippet-long");
        let long_line = format!("агенты {}", "о".repeat(400));
        fx.write("2026-07-18_Длинный.md", &long_line);

        let entries = scan_digests(&fx.dir, "тест");
        let hits = search_digests(&entries, "агенты", None, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet.chars().count(), SNIPPET_MAX_CHARS);
        assert!(hits[0].snippet.starts_with("агенты"));
    }

    #[test]
    fn snippet_is_empty_when_only_title_matches() {
        let fx = Fixture::new("snippet-title");
        fx.write("2026-07-18_Агенты_повсюду.md", "текст без мишени");

        let entries = scan_digests(&fx.dir, "тест");
        let hits = search_digests(&entries, "агенты", None, 10);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.is_empty());
    }

    #[test]
    fn read_digest_truncates_by_chars_not_bytes() {
        let fx = Fixture::new("read-unicode");
        let path = fx.write("2026-07-18_Юникод.md", "абвгд");
        // кириллица — 2 байта на символ; обрезка по chars не рвёт кодпоинт
        let cut = read_digest(&path, 3).unwrap();
        assert_eq!(cut, "абв");
        assert_eq!(cut.len(), 6);
        let full = read_digest(&path, 100).unwrap();
        assert_eq!(full, "абвгд");
        assert!(read_digest(&fx.dir.join("нет_такого.md"), 10).is_err());
    }

    #[test]
    fn civil_shift_handles_months_leap_years_and_epoch() {
        assert_eq!(shift_date("2026-07-18", -7).unwrap(), "2026-07-11");
        assert_eq!(shift_date("2026-03-01", -1).unwrap(), "2026-02-28");
        assert_eq!(shift_date("2024-03-01", -1).unwrap(), "2024-02-29"); // високосный
        assert_eq!(shift_date("2026-01-01", -1).unwrap(), "2025-12-31");
        assert_eq!(shift_date("1970-01-01", 0).unwrap(), "1970-01-01");
        assert_eq!(shift_date("2026-02-30", 1), None); // невалидный вход
        // roundtrip: дата → дни → дата
        for date in ["1999-12-31", "2000-02-29", "2026-07-19"] {
            let (y, m, d) = parse_iso_date(date).unwrap();
            let (y2, m2, d2) = civil_from_days(days_from_civil(y, m, d));
            assert_eq!((y, m, d), (y2, m2, d2), "roundtrip для {date}");
        }
    }

    #[test]
    fn load_collections_fills_defaults_and_label_alias() {
        let fx = Fixture::new("hf-load");
        let json = r#"{
            "acme/models-abc": {
                "slug": "acme/models-abc",
                "title": "Acme Models",
                "upvotes": 42,
                "provider_key": "acme",
                "provider_label": "Acme Corp"
            },
            "empty/entry": {}
        }"#;
        let path = fx.write("collections_data.json", json);

        let cols = load_collections(&path).unwrap();
        assert_eq!(cols.len(), 2);
        let full = cols.iter().find(|c| c.slug == "acme/models-abc").unwrap();
        assert_eq!(full.title, "Acme Models");
        assert_eq!(full.upvotes, 42);
        assert_eq!(full.provider_name, "Acme Corp"); // алиас provider_label
        let empty = cols.iter().find(|c| c.slug == "empty/entry").unwrap();
        assert!(empty.title.is_empty());
        assert_eq!(empty.item_count, 0); // slug «empty/entry» подставлен из ключа dict
    }

    #[test]
    fn search_collections_ranks_title_theme_then_text() {
        let cols = vec![
            col("Robotics stack", 1),
            HfCollection { theme: "robotics".to_owned(), ..col("", 2) },
            HfCollection { description: "robotics robotics".to_owned(), ..col("", 3) },
        ];
        let hits = search_collections(&cols, "robotics", None, 10);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].title, "Robotics stack"); // 50 за заголовок
        assert_eq!(hits[1].upvotes, 2); // 30 за тему
        assert_eq!(hits[2].upvotes, 3); // 20 за два вхождения в description
    }

    #[test]
    fn search_collections_breaks_ties_by_upvotes_then_title() {
        let cols = vec![col("vision beta", 3), col("vision alpha", 3), col("vision gamma", 9)];
        let hits = search_collections(&cols, "vision", None, 10);
        assert_eq!(hits[0].title, "vision gamma"); // равные очки → upvotes ↓
        assert_eq!(hits[1].title, "vision alpha"); // равные upvotes → title ↑
        let limited = search_collections(&cols, "vision", None, 2);
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn search_collections_filters_provider_case_insensitive() {
        let cols = vec![
            HfCollection { provider_key: "DeepSeek-AI".to_owned(), ..col("agents hub", 0) },
            HfCollection { provider_key: "qwen".to_owned(), ..col("agents den", 0) },
        ];
        let hits = search_collections(&cols, "agents", Some("deepseek"), 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provider_key, "DeepSeek-AI");
        assert_eq!(search_collections(&cols, "agents", None, 10).len(), 2);
        // пустой запрос — пустой ответ даже с provider-фильтром
        assert!(search_collections(&cols, "", Some("deepseek"), 10).is_empty());
        assert!(search_collections(&cols, "agents", Some("anthropic"), 10).is_empty());
    }

    /// Мягкий тест на реальных корнях: пропускается, если библиотеки нет.
    #[test]
    fn real_roots_soft_check() {
        let news = Path::new(NEWS_ROOT);
        if news.is_dir() {
            let entries = scan_digests(news, "новости");
            assert!(!entries.is_empty(), "ожидались дайджесты в {NEWS_ROOT}");
            assert!(entries.windows(2).all(|w| w[0].date >= w[1].date));
            assert!(entries.iter().any(|e| e.source != "новости"));
        }
        let hf = Path::new(HF_ROOT);
        if hf.is_dir() {
            assert!(!scan_digests(hf, "HF").is_empty(), "ожидались дайджесты в {HF_ROOT}");
        }
        let json = hf.join("collections_data.json");
        if json.is_file() {
            let cols = load_collections(&json).unwrap();
            assert!(cols.len() > 100, "ожидалась сотня+ коллекций");
            assert!(cols.iter().all(|c| !c.slug.is_empty()));
            assert!(cols.iter().any(|c| !c.provider_name.is_empty()));
        }
    }
}
