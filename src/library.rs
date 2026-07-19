//! Индекс ML-библиотеки `recipes_taxonomy` (~63 ГБ разобранных источников):
//! ветки таксономии, PDF arXiv с txt-зеркалами вида `file.pdf.txt`, docx и
//! `taxonomy_mapping.json` вида `{ "ветка/подветка": [keywords] }`.
//!
//! [`LibraryIndex::load`] читает маппинг (если файл есть) и сканирует
//! top-level каталоги корня: они становятся ветками даже без записи в
//! маппинге и наследуют keywords своих подветок. Поверх индекса — два
//! поиска и чтение выдержек:
//!
//! * [`LibraryIndex::search_branches`] ранжирует ветки по пересечению слов
//!   запроса с keywords ветки (слова короче двух букв и чисто цифровые
//!   токены отбрасываются);
//! * [`LibraryIndex::search_docs`] обходит txt-зеркала: слово запроса в
//!   имени файла — [`SCORE_FILENAME`] очков, каждое вхождение в первые
//!   [`TEXT_WINDOW`] байт текста — [`SCORE_TEXT_PER_MATCH`]; обход
//!   ограничен [`MAX_WALK_FILES`] файлами с ранним выходом по
//!   `limit * `[`HIT_EARLY_EXIT_FACTOR`] находок;
//! * [`LibraryIndex::read_excerpt`] читает txt-зеркало (для `file.pdf`
//!   подставляется `file.pdf.txt`), вежливо отказывает для PDF без зеркала
//!   и для docx, а `..` и симлинки наружу корня режет проверкой пути.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Корень боевой библиотеки на машине пользователя.
pub const DEFAULT_ROOT: &str =
    "/home/roman/Документы/КОД/gigachat/РАЗБОРЫ/recipes_taxonomy";

/// Имя файла маппинга «ветка → keywords» в корне библиотеки.
const MAPPING_FILE: &str = "taxonomy_mapping.json";

/// Сколько байт от начала зеркала читается для скоринга и сниппета.
const TEXT_WINDOW: usize = 4 * 1024;

/// Максимум txt-файлов, просматриваемых за один проход поиска документов.
const MAX_WALK_FILES: usize = 30_000;

/// Множитель раннего выхода: обход прекращается после `limit * FACTOR` находок.
const HIT_EARLY_EXIT_FACTOR: usize = 50;

/// Очки за каждое слово запроса, нашедшееся в имени файла.
const SCORE_FILENAME: u64 = 50;

/// Очки за каждое вхождение слова запроса в голову текста ([`TEXT_WINDOW`]).
const SCORE_TEXT_PER_MATCH: u64 = 10;

/// Максимальная длина сниппета в символах.
const SNIPPET_MAX_CHARS: usize = 200;

/// Ветка таксономии: ключ и keywords в нижнем регистре.
#[derive(Debug)]
struct Branch {
    /// Ключ ветки: имя top-level каталога или «верх/подветка» из маппинга.
    key: String,
    /// Keywords из `taxonomy_mapping.json` плюс токены имён каталогов.
    keywords: Vec<String>,
}

/// Одна находка [`LibraryIndex::search_docs`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocHit {
    /// Путь к txt-зеркалу относительно корня библиотеки.
    pub path: PathBuf,
    /// Ветка — родительский каталог относительно корня
    /// («03_ПОСТ-ТРЕЙНИНГ_RL/02_RL_методы»; пустая строка для файлов в корне).
    pub branch: String,
    /// Итоговый скор: [`SCORE_FILENAME`] за слово в имени файла плюс
    /// [`SCORE_TEXT_PER_MATCH`] за каждое вхождение в голову текста.
    pub score: u64,
    /// Первая строка с совпадением (до [`SNIPPET_MAX_CHARS`] символов);
    /// если совпадение только в имени файла — первая непустая строка.
    pub snippet: String,
}

/// Индекс библиотеки: маппинг веток плюс снимок top-level каталогов.
///
/// Индекс не хранит ссылок на файловую систему: все пути документов
/// вычисляются на лету от [`LibraryIndex::root`].
#[derive(Debug)]
pub struct LibraryIndex {
    /// Канонический корень библиотеки.
    root: PathBuf,
    /// Ветки из маппинга и/или со сканированного верхнего уровня,
    /// отсортированные по ключу.
    branches: Vec<Branch>,
}

impl LibraryIndex {
    /// Загружает индекс из корня `root`:
    ///
    /// * читает `taxonomy_mapping.json`, если файл есть (битый JSON —
    ///   ошибка, отсутствующий — допустим);
    /// * сканирует top-level каталоги — каждый становится веткой и
    ///   наследует keywords своих подветок из маппинга.
    ///
    /// Ошибка — только если корень недоступен/не каталог или маппинг
    /// не парсится.
    pub fn load(root: &Path) -> Result<Self> {
        let canon = root
            .canonicalize()
            .with_context(|| format!("корень библиотеки недоступен: {}", root.display()))?;
        if !canon.is_dir() {
            bail!("корень библиотеки не является каталогом: {}", canon.display());
        }

        let mut branches: Vec<Branch> = Vec::new();
        let mapping_path = canon.join(MAPPING_FILE);
        if mapping_path.is_file() {
            let raw = fs::read_to_string(&mapping_path).with_context(|| {
                format!("не читается {MAPPING_FILE}: {}", mapping_path.display())
            })?;
            let map: std::collections::HashMap<String, Vec<String>> =
                serde_json::from_str(&raw)
                    .with_context(|| format!("битый JSON в {}", mapping_path.display()))?;
            for (key, kws) in map {
                let mut keywords: Vec<String> =
                    kws.iter().map(|k| k.to_lowercase()).collect();
                for tok in tokenize(&key) {
                    if !keywords.contains(&tok) {
                        keywords.push(tok);
                    }
                }
                branches.push(Branch { key, keywords });
            }
        }

        let mut top_dirs: Vec<String> = Vec::new();
        if let Ok(entries) = fs::read_dir(&canon) {
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if !ft.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with('.') {
                    top_dirs.push(name);
                }
            }
        }
        top_dirs.sort();
        for name in top_dirs {
            if branches.iter().any(|b| b.key == name) {
                continue;
            }
            let prefix = format!("{name}/");
            let mut keywords = tokenize(&name);
            for b in &branches {
                if b.key.starts_with(&prefix) {
                    for kw in &b.keywords {
                        if !keywords.contains(kw) {
                            keywords.push(kw.clone());
                        }
                    }
                }
            }
            branches.push(Branch { key: name, keywords });
        }
        branches.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(Self {
            root: canon,
            branches,
        })
    }

    /// Канонический корень библиотеки.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Число известных веток (маппинг плюс top-level каталоги).
    pub fn branch_count(&self) -> usize {
        self.branches.len()
    }

    /// Ранжирует ветки по пересечению слов запроса с keywords ветки.
    ///
    /// Слово засчитывается, если оно совпало с keyword'ом целиком или вошло
    /// в него подстрокой (и наоборот) — так «benchmark» ловит
    /// «benchmarking», а «world» — двухсловный «world model». Скор — число
    /// совпавших слов. Возвращает пары `(ключ ветки, скор)` по убыванию
    /// скора, при равенстве — по ключу; ветки с нулевым скором
    /// отбрасываются.
    pub fn search_branches(&self, query: &str) -> Vec<(String, u64)> {
        let words = tokenize(query);
        let mut scored: Vec<(String, u64)> = self
            .branches
            .iter()
            .filter_map(|b| {
                let score = words
                    .iter()
                    .filter(|w| b.keywords.iter().any(|k| word_matches(w, k)))
                    .count() as u64;
                (score > 0).then(|| (b.key.clone(), score))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored
    }

    /// Ищет по txt-зеркалам (`file.pdf.txt` и прочие `*.txt`).
    ///
    /// Скор файла: [`SCORE_FILENAME`] за каждое слово запроса в имени плюс
    /// [`SCORE_TEXT_PER_MATCH`] за каждое вхождение слова в первые
    /// [`TEXT_WINDOW`] байт текста; файлы без совпадений отбрасываются.
    /// Обход — не более [`MAX_WALK_FILES`] зеркал за проход, с ранним
    /// выходом после `limit * `[`HIT_EARLY_EXIT_FACTOR`] находок (дальше
    /// сортировка по скору и отрезание до `limit`). Скрытые каталоги и
    /// симлинки не посещаются. Пустой запрос или `limit == 0` — пустой
    /// результат.
    pub fn search_docs(&self, query: &str, limit: usize) -> Vec<DocHit> {
        let words = tokenize(query);
        if words.is_empty() || limit == 0 {
            return Vec::new();
        }
        let max_hits = limit.saturating_mul(HIT_EARLY_EXIT_FACTOR);
        let mut hits: Vec<DocHit> = Vec::new();
        let mut examined = 0usize;
        let mut stack = vec![self.root.clone()];
        'walk: while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_symlink() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let path = entry.path();
                if ft.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !ft.is_file() || !name.to_lowercase().ends_with(".txt") {
                    continue;
                }
                examined += 1;
                if examined > MAX_WALK_FILES {
                    break 'walk;
                }
                if let Some(hit) = self.score_doc(&path, &words) {
                    hits.push(hit);
                    if hits.len() >= max_hits {
                        break 'walk;
                    }
                }
            }
        }
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
        hits.truncate(limit);
        hits
    }

    /// Читает excerpt из txt-зеркала, не более `max_chars` символов.
    ///
    /// * `path` — относительный (от корня) или абсолютный; обязан указывать
    ///   внутрь корня: проверка лексическая (после раскрытия `.`/`..`) и
    ///   повторная после canonicalize — симлинк наружу не пройдёт.
    /// * Для `file.pdf` читается зеркало `file.pdf.txt`; нет зеркала —
    ///   вежливая ошибка с подсказкой про `pdftotext`.
    /// * `docx` не читается в принципе — ошибка «используйте txt».
    pub fn read_excerpt(&self, path: &Path, max_chars: usize) -> Result<String> {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let normalized = normalize_lexically(&joined);
        if !normalized.starts_with(&self.root) {
            bail!(
                "отказано: путь выходит за пределы корня библиотеки: {}",
                path.display()
            );
        }
        let ext = normalized
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_lowercase)
            .unwrap_or_default();
        let target = match ext.as_str() {
            "txt" => normalized,
            "pdf" => {
                let mut mirror = normalized.into_os_string();
                mirror.push(".txt");
                let mirror = PathBuf::from(mirror);
                if !mirror.is_file() {
                    bail!(
                        "PDF нельзя читать напрямую, а txt-зеркало не найдено: {}. \
                         Создайте зеркало (`pdftotext file.pdf file.pdf.txt`) и повторите.",
                        path.display()
                    );
                }
                mirror
            }
            "docx" => bail!("docx не читается, используйте txt: {}", path.display()),
            other => bail!(
                "неподдерживаемое расширение «{other}» ({}): читаются только .txt зеркала",
                path.display()
            ),
        };
        let canon = target
            .canonicalize()
            .with_context(|| format!("файл не найден: {}", target.display()))?;
        if !canon.starts_with(&self.root) {
            bail!(
                "отказано: путь указывает наружу корня библиотеки (симлинк): {}",
                path.display()
            );
        }
        let bytes =
            fs::read(&canon).with_context(|| format!("не читается файл: {}", canon.display()))?;
        let text = String::from_utf8_lossy(&bytes);
        Ok(text.chars().take(max_chars).collect())
    }

    /// Скорит одно зеркало; `None` — если совпадений нет совсем.
    fn score_doc(&self, path: &Path, words: &[String]) -> Option<DocHit> {
        let name = path.file_name()?.to_string_lossy().to_lowercase();
        let fname_hits = words
            .iter()
            .filter(|w| name.contains(w.as_str()))
            .count() as u64;
        // Нечитаемый файл не мешает поиску: имя всё равно может дать очки.
        let head = read_head(path).unwrap_or_default();
        let lower = head.to_lowercase();
        let text_hits: u64 = words
            .iter()
            .map(|w| lower.matches(w.as_str()).count() as u64)
            .sum();
        let score = fname_hits * SCORE_FILENAME + text_hits * SCORE_TEXT_PER_MATCH;
        if score == 0 {
            return None;
        }
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        let branch = rel
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(DocHit {
            path: rel.to_path_buf(),
            branch,
            score,
            snippet: make_snippet(&head, words),
        })
    }
}

/// Разбивает текст на слова-токены: lowercase, минимум два символа и хотя
/// бы одна буква (чисто цифровые вроде «01» отбрасываются), без повторов,
/// порядок сохраняется.
fn tokenize(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        let word = raw.to_lowercase();
        if word.chars().count() >= 2
            && word.chars().any(char::is_alphabetic)
            && !out.contains(&word)
        {
            out.push(word);
        }
    }
    out
}

/// Слово против keyword'а: точное совпадение или подстрока в любую сторону.
fn word_matches(word: &str, keyword: &str) -> bool {
    word == keyword || word.contains(keyword) || keyword.contains(word)
}

/// Читает первые [`TEXT_WINDOW`] байт файла как lossy-текст.
fn read_head(path: &Path) -> std::io::Result<String> {
    let file = File::open(path)?;
    let mut buf = Vec::new();
    file.take(TEXT_WINDOW as u64).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Сниппет: первая строка со словом запроса (иначе — первая непустая),
/// обрезанная до [`SNIPPET_MAX_CHARS`] символов.
fn make_snippet(head: &str, words: &[String]) -> String {
    let mut first_nonempty = "";
    for line in head.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if first_nonempty.is_empty() {
            first_nonempty = trimmed;
        }
        let lower = trimmed.to_lowercase();
        if words.iter().any(|w| lower.contains(w.as_str())) {
            return trimmed.chars().take(SNIPPET_MAX_CHARS).collect();
        }
    }
    first_nonempty.chars().take(SNIPPET_MAX_CHARS).collect()
}

/// Лексическая нормализация пути (раскрытие `.` и `..`) без обращения к ФС.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Временная «библиотека» в системном temp-каталоге; удаляется в `Drop`.
    struct TempLib(PathBuf);

    impl TempLib {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("theseus_library_test_{pid}_{n}"));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// Пишет файл `rel` внутри корня, создавая родительские каталоги.
        fn write(&self, rel: &str, content: &str) -> PathBuf {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
            path
        }
    }

    impl Drop for TempLib {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Индекс поверх фикстуры: маппинг на две подветки и три top-level ветки.
    fn make_index() -> (TempLib, LibraryIndex) {
        let tmp = TempLib::new();
        tmp.write(
            MAPPING_FILE,
            r#"{
                "01_СРЕДЫ/01_Масштабирование": ["scaling", "synthesis", "world model"],
                "03_RL/02_GRPO": ["grpo", "reinforcement", "reward", "policy"]
            }"#,
        );
        fs::create_dir_all(tmp.path().join("01_СРЕДЫ/01_Масштабирование")).unwrap();
        fs::create_dir_all(tmp.path().join("03_RL/02_GRPO")).unwrap();
        fs::create_dir_all(tmp.path().join("07_ПОСОБИЯ")).unwrap();
        let idx = LibraryIndex::load(tmp.path()).unwrap();
        (tmp, idx)
    }

    #[test]
    fn load_reads_mapping_and_scans_top_dirs() {
        let (_tmp, idx) = make_index();
        // 2 ключа маппинга + 3 top-level каталога.
        assert_eq!(idx.branch_count(), 5);
        assert!(idx.root().is_absolute());
    }

    #[test]
    fn load_fails_on_missing_root() {
        let res = LibraryIndex::load(Path::new("/theseus_no_such_library_root_42"));
        assert!(res.is_err());
    }

    #[test]
    fn load_works_without_mapping_file() {
        let tmp = TempLib::new();
        fs::create_dir_all(tmp.path().join("01_АЛФА")).unwrap();
        let idx = LibraryIndex::load(tmp.path()).unwrap();
        assert_eq!(idx.branch_count(), 1);
        // Ветка без маппинга ищется по токенам имени каталога.
        let hits = idx.search_branches("алфа");
        assert_eq!(hits, vec![("01_АЛФА".to_string(), 1)]);
    }

    #[test]
    fn load_fails_on_corrupt_mapping() {
        let tmp = TempLib::new();
        tmp.write(MAPPING_FILE, "{ это не json");
        let err = LibraryIndex::load(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("битый JSON"), "{err}");
    }

    #[test]
    fn search_branches_ranks_by_keyword_overlap() {
        let (_tmp, idx) = make_index();
        let hits = idx.search_branches("grpo reward scaling");
        // «03_RL» наследует keywords подветки, потому делит скор с ней;
        // при равном скоре сортировка по ключу.
        assert_eq!(
            hits,
            vec![
                ("03_RL".to_string(), 2),
                ("03_RL/02_GRPO".to_string(), 2),
                ("01_СРЕДЫ".to_string(), 1),
                ("01_СРЕДЫ/01_Масштабирование".to_string(), 1),
            ]
        );
    }

    #[test]
    fn search_branches_matches_multiword_keyword_and_dir_name() {
        let (_tmp, idx) = make_index();
        // «world» и «model» обе покрываются двухсловным keyword «world model».
        let hits = idx.search_branches("world model");
        assert_eq!(hits[0].1, 2);
        assert!(hits.iter().any(|(k, _)| k == "01_СРЕДЫ/01_Масштабирование"));
        // Ветка вне маппинга — по токену имени («07_ПОСОБИЯ» → «пособия»).
        let hits = idx.search_branches("пособия");
        assert_eq!(hits, vec![("07_ПОСОБИЯ".to_string(), 1)]);
    }

    #[test]
    fn search_branches_empty_query_and_no_match() {
        let (_tmp, idx) = make_index();
        assert!(idx.search_branches("").is_empty());
        assert!(idx.search_branches("  ...  ").is_empty());
        assert!(idx.search_branches("zzzqqq").is_empty());
    }

    #[test]
    fn search_docs_filename_match_outranks_text_occurrences() {
        let (tmp, idx) = make_index();
        tmp.write("03_RL/02_GRPO/grpo_tricks.pdf.txt", "текст без нужных слов");
        tmp.write("03_RL/02_GRPO/notes.txt", "grpo grpo и ещё раз grpo");
        let hits = idx.search_docs("grpo", 10);
        assert_eq!(hits.len(), 2);
        // Имя файла (50) бьёт три вхождения в текст (30).
        assert_eq!(
            hits[0].path,
            PathBuf::from("03_RL/02_GRPO/grpo_tricks.pdf.txt")
        );
        assert_eq!(hits[0].score, SCORE_FILENAME);
        assert_eq!(hits[1].path, PathBuf::from("03_RL/02_GRPO/notes.txt"));
        assert_eq!(hits[1].score, 3 * SCORE_TEXT_PER_MATCH);
    }

    #[test]
    fn search_docs_text_score_scales_with_occurrences() {
        let (tmp, idx) = make_index();
        tmp.write("03_RL/a.txt", "rl rl rl");
        tmp.write("03_RL/b.txt", "rl");
        let hits = idx.search_docs("rl", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, PathBuf::from("03_RL/a.txt"));
        assert_eq!(hits[0].score, 30);
        assert_eq!(hits[1].path, PathBuf::from("03_RL/b.txt"));
        assert_eq!(hits[1].score, 10);
    }

    #[test]
    fn search_docs_respects_limit_and_breaks_ties_by_path() {
        let (tmp, idx) = make_index();
        for i in 0..4 {
            tmp.write(&format!("03_RL/doc{i}.txt"), "grpo");
        }
        let hits = idx.search_docs("grpo", 2);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, PathBuf::from("03_RL/doc0.txt"));
        assert_eq!(hits[1].path, PathBuf::from("03_RL/doc1.txt"));
    }

    #[test]
    fn search_docs_snippet_first_matching_line_capped() {
        let (tmp, idx) = make_index();
        tmp.write(
            "03_RL/multi.pdf.txt",
            "первая строка без слов\nGRPO вторая строка\nтретья строка grpo",
        );
        let long_line = format!("grpo {}", "д".repeat(500));
        tmp.write("03_RL/long.pdf.txt", &long_line);
        let hits = idx.search_docs("grpo", 5);
        assert_eq!(hits.len(), 2);
        // Сниппет — первая строка с совпадением, в исходном регистре.
        assert_eq!(hits[0].snippet, "GRPO вторая строка");
        let long = hits
            .iter()
            .find(|h| h.path == Path::new("03_RL/long.pdf.txt"))
            .unwrap();
        // Длинная строка обрезается до 200 символов.
        assert_eq!(long.snippet.chars().count(), SNIPPET_MAX_CHARS);
        assert!(long.snippet.starts_with("grpo"));
    }

    #[test]
    fn search_docs_snippet_falls_back_to_first_line_when_only_name_matches() {
        let (tmp, idx) = make_index();
        tmp.write("03_RL/grpo_named.pdf.txt", "\n\nпервая непустая\nвторая");
        let hits = idx.search_docs("grpo", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].score, SCORE_FILENAME);
        assert_eq!(hits[0].snippet, "первая непустая");
    }

    #[test]
    fn search_docs_ignores_non_txt_hidden_and_empty_inputs() {
        let (tmp, idx) = make_index();
        tmp.write("03_RL/grpo_raw.pdf", "%PDF-1.4 бинарь");
        tmp.write("03_RL/grpo_doc.docx", "docx содержимое");
        tmp.write(".hidden/grpo.pdf.txt", "grpo в скрытом каталоге");
        assert!(idx.search_docs("grpo", 10).is_empty());
        assert!(idx.search_docs("", 10).is_empty());
        assert!(idx.search_docs("grpo", 0).is_empty());
    }

    #[test]
    fn search_docs_branch_is_relative_parent_dir() {
        let (tmp, idx) = make_index();
        tmp.write("root_note.txt", "grpo в корне");
        tmp.write("03_RL/02_GRPO/deep.pdf.txt", "grpo вглубине");
        let hits = idx.search_docs("grpo", 10);
        assert_eq!(hits.len(), 2);
        let root_hit = hits
            .iter()
            .find(|h| h.path == Path::new("root_note.txt"))
            .unwrap();
        assert_eq!(root_hit.branch, "");
        let deep = hits
            .iter()
            .find(|h| h.path == Path::new("03_RL/02_GRPO/deep.pdf.txt"))
            .unwrap();
        assert_eq!(deep.branch, "03_RL/02_GRPO");
    }

    #[test]
    fn read_excerpt_reads_txt_relative_and_absolute() {
        let (tmp, idx) = make_index();
        let abs = tmp.write("03_RL/note.txt", "привет, мир grpo");
        assert_eq!(
            idx.read_excerpt(Path::new("03_RL/note.txt"), 100).unwrap(),
            "привет, мир grpo"
        );
        let abs = abs.canonicalize().unwrap();
        assert_eq!(idx.read_excerpt(&abs, 100).unwrap(), "привет, мир grpo");
    }

    #[test]
    fn read_excerpt_reads_pdf_through_mirror_and_truncates_chars() {
        let (tmp, idx) = make_index();
        let content = "Я".repeat(50);
        tmp.write("03_RL/paper.pdf.txt", &content);
        // Дан .pdf — читается зеркало .pdf.txt.
        let got = idx.read_excerpt(Path::new("03_RL/paper.pdf"), 10).unwrap();
        assert_eq!(got, "Я".repeat(10));
        // Многобайтовые символы обрезаются по chars, не по байтам.
        assert_eq!(got.chars().count(), 10);
    }

    #[test]
    fn read_excerpt_pdf_without_mirror_and_docx_errors() {
        let (tmp, idx) = make_index();
        let err = idx
            .read_excerpt(Path::new("03_RL/ghost.pdf"), 100)
            .unwrap_err();
        assert!(err.to_string().contains("txt-зеркало не найдено"), "{err}");
        tmp.write("03_RL/report.docx", "фейковый docx");
        let err = idx
            .read_excerpt(Path::new("03_RL/report.docx"), 100)
            .unwrap_err();
        assert!(err.to_string().contains("docx не читается"), "{err}");
        let err = idx
            .read_excerpt(Path::new("03_RL/absent.txt"), 100)
            .unwrap_err();
        assert!(err.to_string().contains("не найден"), "{err}");
    }

    #[test]
    fn read_excerpt_rejects_path_traversal() {
        let (tmp, idx) = make_index();
        let pid = std::process::id();
        let outside = std::env::temp_dir().join(format!("theseus_library_outside_{pid}.txt"));
        fs::write(&outside, "данные снаружи корня").unwrap();
        let name = outside.file_name().unwrap().to_string_lossy().into_owned();
        // Файл существует, но лежит вне корня: `..` — отказ.
        let rel = format!("../{name}");
        let err = idx.read_excerpt(Path::new(&rel), 100).unwrap_err();
        assert!(err.to_string().contains("пределы корня"), "{err}");
        // Абсолютный путь наружу — тоже отказ.
        let err = idx.read_excerpt(&outside, 100).unwrap_err();
        assert!(err.to_string().contains("пределы корня"), "{err}");
        // Вложенный `..`, остающийся внутри корня, — легален.
        tmp.write("03_RL/inner.txt", "внутри");
        assert_eq!(
            idx.read_excerpt(Path::new("03_RL/../03_RL/inner.txt"), 100)
                .unwrap(),
            "внутри"
        );
        let _ = fs::remove_file(&outside);
    }

    /// Мягкий тест боевой библиотеки: пропускается, если её нет на машине.
    #[test]
    fn real_library_soft() {
        let root = Path::new(DEFAULT_ROOT);
        if !root.is_dir() {
            eprintln!("пропуск: библиотека недоступна: {DEFAULT_ROOT}");
            return;
        }
        let idx = LibraryIndex::load(root).unwrap();
        assert!(idx.branch_count() > 0);
        if root.join(MAPPING_FILE).is_file() {
            assert!(!idx.search_branches("benchmark").is_empty());
        }
        let hits = idx.search_docs("grpo", 3);
        assert!(hits.len() <= 3);
        assert!(hits.iter().all(|h| h.score > 0));
        // Найденные зеркала должны читаться через read_excerpt.
        for h in &hits {
            idx.read_excerpt(&h.path, 500).unwrap();
        }
    }
}
