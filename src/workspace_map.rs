//! Карта файлов workspace для explore-агента (образец — `file-search`
//! из codex-rs): дешёвый снапшот дерева проекта, по которому агент
//! ориентируется, прежде чем читать конкретные файлы.
//!
//! [`WsMap::scan`] обходит дерево от корня вглубь (файлы не глубже
//! [`MAX_DEPTH`] компонентов пути), пропуская служебные каталоги
//! ([`IGNORED_DIRS`]), симлинки и всё, что не является обычным файлом.
//! Для каждого файла запоминается относительный путь, размер, mtime и
//! класс [`WsKind`]:
//!
//! * [`WsKind::Hidden`] — файл скрыт: его имя или имя любого родительского
//!   каталога начинается с точки. Содержимое скрытых файлов не sniff'ается:
//!   показывать их агенту всё равно не планируется.
//! * [`WsKind::Binary`] — в первых [`SNIFF_LEN`] байтах встретился NUL
//!   (эвристика `grep -I`, как в `filetype.rs`).
//! * [`WsKind::Text`] — всё остальное, включая пустые и нечитаемые файлы:
//!   класс — лишь подсказка, реальное чтение случится позже и само даст
//!   настоящую ошибку ввода-вывода.
//!
//! Поверх карты — навигационные запросы: [`WsMap::filter_glob`],
//! [`WsMap::largest`], [`WsMap::freshest`], [`WsMap::find_substring`]
//! и человеко-читаемая сводка [`WsMap::summary`].
//!
//! ## Глоб-синтаксис [`WsMap::filter_glob`]
//!
//! * `**` как целый компонент — любое число вложенных каталогов, включая
//!   ноль (`src/**/*.rs` найдёт и `src/main.rs`);
//! * `*` внутри компонента — любая последовательность символов, но не
//!   пересекающая `/`;
//! * `?` — ровно один Unicode-символ;
//! * остальные символы — литералы; сравнение регистрозависимое;
//! * паттерн без `/` матчится с basename файла на любой глубине
//!   (`*.rs` найдёт `src/main.rs`).
//!
//! Сканирование best-effort и никогда не паникует: нечитаемые каталоги
//! молча пропускаются, неудачный `stat`/mtime даёт нули, несуществующий
//! корень — пустую карту.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Максимальная глубина файла от корня карты в компонентах пути:
/// `a/b/c.txt` — глубина 3. Более глубокие файлы в карту не попадают.
pub const MAX_DEPTH: usize = 12;

/// Имена каталогов, которые сканер обходит стороной на любой глубине:
/// VCS-хранилище, сборочный таргет и служебные каталоги самого харнесса.
/// Одноимённые обычные файлы не затрагиваются.
pub const IGNORED_DIRS: &[&str] = &[".git", "target", ".theseus", "sessions"];

/// Сколько байт читаем от начала файла для NUL-эвристики бинарности.
const SNIFF_LEN: usize = 8 * 1024;

/// Сколько языковых групп показывает [`WsMap::summary`].
const SUMMARY_TOP: usize = 10;

/// Метка группы для файлов без расширения в [`WsMap::summary`].
const NO_EXT_GROUP: &str = "(без расширения)";

/// Класс файла в карте workspace (семантика — в документации модуля).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WsKind {
    /// Обычный текстовый файл — можно читать и показывать агенту.
    Text,
    /// Бинарный файл (NUL-байт в голове) — в промпт не вставлять.
    Binary,
    /// Скрытый файл (dotfile или внутри dot-каталога) — не показывать
    /// без явного запроса.
    Hidden,
}

/// Одна запись карты: файл и его дешёвые метаданные.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WsEntry {
    /// Путь относительно корня карты (`src/main.rs`).
    pub path: PathBuf,
    /// Размер файла в байтах.
    pub bytes: u64,
    /// Время модификации, секунды с UNIX_EPOCH; 0, если узнать не удалось.
    pub mtime_secs: u64,
    /// Класс файла.
    pub kind: WsKind,
}

/// Снапшот файлового дерева workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WsMap {
    /// Корень сканирования в том виде, как его передали в [`WsMap::scan`].
    pub root: PathBuf,
    /// Файлы в порядке детерминированного обхода: каталоги сортируются
    /// по имени, обход — в глубину.
    pub entries: Vec<WsEntry>,
}

impl WsMap {
    /// Отсканировать дерево под `root` и собрать карту, не более
    /// `max_entries` файлов. Потолок нужен, чтобы гигантский репозиторий
    /// не раздул снапшот: отсекается хвост детерминированного обхода.
    ///
    /// Никогда не паникует: любые ошибки ввода-вывода дают молчаливый
    /// пропуск узла, несуществующий корень — пустую карту.
    pub fn scan(root: &Path, max_entries: usize) -> Self {
        let mut entries = Vec::new();
        if max_entries > 0 {
            walk(root, Path::new(""), 0, max_entries, &mut entries);
        }
        Self {
            root: root.to_path_buf(),
            entries,
        }
    }

    /// Отобрать записи по глоб-паттерну (синтаксис — в документации
    /// модуля). Порядок совпадений — порядок обхода карты.
    ///
    /// Пустой паттерн (или состоящий из одних разделителей) не матчит
    /// ничего. Для регистронезависимого поиска — [`WsMap::find_substring`].
    pub fn filter_glob(&self, pattern: &str) -> Vec<&WsEntry> {
        let parts = split_pattern(pattern);
        if parts.is_empty() {
            return Vec::new();
        }
        if !pattern.contains('/') {
            // Паттерн без разделителей — по basename на любой глубине.
            let pat = parts[0];
            return self
                .entries
                .iter()
                .filter(|e| {
                    e.path
                        .file_name()
                        .is_some_and(|n| component_match(pat, &n.to_string_lossy()))
                })
                .collect();
        }
        self.entries
            .iter()
            .filter(|e| {
                let comps = path_components(&e.path);
                let refs: Vec<&str> = comps.iter().map(String::as_str).collect();
                glob_match(&parts, &refs)
            })
            .collect()
    }

    /// `n` самых больших файлов по убыванию размера; при равенстве —
    /// по пути (детерминизм для сравнения снапшотов).
    pub fn largest(&self, n: usize) -> Vec<&WsEntry> {
        let mut ranked: Vec<&WsEntry> = self.entries.iter().collect();
        ranked.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.path.cmp(&b.path)));
        ranked.truncate(n);
        ranked
    }

    /// `n` самых свежих файлов по убыванию mtime; при равенстве — по пути.
    pub fn freshest(&self, n: usize) -> Vec<&WsEntry> {
        let mut ranked: Vec<&WsEntry> = self.entries.iter().collect();
        ranked.sort_by(|a, b| b.mtime_secs.cmp(&a.mtime_secs).then(a.path.cmp(&b.path)));
        ranked.truncate(n);
        ranked
    }

    /// Найти файлы, чей относительный путь (со слэшами) содержит `needle`.
    /// Сравнение регистронезависимое и Unicode-aware (`to_lowercase`),
    /// поэтому ищется и по именам каталогов, и по кириллице.
    ///
    /// Пустой `needle` возвращает всю карту — семантика `str::contains`.
    pub fn find_substring(&self, needle: &str) -> Vec<&WsEntry> {
        let needle = needle.to_lowercase();
        self.entries
            .iter()
            .filter(|e| rel_slash(&e.path).to_lowercase().contains(&needle))
            .collect()
    }

    /// Человеко-читаемая сводка карты: заголовок с числом файлов и байт,
    /// затем топ-[`SUMMARY_TOP`] языковых групп по расширениям
    /// (сортировка по суммарному размеру группы, колонки выровнены).
    pub fn summary(&self) -> String {
        let mut text = 0usize;
        let mut binary = 0usize;
        let mut hidden = 0usize;
        let mut total_bytes = 0u64;
        let mut groups: BTreeMap<String, GroupStat> = BTreeMap::new();
        for e in &self.entries {
            match e.kind {
                WsKind::Text => text += 1,
                WsKind::Binary => binary += 1,
                WsKind::Hidden => hidden += 1,
            }
            total_bytes += e.bytes;
            let stat = groups.entry(group_key(&e.path)).or_default();
            stat.count += 1;
            stat.bytes += e.bytes;
        }

        let count = self.entries.len() as u64;
        let total_word = plural(total_bytes, "байт", "байта", "байтов");
        let root = self.root.display();
        let mut out = format!(
            "workspace: {root}\nфайлов: {count}, всего {total_bytes} {total_word} (текстовых: {text}, бинарных: {binary}, скрытых: {hidden})\n"
        );
        if groups.is_empty() {
            return out;
        }
        let total_groups = groups.len();
        let mut ranked: Vec<(String, GroupStat)> = groups.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.bytes
                .cmp(&a.1.bytes)
                .then(b.1.count.cmp(&a.1.count))
                .then(a.0.cmp(&b.0))
        });
        ranked.truncate(SUMMARY_TOP);
        out.push_str(&format!(
            "языковые группы по расширениям (топ-{SUMMARY_TOP} из {total_groups}):\n"
        ));
        let width = ranked
            .iter()
            .map(|g| g.0.chars().count())
            .max()
            .unwrap_or(0);
        for (name, stat) in &ranked {
            let cnt = stat.count as u64;
            let cnt_word = plural(cnt, "файл", "файла", "файлов");
            let bytes = stat.bytes;
            let byte_word = plural(bytes, "байт", "байта", "байтов");
            out.push_str(&format!(
                "  {name:width$} {cnt} {cnt_word}, {bytes} {byte_word}\n"
            ));
        }
        out
    }
}

/// Накопитель статистики языковой группы для [`WsMap::summary`].
#[derive(Debug, Default, Clone, Copy)]
struct GroupStat {
    count: usize,
    bytes: u64,
}

/// Рекурсивный обход в глубину с сортировкой каталогов по имени —
/// порядок записей детерминирован (важно и для потолка `max_entries`,
/// и для сравнения снапшотов). Глубина ограничена [`MAX_DEPTH`],
/// поэтому рекурсия безопасна.
fn walk(dir: &Path, rel: &Path, depth: usize, max_entries: usize, out: &mut Vec<WsEntry>) {
    // Файлы каталога глубины `depth` лежат на глубине depth+1 — за потолком.
    if depth >= MAX_DEPTH || out.len() >= max_entries {
        return;
    }
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<fs::DirEntry> = read_dir.flatten().collect();
    children.sort_by_key(fs::DirEntry::file_name);
    for child in children {
        if out.len() >= max_entries {
            return;
        }
        let Ok(file_type) = child.file_type() else {
            continue;
        };
        // Симлинки не ходим: ни вглубь, ни в карту — иначе можно зациклиться
        // или уйти за пределы workspace.
        if file_type.is_symlink() {
            continue;
        }
        let name = child.file_name();
        let rel_child = rel.join(&name);
        if file_type.is_dir() {
            if !is_ignored_dir(&name) {
                walk(&child.path(), &rel_child, depth + 1, max_entries, out);
            }
        } else if file_type.is_file() {
            // Прочее (сокеты, fifo, устройства) в карту не входит.
            if let Ok(metadata) = child.metadata() {
                out.push(make_entry(rel_child, &metadata, &child.path()));
            }
        }
    }
}

/// Собрать запись карты из метаданных; класс определяется по пути
/// (скрытость — она дешевле и важнее) и голове файла (NUL-эвристика).
fn make_entry(rel: PathBuf, metadata: &fs::Metadata, full_path: &Path) -> WsEntry {
    let kind = if is_hidden_path(&rel) {
        WsKind::Hidden
    } else if sniff_binary(full_path) {
        WsKind::Binary
    } else {
        WsKind::Text
    };
    let mtime_secs = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    WsEntry {
        path: rel,
        bytes: metadata.len(),
        mtime_secs,
        kind,
    }
}

/// Скрытый ли путь: точечный компонент на любой глубине
/// (`.gitignore`, `docs/.drafts/a.md`).
fn is_hidden_path(rel: &Path) -> bool {
    rel.components().any(|c| is_hidden_name(c.as_os_str()))
}

/// Имя начинается с точки. Сам компонент `.` сюда не доходит:
/// относительные пути строятся только из нормальных компонентов.
fn is_hidden_name(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

/// Каталог в стоп-листе [`IGNORED_DIRS`] (сравнение точное, любой уровень).
fn is_ignored_dir(name: &OsStr) -> bool {
    IGNORED_DIRS.iter().any(|d| name.to_str() == Some(*d))
}

/// NUL-эвристика бинарности по голове файла (как `grep -I`).
/// Нечитаемый файл бинарным не считается: класс — подсказка, а не вердикт.
fn sniff_binary(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut buf = [0u8; SNIFF_LEN];
    let Ok(n) = file.read(&mut buf) else {
        return false;
    };
    buf[..n].contains(&0)
}

/// Разбить глоб-паттерн на компоненты: пустые сегменты (двойные слэши,
/// крайние слэши) и «текущий каталог» `.` выкидываются.
fn split_pattern(pattern: &str) -> Vec<&str> {
    pattern
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect()
}

/// Компоненты относительного пути как строки (невалидный UTF-8 —
/// lossy, этого достаточно для сопоставления).
fn path_components(path: &Path) -> Vec<String> {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect()
}

/// Относительный путь одной строкой со слэшами — для подстрочного поиска.
fn rel_slash(path: &Path) -> String {
    path_components(path).join("/")
}

/// Сопоставить путь с разобранным глоб-паттерном (см. синтаксис в
/// документации модуля). Компонент `**` поглощает любое число
/// компонентов пути, включая ноль.
fn glob_match(pattern: &[&str], path: &[&str]) -> bool {
    let Some((&first, rest)) = pattern.split_first() else {
        return path.is_empty();
    };
    if first == "**" {
        // Хвост из одной «**» матчит всё оставшееся; иначе перебираем,
        // сколько компонентов она поглотит.
        return rest.is_empty() || (0..=path.len()).any(|skip| glob_match(rest, &path[skip..]));
    }
    match path.split_first() {
        Some((head, tail)) => component_match(first, head) && glob_match(rest, tail),
        None => false,
    }
}

/// Сопоставить один компонент пути с паттерном: `*` — любая
/// последовательность символов (в том числе пустая), `?` — ровно один
/// символ. Классический двухуказательный разбор с откатом к последней
/// звёздочке; сравнение посимвольное, регистрозависимое.
fn component_match(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = name.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // Точка отката последней звёздочки: (её индекс в паттерне, индекс
    // в тексте, с которого возобновлять сопоставление).
    let mut star: Option<(usize, usize)> = None;
    while ti < text.len() {
        if pi < pat.len() && (pat[pi] == '?' || pat[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            // Откат: звёздочка поглощает ещё один символ текста.
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    // Остаток паттерна валиден, только если это сплошные звёздочки.
    pat[pi..].iter().all(|&c| c == '*')
}

/// Ключ языковой группы: `.rs`, `.py`, ... в нижнем регистре;
/// [`NO_EXT_GROUP`] — когда расширения нет или оно не UTF-8.
fn group_key(path: &Path) -> String {
    path.extension()
        .and_then(OsStr::to_str)
        .map_or_else(|| NO_EXT_GROUP.to_string(), |ext| {
            let lower = ext.to_lowercase();
            format!(".{lower}")
        })
}

/// Русская плюрализация: 1 файл / 3 файла / 5 файлов, с исключением
/// 11–14 (`11 файлов`, но `21 файл`).
fn plural(n: u64, one: &'static str, few: &'static str, many: &'static str) -> &'static str {
    if (11..=14).contains(&(n % 100)) {
        return many;
    }
    match n % 10 {
        1 => one,
        2..=4 => few,
        _ => many,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::SystemTime;

    /// Минимальный tempdir без внешних крейтов: уникальное имя + чистка в Drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("theseus-wsmap-{pid}-{n}-{nanos}"));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Создать файл (с родительскими каталогами) и записать содержимое.
    fn write_file(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
    }

    /// Относительные пути записей карты списком строк — удобно сравнивать.
    fn paths(map: &WsMap) -> Vec<String> {
        map.entries.iter().map(|e| rel_slash(&e.path)).collect()
    }

    /// Собрать карту вручную из кортежей (путь, байты, mtime, класс) —
    /// для тестов, которым нужен контроль над метаданными.
    fn fake_map(entries: &[(&str, u64, u64, WsKind)]) -> WsMap {
        WsMap {
            root: PathBuf::from("/fake"),
            entries: entries
                .iter()
                .map(|&(path, bytes, mtime_secs, kind)| WsEntry {
                    path: PathBuf::from(path),
                    bytes,
                    mtime_secs,
                    kind,
                })
                .collect(),
        }
    }

    // ------------------------------------------------------------------
    // scan: обход, метаданные, классы
    // ------------------------------------------------------------------

    #[test]
    fn scan_collects_files_with_metadata() {
        let dir = TempDir::new();
        write_file(dir.path(), "src/main.rs", b"fn main() {}\n");
        write_file(dir.path(), "src/lib.rs", b"pub fn x() {}\n");
        write_file(dir.path(), "README.md", b"# title\n");

        let map = WsMap::scan(dir.path(), 100);
        assert_eq!(map.root, dir.path());
        // Детерминированный порядок: имена отсортированы, обход в глубину.
        assert_eq!(paths(&map), vec!["README.md", "src/lib.rs", "src/main.rs"]);
        let main = map
            .entries
            .iter()
            .find(|e| e.path == Path::new("src/main.rs"))
            .unwrap();
        assert_eq!(main.bytes, 13);
        assert_eq!(main.kind, WsKind::Text);
        assert!(main.mtime_secs > 0, "свежесозданный файл должен иметь mtime");
    }

    #[test]
    fn scan_ignores_service_dirs_but_not_same_named_files() {
        let dir = TempDir::new();
        for rel in [
            ".git/HEAD",
            ".git/objects/ab/cd",
            "target/debug/build.rs",
            ".theseus/state.json",
            "src/sessions/0001.jsonl", // игнор на любой глубине
        ] {
            write_file(dir.path(), rel, b"x\n");
        }
        // Одноимённый стоп-листу обычный файл игнору НЕ подлежит.
        write_file(dir.path(), "sessions", b"plain file\n");
        write_file(dir.path(), "src/main.rs", b"fn main() {}\n");

        let map = WsMap::scan(dir.path(), 100);
        assert_eq!(paths(&map), vec!["sessions", "src/main.rs"]);
    }

    #[test]
    fn scan_marks_hidden_binary_and_text() {
        let dir = TempDir::new();
        write_file(dir.path(), ".gitignore", b"target/\n");
        write_file(dir.path(), ".config/app/settings.toml", b"[x]\n");
        write_file(dir.path(), ".secret", b"\x00\x01"); // скрытость важнее бинарности
        write_file(dir.path(), "data/blob.bin", b"ab\x00cd");
        write_file(dir.path(), "data/empty.dat", b"");
        write_file(dir.path(), "src/visible.rs", b"fn x() {}\n");

        let map = WsMap::scan(dir.path(), 100);
        let kind_of = |rel: &str| {
            map.entries
                .iter()
                .find(|e| e.path == Path::new(rel))
                .unwrap_or_else(|| panic!("нет записи {rel}"))
                .kind
        };
        assert_eq!(kind_of(".gitignore"), WsKind::Hidden);
        assert_eq!(kind_of(".config/app/settings.toml"), WsKind::Hidden);
        assert_eq!(kind_of(".secret"), WsKind::Hidden);
        assert_eq!(kind_of("data/blob.bin"), WsKind::Binary);
        // Пустой файл — текст: NUL в голове не найден.
        assert_eq!(kind_of("data/empty.dat"), WsKind::Text);
        assert_eq!(kind_of("src/visible.rs"), WsKind::Text);
    }

    #[test]
    fn scan_respects_max_depth() {
        let dir = TempDir::new();
        // Файл ровно на глубине MAX_DEPTH (11 каталогов + имя) — в карте.
        let mut shallow = PathBuf::new();
        for i in 1..MAX_DEPTH {
            shallow.push(format!("d{i}"));
        }
        // На глубине MAX_DEPTH + 1 — уже нет.
        let mut deep = shallow.clone();
        deep.push("d12");
        write_file(dir.path(), &shallow.join("here.txt").to_string_lossy(), b"a\n");
        write_file(dir.path(), &deep.join("gone.txt").to_string_lossy(), b"b\n");
        write_file(dir.path(), "top.txt", b"c\n");

        let map = WsMap::scan(dir.path(), 100);
        let got = paths(&map);
        let want = rel_slash(&shallow.join("here.txt"));
        assert!(got.contains(&want), "глубина {MAX_DEPTH} должна входить: {got:?}");
        assert!(
            got.iter().all(|p| !p.ends_with("gone.txt")),
            "глубина {} входить не должна: {got:?}",
            MAX_DEPTH + 1
        );
        assert!(got.contains(&"top.txt".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        write_file(dir.path(), "real/inner.txt", b"x\n");
        write_file(dir.path(), "plain.txt", b"y\n");
        symlink("real", dir.path().join("dirlink")).unwrap();
        symlink("plain.txt", dir.path().join("filelink.txt")).unwrap();

        let map = WsMap::scan(dir.path(), 100);
        // Ни симлинк на каталог (и его «содержимое»), ни симлинк на файл
        // в карту не попадают.
        assert_eq!(paths(&map), vec!["plain.txt", "real/inner.txt"]);
    }

    #[test]
    fn scan_max_entries_caps_result_deterministically() {
        let dir = TempDir::new();
        for i in 0..30 {
            write_file(dir.path(), &format!("f{i:02}.txt"), b"x\n");
        }
        let capped = WsMap::scan(dir.path(), 7);
        assert_eq!(capped.entries.len(), 7);
        // Отсекается хвост отсортированного обхода.
        assert_eq!(paths(&capped)[0], "f00.txt");
        assert_eq!(paths(&capped)[6], "f06.txt");
        // Нулевой потолок — пустая карта.
        let zero = WsMap::scan(dir.path(), 0);
        assert!(zero.entries.is_empty());
        // Повторный скан даёт тот же результат в том же порядке.
        let again = WsMap::scan(dir.path(), 7);
        assert_eq!(paths(&capped), paths(&again));
    }

    #[test]
    fn scan_missing_root_gives_empty_map() {
        let dir = TempDir::new();
        let missing = dir.path().join("no-such-subdir");
        let map = WsMap::scan(&missing, 100);
        assert!(map.entries.is_empty());
        assert_eq!(map.root, missing);
    }

    // ------------------------------------------------------------------
    // filter_glob
    // ------------------------------------------------------------------

    /// Дерево для глоб-тестов; TempDir возвращается, чтобы жил до конца теста.
    fn glob_fixture() -> (TempDir, WsMap) {
        let dir = TempDir::new();
        for rel in [
            "src/main.rs",
            "src/lib.rs",
            "src/agent/loop.rs",
            "src/agent/nested/deep.rs",
            "docs/guide.md",
            "README.md",
            "build.py",
            "src/a.rs",
            "src/ab.txt",
        ] {
            write_file(dir.path(), rel, b"x\n");
        }
        let map = WsMap::scan(dir.path(), 100);
        (dir, map)
    }

    fn glob_paths(map: &WsMap, pattern: &str) -> Vec<String> {
        map.filter_glob(pattern)
            .iter()
            .map(|e| rel_slash(&e.path))
            .collect()
    }

    #[test]
    fn glob_double_star_matches_any_depth() {
        let (_dir, map) = glob_fixture();
        let want = vec![
            "src/a.rs", "src/agent/loop.rs", "src/agent/nested/deep.rs", "src/lib.rs", "src/main.rs",
        ];
        assert_eq!(glob_paths(&map, "**/*.rs"), want);
    }

    #[test]
    fn glob_double_star_matches_zero_components() {
        let (_dir, map) = glob_fixture();
        // «**» между компонентами может не поглотить ничего.
        let want = vec![
            "src/a.rs", "src/agent/loop.rs", "src/agent/nested/deep.rs", "src/lib.rs", "src/main.rs",
        ];
        assert_eq!(glob_paths(&map, "src/**/*.rs"), want);
        // Одинокая «**» матчит всё дерево.
        assert_eq!(glob_paths(&map, "**").len(), map.entries.len());
    }

    #[test]
    fn glob_star_stays_within_component() {
        let (_dir, map) = glob_fixture();
        // «*» не пересекает слэш: ровно один уровень под src.
        assert_eq!(
            glob_paths(&map, "src/*.rs"),
            vec!["src/a.rs", "src/lib.rs", "src/main.rs"]
        );
        assert_eq!(glob_paths(&map, "src/*/*.rs"), vec!["src/agent/loop.rs"]);
        // Точный литеральный путь — тоже валидный паттерн.
        assert_eq!(glob_paths(&map, "src/main.rs"), vec!["src/main.rs"]);
    }

    #[test]
    fn glob_question_mark_matches_exactly_one_char() {
        let (_dir, map) = glob_fixture();
        assert_eq!(glob_paths(&map, "src/?.rs"), vec!["src/a.rs"]);
        // «??» требует два символа — однобуквенное имя не подходит.
        assert!(glob_paths(&map, "src/??.rs").is_empty());
        assert_eq!(glob_paths(&map, "src/??.txt"), vec!["src/ab.txt"]);
    }

    #[test]
    fn glob_without_slash_matches_basename_anywhere() {
        let (_dir, map) = glob_fixture();
        assert_eq!(glob_paths(&map, "*.md"), vec!["README.md", "docs/guide.md"]);
        assert_eq!(glob_paths(&map, "main.*"), vec!["src/main.rs"]);
    }

    #[test]
    fn glob_empty_or_garbage_pattern_matches_nothing() {
        let (_dir, map) = glob_fixture();
        assert!(glob_paths(&map, "").is_empty());
        assert!(glob_paths(&map, "/").is_empty());
        assert!(glob_paths(&map, "zzz/**").is_empty());
    }

    #[test]
    fn component_match_edge_cases() {
        let cases: &[(&str, &str, bool)] = &[
            ("*.rs", "main.rs", true),
            ("*.rs", "main.py", false),
            ("*", "", true), // звезда матчит и пустое имя
            ("*", "anything", true),
            ("?", "", false),   // а «?» требует ровно один символ
            ("?", "ё", true),   // один Unicode-символ — это один символ
            ("a*c", "abc", true),
            ("a*c", "ac", true), // звезда может быть пустой
            ("a*c", "ab", false),
            ("a*b*c", "aXbYc", true),
            ("a*b*c", "acb", false),
            ("main.rs", "main.rs", true),
            ("main.rs", "main.RS", false), // сравнение регистрозависимое
        ];
        for &(pat, name, want) in cases {
            assert_eq!(component_match(pat, name), want, "{pat:?} vs {name:?}");
        }
    }

    // ------------------------------------------------------------------
    // largest / freshest / find_substring
    // ------------------------------------------------------------------

    #[test]
    fn largest_orders_by_size_then_path() {
        let map = fake_map(&[
            ("b/mid.rs", 200, 10, WsKind::Text),
            ("a/big.rs", 900, 20, WsKind::Text),
            ("c/tie1.rs", 200, 30, WsKind::Text),
            ("a/tie0.rs", 200, 40, WsKind::Text),
            ("d/small.rs", 5, 50, WsKind::Text),
        ]);
        let top: Vec<String> = map.largest(3).iter().map(|e| rel_slash(&e.path)).collect();
        // 900 байт — первый; равные 200 — по пути.
        assert_eq!(top, vec!["a/big.rs", "a/tie0.rs", "b/mid.rs"]);
        // n больше числа записей — отдать всё; n=0 — ничего.
        assert_eq!(map.largest(100).len(), 5);
        assert!(map.largest(0).is_empty());
    }

    #[test]
    fn freshest_orders_by_mtime_then_path() {
        let map = fake_map(&[
            ("old.rs", 10, 100, WsKind::Text),
            ("new.rs", 10, 999, WsKind::Text),
            ("mid.rs", 10, 500, WsKind::Text),
        ]);
        let order: Vec<String> = map.freshest(2).iter().map(|e| rel_slash(&e.path)).collect();
        assert_eq!(order, vec!["new.rs", "mid.rs"]);
        assert!(map.freshest(0).is_empty());
    }

    #[test]
    fn find_substring_is_case_insensitive_and_spans_dirs() {
        let map = fake_map(&[
            ("src/MainLoop.rs", 1, 1, WsKind::Text),
            ("SRC/backup.rs", 1, 1, WsKind::Text),
            ("docs/notes.md", 1, 1, WsKind::Text),
            ("Документы/Отчёт.md", 1, 1, WsKind::Text),
        ]);
        let hit = |needle: &str| {
            map.find_substring(needle)
                .iter()
                .map(|e| rel_slash(&e.path))
                .collect::<Vec<_>>()
        };
        assert_eq!(hit("mainloop"), vec!["src/MainLoop.rs"]);
        // Совпадение может захватывать и имя каталога.
        assert_eq!(hit("SRC/"), vec!["src/MainLoop.rs", "SRC/backup.rs"]);
        // Кириллица тоже регистронезависима.
        assert_eq!(hit("ОТЧЁТ"), vec!["Документы/Отчёт.md"]);
        // Пустой запрос — вся карта, семантика str::contains.
        assert_eq!(hit("").len(), 4);
        assert!(hit("нет-такого").is_empty());
    }

    // ------------------------------------------------------------------
    // summary
    // ------------------------------------------------------------------

    #[test]
    fn summary_reports_totals_and_groups() {
        let map = fake_map(&[
            ("a.rs", 100, 1, WsKind::Text),
            ("b.rs", 300, 1, WsKind::Text),
            ("c.py", 50, 1, WsKind::Text),
            ("d.bin", 1000, 1, WsKind::Binary),
            (".hidden", 10, 1, WsKind::Hidden),
            ("README", 5, 1, WsKind::Text),
        ]);
        let s = map.summary();
        let mut lines = s.lines();
        assert_eq!(lines.next(), Some("workspace: /fake"));
        assert_eq!(
            lines.next(),
            Some("файлов: 6, всего 1465 байтов (текстовых: 4, бинарных: 1, скрытых: 1)")
        );
        assert_eq!(
            lines.next(),
            Some("языковые группы по расширениям (топ-10 из 4):")
        );
        // Группы по убыванию суммарного размера; колонка имени выровнена
        // по самой длинной («(без расширения)» — 16 символов).
        let expected = [
            format!("  {:16} 1 файл, 1000 байтов", ".bin"),
            format!("  {:16} 2 файла, 400 байтов", ".rs"),
            format!("  {:16} 1 файл, 50 байтов", ".py"),
            format!("  {:16} 2 файла, 15 байтов", "(без расширения)"),
        ];
        for want in &expected {
            assert_eq!(lines.next(), Some(want.as_str()));
        }
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn summary_keeps_only_top_ten_groups() {
        let entries: Vec<(String, u64, u64, WsKind)> = (0..12u64)
            .map(|i| (format!("f{i:02}.e{i:02}"), i, 0, WsKind::Text))
            .collect();
        let refs: Vec<(&str, u64, u64, WsKind)> = entries
            .iter()
            .map(|(p, b, m, k)| (p.as_str(), *b, *m, *k))
            .collect();
        let map = fake_map(&refs);
        let s = map.summary();
        assert!(s.contains("топ-10 из 12):"), "должно быть 12 групп: {s}");
        // Показаны 10 самых «тяжёлых» групп: .e11 (11 байт) … .e02 (2 байта).
        for i in (2..=11).rev() {
            assert!(s.contains(&format!(".e{i:02}")), "группа .e{i:02} в топе: {s}");
        }
        assert!(!s.contains(".e00"), "нулевая группа в топ не входит: {s}");
        assert!(!s.contains(".e01"), "мелкая группа в топ не входит: {s}");
        // Три строки заголовка + десять строк групп.
        assert_eq!(s.lines().count(), 13);
    }

    #[test]
    fn summary_of_empty_map_has_no_group_section() {
        let map = WsMap::default();
        let s = map.summary();
        assert_eq!(s.lines().count(), 2);
        assert!(s.contains("файлов: 0, всего 0 байтов"));
        assert!(!s.contains("языковые группы"));
    }

    #[test]
    fn plural_picks_russian_forms() {
        assert_eq!(plural(1, "файл", "файла", "файлов"), "файл");
        assert_eq!(plural(2, "файл", "файла", "файлов"), "файла");
        assert_eq!(plural(5, "файл", "файла", "файлов"), "файлов");
        // Исключение 11–14.
        assert_eq!(plural(11, "файл", "файла", "файлов"), "файлов");
        assert_eq!(plural(14, "файл", "файла", "файлов"), "файлов");
        // Сотни не ломают правило: смотрим на последние две цифры.
        assert_eq!(plural(21, "файл", "файла", "файлов"), "файл");
        assert_eq!(plural(111, "файл", "файла", "файлов"), "файлов");
        assert_eq!(plural(122, "файл", "файла", "файлов"), "файла");
    }
}
