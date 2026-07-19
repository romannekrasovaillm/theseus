//! Журнал решений агента (decision log).
//!
//! Образец — практика ADR (Architecture Decision Records) и файл истории
//! сообщений codex: каждое значимое событие сессии (решение, наблюдение,
//! ошибка, вывод) фиксируется одной строкой JSON в append-only журнале,
//! чтобы позже можно было восстановить ход рассуждений агента.
//!
//! Формат хранения — JSON Lines: одна запись [`LogEntry`] = одна строка
//! (переводы строк внутри текста экранируются сериализатором, поэтому
//! физическая строка на запись гарантированно одна). Чтение толерантно
//! к повреждениям: битые строки (обрыв записи при крахе процесса, ручные
//! правки, невалидный UTF-8) пропускаются, а не роняют весь журнал.
//!
//! Поверх сырого хранилища [`LogBook`] даёт запросы: фильтр по виду
//! записи, регистронезависимый поиск по тексту и тегам, «хвост» журнала,
//! срез «с момента», рендер в Markdown с группировкой по суткам (UTC)
//! и ротацию по размеру с атомарной перезаписью (tmp-файл + `rename`).
//!
//! # Пример
//!
//! ```
//! use theseus::logbook::{LogBook, LogEntry, LogKind};
//!
//! let dir = std::env::temp_dir().join(format!("theseus-logbook-doc-{}", std::process::id()));
//! let log = LogBook::new(dir.join("agent.jsonl"));
//!
//! log.append(&LogEntry::new(1_728_000_000, LogKind::Decision, "выбран jsonl вместо sqlite"))?;
//! log.append(&LogEntry::new(1_728_000_060, LogKind::Learning, "rename атомарен в пределах ФС"))?;
//!
//! assert_eq!(log.read_all().len(), 2);
//! assert_eq!(log.filter_kind(LogKind::Decision).len(), 1);
//! # std::fs::remove_dir_all(&dir).ok();
//! # Ok::<(), std::io::Error>(())
//! ```

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

/// Секунд в сутках — сетка группировки по дням и рендера времени (UTC).
const SECS_PER_DAY: u64 = 86_400;

// === Вид записи ===

/// Вид записи журнала.
///
/// Сериализуется в snake_case (`"decision"`, `"observation"`, `"error"`,
/// `"learning"`) — строковые имена стабильны, на них можно полагаться
/// во внешних инструментах, читающих журнал напрямую.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogKind {
    /// Решение: что выбрали и почему (аналог ADR-записи).
    Decision,
    /// Наблюдение: факт о системе, результат замера, вывод команды.
    Observation,
    /// Ошибка: сбой, отказ инструмента, неожиданный результат.
    Error,
    /// Вывод/урок: обобщение, которое стоит учитывать в дальнейшей работе.
    Learning,
}

impl LogKind {
    /// Все виды записей в порядке объявления — для перебора в UI и тестах.
    pub const ALL: [LogKind; 4] = [
        LogKind::Decision,
        LogKind::Observation,
        LogKind::Error,
        LogKind::Learning,
    ];

    /// Строковое имя вида (совпадает с serde-представлением).
    pub fn as_str(self) -> &'static str {
        match self {
            LogKind::Decision => "decision",
            LogKind::Observation => "observation",
            LogKind::Error => "error",
            LogKind::Learning => "learning",
        }
    }

    /// Русская метка для рендера и статус-строк.
    pub fn label(self) -> &'static str {
        match self {
            LogKind::Decision => "Решение",
            LogKind::Observation => "Наблюдение",
            LogKind::Error => "Ошибка",
            LogKind::Learning => "Вывод",
        }
    }
}

impl fmt::Display for LogKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// === Запись ===

/// Одна запись журнала.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Метка времени — секунды Unix-эпохи (UTC).
    pub ts_secs: u64,
    /// Вид записи.
    pub kind: LogKind,
    /// Текст записи (свободная форма; переводы строк допустимы — при
    /// записи они экранируются в `\n` внутри JSON-строки).
    pub text: String,
    /// Теги для поиска и группировки (без ведущего `#`).
    ///
    /// `default`: строки журнала, записанные старыми версиями без поля
    /// `tags`, читаются с пустым списком тегов.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl LogEntry {
    /// Запись без тегов.
    pub fn new(ts_secs: u64, kind: LogKind, text: impl Into<String>) -> Self {
        Self { ts_secs, kind, text: text.into(), tags: Vec::new() }
    }

    /// Запись с тегами.
    pub fn with_tags<S, T>(ts_secs: u64, kind: LogKind, text: impl Into<String>, tags: T) -> Self
    where
        S: Into<String>,
        T: IntoIterator<Item = S>,
    {
        Self {
            ts_secs,
            kind,
            text: text.into(),
            tags: tags.into_iter().map(Into::into).collect(),
        }
    }
}

// === Журнал ===

/// Журнал решений, привязанный к файлу JSON Lines.
///
/// Методы-запросы (`read_all`, `filter_kind`, `search`, `recent`, `since`,
/// `render_markdown`) каждый раз перечитывают файл — журнал может
/// пополняться параллельно (например, субагентами), и кэш устаревал бы.
/// Для журналов сессионного масштаба (тысячи записей) полное чтение дёшево.
#[derive(Debug, Clone)]
pub struct LogBook {
    /// Путь к файлу журнала (jsonl).
    path: PathBuf,
}

impl LogBook {
    /// Журнал по пути `path`. Файл не создаётся до первого [`LogBook::append`].
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Путь к файлу журнала.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Дописать запись в конец журнала одной строкой JSON.
    ///
    /// Родительские каталоги создаются при необходимости. Файл открывается
    /// в режиме append, поэтому параллельные записи из разных процессов не
    /// затирают друг друга (в пределах гарантий ОС для append-записи).
    /// Обрыв записи при крахе оставляет битый «хвост», который чтение
    /// толерантно пропустит.
    pub fn append(&self, entry: &LogEntry) -> io::Result<()> {
        let line = to_line(entry)?;
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// Прочитать все записи журнала в файловом порядке (старые → новые).
    ///
    /// Толерантно к повреждениям: пустые строки, невалидный UTF-8 и строки,
    /// не разбирающиеся как [`LogEntry`], молча пропускаются. Отсутствующий
    /// файл трактуется как пустой журнал; ошибки чтения (права, ввод-вывод)
    /// тоже дают пустой журнал — строгий вариант см. [`LogBook::try_read_all`].
    pub fn read_all(&self) -> Vec<LogEntry> {
        self.try_read_all().unwrap_or_default()
    }

    /// Строгий вариант [`LogBook::read_all`]: ошибки чтения файла (кроме
    /// «файла нет», что даёт пустой журнал) возвращаются вызывающему.
    /// Битые строки по-прежнему пропускаются — повреждённый фрагмент не
    /// должен ронять весь журнал.
    pub fn try_read_all(&self) -> io::Result<Vec<LogEntry>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut entries = Vec::new();
        for chunk in bytes.split(|&b| b == b'\n') {
            let Ok(text) = std::str::from_utf8(chunk) else {
                continue; // невалидный UTF-8 — строка не читается
            };
            let line = text.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<LogEntry>(line) {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// Записи заданного вида в файловом порядке.
    pub fn filter_kind(&self, kind: LogKind) -> Vec<LogEntry> {
        self.read_all().into_iter().filter(|e| e.kind == kind).collect()
    }

    /// Регистронезависимый поиск подстроки по тексту записи и по тегам.
    ///
    /// Пустой (или состоящий из пробелов) запрос возвращает весь журнал —
    /// «нет фильтра». Порядок — файловый.
    pub fn search(&self, query: &str) -> Vec<LogEntry> {
        let needle = query.trim().to_lowercase();
        self.read_all()
            .into_iter()
            .filter(|e| {
                needle.is_empty()
                    || e.text.to_lowercase().contains(&needle)
                    || e.tags.iter().any(|t| t.to_lowercase().contains(&needle))
            })
            .collect()
    }

    /// Последние `n` записей журнала в хронологическом порядке
    /// (старые → новые, самая свежая — последняя).
    ///
    /// `n == 0` даёт пустой список; `n` больше длины журнала — весь журнал.
    pub fn recent(&self, n: usize) -> Vec<LogEntry> {
        let entries = self.read_all();
        let skip = entries.len().saturating_sub(n);
        entries.into_iter().skip(skip).collect()
    }

    /// Записи с меткой времени `>= ts_secs` (граница включительно),
    /// в файловом порядке.
    pub fn since(&self, ts_secs: u64) -> Vec<LogEntry> {
        self.read_all().into_iter().filter(|e| e.ts_secs >= ts_secs).collect()
    }

    /// Рендер журнала в Markdown, сгруппированный по суткам (UTC).
    ///
    /// Формат: заголовок `# Журнал решений`, далее секции `## ГГГГ-ММ-ДД`
    /// в порядке возрастания дат, внутри — пункты
    /// `- ЧЧ:ММ:СС **Вид**: текст` с тегами `` `#тег` `` в конце строки.
    /// Записи внутри суток идут в файловом порядке. Пустой журнал даёт
    /// заголовок и строку «(записей нет)».
    pub fn render_markdown(&self) -> String {
        let entries = self.read_all();
        let mut out = String::from("# Журнал решений\n");
        if entries.is_empty() {
            out.push_str("\n(записей нет)\n");
            return out;
        }
        // BTreeMap даёт возрастающий порядок дат независимо от порядка
        // записей в файле (мало ли — журнал склеен из нескольких).
        let mut by_day: BTreeMap<u64, Vec<&LogEntry>> = BTreeMap::new();
        for entry in &entries {
            by_day.entry(entry.ts_secs / SECS_PER_DAY).or_default().push(entry);
        }
        for (day, day_entries) in &by_day {
            let _ = writeln!(out, "\n## {}", format_day(*day));
            for entry in day_entries {
                let _ = write!(
                    out,
                    "- {} **{}**: {}",
                    format_time_of_day(entry.ts_secs),
                    entry.kind.label(),
                    entry.text
                );
                for tag in &entry.tags {
                    let _ = write!(out, " `#{tag}`");
                }
                out.push('\n');
            }
        }
        out
    }

    /// Ротация журнала по размеру: старейшие записи отбрасываются так,
    /// чтобы оставшийся «хвост» укладывался в `max_bytes`. Возвращает
    /// число отброшенных записей.
    ///
    /// Семантика:
    /// - файл в пределах лимита не перезаписывается (возврат `Ok(0)`);
    /// - самая новая запись сохраняется всегда, даже если одна она
    ///   превышает лимит (иначе журнал обнулялся бы);
    /// - битые строки при перезаписи отбрасываются вместе со старыми
    ///   записями (в счёт отброшенных не входят — они не были записями);
    /// - перезапись атомарна: содержимое пишется во временный файл
    ///   `<имя>.tmp-<pid>` рядом с журналом, сбрасывается на диск
    ///   (`sync_all`) и переименовывается поверх журнала; при ошибке
    ///   временный файл удаляется, чтобы не оставлять сирот.
    pub fn rotate(&self, max_bytes: u64) -> io::Result<usize> {
        let size = match fs::metadata(&self.path) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        if size <= max_bytes {
            return Ok(0);
        }
        let entries = self.try_read_all()?;
        // Собираем «хвост»: идём от самой новой записи к старым, накапливая
        // размер строк (JSON + '\n'). Новейшая запись входит всегда.
        let mut lines: Vec<String> = Vec::with_capacity(entries.len());
        let mut total: u64 = 0;
        for (i, entry) in entries.iter().enumerate().rev() {
            let line = to_line(entry)?;
            let bytes = line.len() as u64 + 1; // + перевод строки
            if i + 1 != entries.len() && total.saturating_add(bytes) > max_bytes {
                break;
            }
            total += bytes;
            lines.push(line);
        }
        lines.reverse();
        let dropped = entries.len() - lines.len();
        write_atomic(&self.path, &lines)?;
        Ok(dropped)
    }
}

// === Вспомогательные функции ===

/// Сериализовать запись в строку JSON (без перевода строки в конце).
fn to_line(entry: &LogEntry) -> io::Result<String> {
    serde_json::to_string(entry).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Атомарно перезаписать журнал `path` строками `lines`: временный файл
/// `<имя>.tmp-<pid>` рядом с журналом (та же файловая система — `rename`
/// атомарен) → `sync_all` → `rename` поверх журнала. При ошибке временный
/// файл удаляется, чтобы не оставлять сирот.
fn write_atomic(path: &Path, lines: &[String]) -> io::Result<()> {
    let tmp = tmp_path(path);
    if let Err(e) = write_and_rename(&tmp, path, lines) {
        let _ = fs::remove_file(&tmp); // не оставляем сироту
        return Err(e);
    }
    Ok(())
}

/// Записать строки во временный файл, сбросить на диск и переименовать
/// поверх `path`.
fn write_and_rename(tmp: &Path, path: &Path, lines: &[String]) -> io::Result<()> {
    let file = File::create(tmp)?;
    let mut writer = BufWriter::new(file);
    for line in lines {
        writeln!(writer, "{line}")?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    fs::rename(tmp, path)
}

/// Путь временного файла для атомарной перезаписи `path` (рядом с журналом,
/// чтобы `rename` не выходил за пределы одной файловой системы).
fn tmp_path(path: &Path) -> PathBuf {
    let pid = std::process::id();
    let name = path.file_name().and_then(OsStr::to_str).unwrap_or("logbook");
    path.with_file_name(format!("{name}.tmp-{pid}"))
}

/// Дата UTC (`ГГГГ-ММ-ДД`) для номера суток от Unix-эпохи.
///
/// Алгоритм Говарда Хиннанта `civil_from_days` — целочисленный, без таблиц;
/// смещение 719468 переводит нулевую точку с 1970-01-01 на 0000-03-01,
/// чтобы високосный день оказался последним днём «условного года».
fn format_day(day: u64) -> String {
    let z = day.saturating_add(719_468);
    let era = z / 146_097;
    let doe = z - era * 146_097; // день эры: [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // год эры: [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // день «года от 1 марта»: [0, 365]
    let mp = (5 * doy + 2) / 153; // месяц (март = 0): [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // день месяца: [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // месяц: [1, 12]
    let year = if m <= 2 { year + 1 } else { year };
    format!("{year:04}-{m:02}-{d:02}")
}

/// Время суток UTC (`ЧЧ:ММ:СС`) для метки времени в секундах.
fn format_time_of_day(ts_secs: u64) -> String {
    let secs = ts_secs % SECS_PER_DAY;
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

// === Тесты ===

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Счётчик для уникальных имён временных каталогов в рамках процесса.
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Временный каталог теста; удаляется при дропе.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("theseus-logbook-test-{}-{n}", std::process::id()));
            fs::create_dir_all(&dir).expect("создать временный каталог");
            Self(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        /// Имена файлов в каталоге (для проверки отсутствия tmp-«сирот»).
        fn file_names(&self) -> Vec<String> {
            fs::read_dir(&self.0)
                .expect("прочитать каталог")
                .map(|e| {
                    e.expect("запись каталога").file_name().to_string_lossy().into_owned()
                })
                .collect()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Журнал в временном каталоге со стандартным именем файла.
    fn make_log(dir: &TestDir) -> LogBook {
        LogBook::new(dir.path("logbook.jsonl"))
    }

    /// Короткий конструктор записи без тегов.
    fn entry(ts_secs: u64, kind: LogKind, text: &str) -> LogEntry {
        LogEntry::new(ts_secs, kind, text)
    }

    #[test]
    fn append_and_read_all_roundtrip() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        let entries = vec![
            LogEntry::with_tags(1_700_000_000, LogKind::Decision, "выбран jsonl", ["формат"]),
            entry(1_700_000_060, LogKind::Observation, "cargo test зелёный"),
            entry(1_700_000_120, LogKind::Error, "таймаут сети"),
        ];
        for e in &entries {
            log.append(e).expect("append");
        }
        assert_eq!(log.read_all(), entries);
    }

    #[test]
    fn append_creates_parent_directories() {
        let dir = TestDir::new();
        let log = LogBook::new(dir.path("nested/deep/log.jsonl"));
        log.append(&entry(1, LogKind::Learning, "x")).expect("append");
        assert_eq!(log.read_all().len(), 1);
    }

    #[test]
    fn read_all_missing_file_returns_empty() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        assert!(log.read_all().is_empty());
        // Запросы поверх пустого журнала — пустые, без паники.
        assert!(log.filter_kind(LogKind::Decision).is_empty());
        assert!(log.search("что угодно").is_empty());
        assert!(log.recent(10).is_empty());
        assert!(log.since(0).is_empty());
    }

    #[test]
    fn broken_lines_are_skipped() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        log.append(&entry(10, LogKind::Decision, "первая")).expect("append");
        log.append(&entry(20, LogKind::Learning, "вторая")).expect("append");
        // Портим файл: мусор, валидный JSON не той формы, обрыв JSON, пустые строки.
        let mut raw = fs::read_to_string(log.path()).expect("прочитать журнал");
        raw.push_str("это не json\n");
        raw.push_str("{\"ts_secs\":30}\n"); // нет kind/text
        raw.push_str("{\"ts_secs\":40,\"kind\":\"decision\",\"text\":\"оборвано\n");
        raw.push_str("\n   \n");
        fs::write(log.path(), raw).expect("перезаписать журнал");

        let all = log.read_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].text, "первая");
        assert_eq!(all[1].text, "вторая");
    }

    #[test]
    fn invalid_utf8_line_is_skipped() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        log.append(&entry(1, LogKind::Observation, "до")).expect("append");
        log.append(&entry(2, LogKind::Observation, "после")).expect("append");
        // Вставляем строку с невалидным UTF-8 между записями.
        let mut raw = fs::read(log.path()).expect("прочитать байты");
        let cut = raw.iter().position(|&b| b == b'\n').expect("перевод строки") + 1;
        raw.splice(cut..cut, b"\xFF\xFE garbage\n".iter().copied());
        fs::write(log.path(), raw).expect("перезаписать журнал");

        let all = log.read_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].text, "до");
        assert_eq!(all[1].text, "после");
    }

    #[test]
    fn tags_roundtrip_and_missing_tags_default() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        let with = LogEntry::with_tags(5, LogKind::Decision, "с тегами", ["а", "б"]);
        let without = entry(6, LogKind::Error, "без тегов");
        log.append(&with).expect("append");
        log.append(&without).expect("append");
        // Строка без поля tags (журнал от старой версии) читается с пустыми тегами.
        let mut f = OpenOptions::new().append(true).open(log.path()).expect("открыть");
        writeln!(f, "{{\"ts_secs\":7,\"kind\":\"learning\",\"text\":\"старая строка\"}}")
            .expect("дописать");

        let all = log.read_all();
        assert_eq!(all[0].tags, vec!["а".to_string(), "б".to_string()]);
        assert!(all[1].tags.is_empty());
        assert!(all[2].tags.is_empty());
        assert_eq!(all[2].kind, LogKind::Learning);
    }

    #[test]
    fn kind_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&LogKind::Observation).expect("json"), "\"observation\"");
        assert_eq!(LogKind::as_str(LogKind::Learning), "learning");
        assert_eq!(LogKind::Learning.to_string(), "learning");
        assert_eq!(LogKind::Error.label(), "Ошибка");
        // Перебор через ALL покрывает все виды ровно один раз.
        let mut seen: Vec<LogKind> = LogKind::ALL.to_vec();
        seen.dedup();
        assert_eq!(seen.len(), 4);
    }

    #[test]
    fn filter_kind_selects_only_requested() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        let kinds = [
            LogKind::Decision,
            LogKind::Error,
            LogKind::Decision,
            LogKind::Learning,
            LogKind::Observation,
        ];
        for (i, kind) in kinds.into_iter().enumerate() {
            log.append(&entry(i as u64, kind, &format!("запись {i}"))).expect("append");
        }
        let decisions = log.filter_kind(LogKind::Decision);
        assert_eq!(decisions.len(), 2);
        assert!(decisions.iter().all(|e| e.kind == LogKind::Decision));
        // По каждому виду фильтр возвращает только его записи.
        for kind in LogKind::ALL {
            assert!(log.filter_kind(kind).iter().all(|e| e.kind == kind));
        }
    }

    #[test]
    fn search_matches_text_case_insensitively() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        log.append(&entry(1, LogKind::Decision, "Выбран Rustls вместо OpenSSL")).expect("append");
        log.append(&entry(2, LogKind::Observation, "ничего про TLS")).expect("append");

        assert_eq!(log.search("rustls").len(), 1);
        assert_eq!(log.search("RUSTLS").len(), 1);
        assert_eq!(log.search("openssl").len(), 1);
        assert_eq!(log.search("tls").len(), 2); // подстрока — в обеих записях
        assert!(log.search("gnutls").is_empty());
    }

    #[test]
    fn search_matches_tags_case_insensitively() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        log.append(&LogEntry::with_tags(
            1,
            LogKind::Learning,
            "rename атомарен",
            ["Файловая-Система", "posix"],
        ))
        .expect("append");
        log.append(&entry(2, LogKind::Learning, "без тегов")).expect("append");

        assert_eq!(log.search("файловая-система").len(), 1);
        assert_eq!(log.search("POSIX").len(), 1);
        assert_eq!(log.search("систем").len(), 1); // подстрока тега тоже матчится
    }

    #[test]
    fn search_with_empty_query_returns_all() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        log.append(&entry(1, LogKind::Decision, "один")).expect("append");
        log.append(&entry(2, LogKind::Decision, "два")).expect("append");
        assert_eq!(log.search("").len(), 2);
        assert_eq!(log.search("   ").len(), 2);
    }

    #[test]
    fn recent_returns_tail_chronologically() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        for i in 0..5 {
            log.append(&entry(i, LogKind::Observation, &format!("запись {i}"))).expect("append");
        }
        let tail = log.recent(2);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].text, "запись 3");
        assert_eq!(tail[1].text, "запись 4");
        // n больше длины — весь журнал; n = 0 — пусто.
        assert_eq!(log.recent(100).len(), 5);
        assert!(log.recent(0).is_empty());
    }

    #[test]
    fn since_filters_inclusively() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        for ts in [100, 200, 300] {
            log.append(&entry(ts, LogKind::Observation, &format!("t{ts}"))).expect("append");
        }
        let got = log.since(200);
        assert_eq!(got.len(), 2); // граница включительно
        assert_eq!(got[0].ts_secs, 200);
        assert!(log.since(301).is_empty());
        assert_eq!(log.since(0).len(), 3);
    }

    #[test]
    fn render_markdown_groups_entries_by_day() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        // День 0 (1970-01-01) и день 1 (1970-01-02); в файле дни «перемешаны».
        log.append(&LogEntry::with_tags(3_600, LogKind::Decision, "утреннее решение", ["план"]))
            .expect("append");
        log.append(&entry(86_400 + 1_800, LogKind::Error, "ошибка второго дня")).expect("append");
        log.append(&entry(7_200, LogKind::Learning, "вывод первого дня")).expect("append");

        let md = log.render_markdown();
        assert!(md.starts_with("# Журнал решений\n"));
        let day1 = md.find("## 1970-01-01").expect("секция первого дня");
        let day2 = md.find("## 1970-01-02").expect("секция второго дня");
        assert!(day1 < day2, "дни идут в порядке возрастания");
        // В секции первого дня — обе его записи, в файловом порядке.
        let sec1 = &md[day1..day2];
        let decision = sec1.find("01:00:00 **Решение**: утреннее решение `#план`").expect("решение");
        let learning = sec1.find("02:00:00 **Вывод**: вывод первого дня").expect("вывод");
        assert!(decision < learning, "внутри суток — файловый порядок");
        assert!(md[day2..].contains("00:30:00 **Ошибка**: ошибка второго дня"));
    }

    #[test]
    fn render_markdown_empty_log() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        let md = log.render_markdown();
        assert!(md.starts_with("# Журнал решений"));
        assert!(md.contains("(записей нет)"));
        assert!(!md.contains("## "));
    }

    #[test]
    fn rotate_keeps_tail_and_reports_exact_drop_count() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        // Записи с одинаковой сериализованной длиной для точного расчёта.
        for i in 0..10u64 {
            log.append(&entry(i, LogKind::Observation, &format!("запись {i:02}"))).expect("append");
        }
        let line_len =
            serde_json::to_string(&entry(0, LogKind::Observation, "запись 00")).expect("json").len()
                as u64
                + 1;
        let dropped = log.rotate(line_len * 4).expect("rotate");
        assert_eq!(dropped, 6);
        let rest = log.read_all();
        assert_eq!(rest.len(), 4);
        assert_eq!(rest[0].text, "запись 06");
        assert_eq!(rest[3].text, "запись 09");
        // Файл — ровно четыре хвостовые строки.
        assert_eq!(fs::metadata(log.path()).expect("метаданные").len(), line_len * 4);
    }

    #[test]
    fn rotate_within_budget_is_noop() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        log.append(&entry(1, LogKind::Decision, "единственная")).expect("append");
        let before = fs::read(log.path()).expect("прочитать");
        let dropped = log.rotate(1_000_000).expect("rotate");
        assert_eq!(dropped, 0);
        assert_eq!(fs::read(log.path()).expect("прочитать"), before);
        // Отсутствующего файла ротация тоже не касается.
        let missing = LogBook::new(dir.path("missing.jsonl"));
        assert_eq!(missing.rotate(0).expect("rotate"), 0);
        assert!(!missing.path().exists());
    }

    #[test]
    fn rotate_keeps_newest_even_over_budget() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        for i in 0..3 {
            log.append(&entry(i, LogKind::Error, &format!("ошибка {i}"))).expect("append");
        }
        // Лимит 0: влезает только гарантированно сохраняемая новейшая запись.
        let dropped = log.rotate(0).expect("rotate");
        assert_eq!(dropped, 2);
        let rest = log.read_all();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].text, "ошибка 2");
    }

    #[test]
    fn rotate_leaves_no_tmp_files_and_valid_journal() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        for i in 0..20u64 {
            log.append(&entry(i, LogKind::Learning, &format!("урок {i}"))).expect("append");
        }
        let dropped = log.rotate(300).expect("rotate");
        assert!(dropped > 0, "журнал из 20 записей в 300 байт не влезает");
        // Атомарность: каталог чист, ни одного tmp-файла после ротации.
        assert_eq!(dir.file_names(), vec!["logbook.jsonl".to_string()]);
        // Журнал остался валидным jsonl: каждая строка разбирается.
        let raw = fs::read_to_string(log.path()).expect("прочитать журнал");
        assert!(raw.lines().all(|l| serde_json::from_str::<LogEntry>(l).is_ok()));
        // И дописывать можно дальше — журнал «живой».
        log.append(&entry(100, LogKind::Decision, "после ротации")).expect("append");
        assert_eq!(log.read_all().last().expect("хвост").text, "после ротации");
    }

    #[test]
    fn multiline_text_stays_single_jsonl_line() {
        let dir = TestDir::new();
        let log = make_log(&dir);
        let text = "строка 1\nстрока 2\r\nстрока 3";
        log.append(&entry(1, LogKind::Observation, text)).expect("append");
        let raw = fs::read_to_string(log.path()).expect("прочитать журнал");
        assert_eq!(raw.lines().count(), 1);
        assert_eq!(log.read_all()[0].text, text);
    }

    #[test]
    fn day_and_time_formatting() {
        assert_eq!(format_day(0), "1970-01-01");
        assert_eq!(format_day(19_000), "2022-01-08");
        assert_eq!(format_day(20_000), "2024-10-04");
        assert_eq!(format_time_of_day(0), "00:00:00");
        assert_eq!(format_time_of_day(86_399), "23:59:59");
        assert_eq!(format_time_of_day(86_400 + 3_661), "01:01:01");
    }
}
