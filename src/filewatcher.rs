//! Детектор внешних изменений workspace во время хода агента.
//!
//! Мотивация (по образцу `codex-rs/file-watcher`): пока модель «думает» и
//! применяет правки, пользователь или другой процесс может поменять файлы
//! «под ногами» агента. Модуль даёт три примитива:
//!
//! * [`Watcher`] — поллинг-снимок дерева файлов ([`Watcher::scan`]). Без
//!   `inotify` и внешних крейтов: только `std`. Сигнатура файла — [`FileSig`]
//!   (mtime, длина, FNV-1a первых 8 КиБ): правка ловится даже при сохранённом
//!   mtime и той же длине.
//! * [`WatchHandle`] — фоновый поток: сравнивает снимки и зовёт колбэк
//!   с [`Diff`] (added / modified / deleted).
//! * [`EditGuard`] — guard-режим для `edit_file`: фиксирует сигнатуру до
//!   правки модели и перед записью проверяет, не изменился ли файл снаружи
//!   (conflict detection → [`EditConflict::ModifiedExternally`]).
//!
//! Игнорируются `.git` и `target` (на любом уровне, как в `.gitignore`) и
//! `.theseus/sessions`. Симлинки не обходятся — защита от циклов. Пути
//! в снимках и диффах — относительные (от корня [`Watcher`]).

use std::collections::HashMap;
use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

/// Сколько первых байт файла участвуют в хэше (8 КиБ).
///
/// Делает поллинг дешёвым на больших файлах; платой является слепота к
/// правке дальше префикса без смены mtime и длины (редкий случай).
pub const HASH_PREFIX_LEN: usize = 8 * 1024;

/// Базис FNV-1a 64-бит.
const FNV1A_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// Простое число FNV 64-бит.
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Максимальная порция сна в потоке наблюдателя: [`WatchHandle::stop`]
/// останавливает поток не дольше чем за этот срок (плюс время одного scan).
const STOP_POLL_SLICE: Duration = Duration::from_millis(50);

/// Минимальный интервал опроса: нулевой превратил бы цикл в busy-loop.
const MIN_INTERVAL: Duration = Duration::from_millis(1);

/// FNV-1a 64-бит от среза байт (полного, без усечения префикса).
///
/// Публичная: пригодится коду, сверяющему сигнатуру вне [`sign_file`].
#[must_use]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    fnv1a64_update(FNV1A_OFFSET_BASIS, bytes)
}

/// Один шаг цепочки FNV-1a поверх очередной порции байт.
fn fnv1a64_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash = (hash ^ u64::from(b)).wrapping_mul(FNV1A_PRIME);
    }
    hash
}

/// FNV-1a от первых [`HASH_PREFIX_LEN`] байт открытого файла.
///
/// Читает не более префикса, короткое чтение (EOF) завершает цикл.
fn fnv1a_prefix(file: &fs::File) -> io::Result<u64> {
    let mut reader = file;
    let mut hash = FNV1A_OFFSET_BASIS;
    let mut remaining = HASH_PREFIX_LEN;
    let mut buf = [0u8; 4096];
    while remaining > 0 {
        let want = remaining.min(buf.len());
        let n = reader.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        hash = fnv1a64_update(hash, &buf[..n]);
        remaining -= n;
    }
    Ok(hash)
}

/// Сигнатура файла: mtime + длина + хэш содержимого.
///
/// Сравнение — по всем трём полям (`PartialEq`). Семантика консервативная:
/// `touch` без смены содержимого тоже считается изменением — для детекции
/// конфликтов ложноположительное срабатывание безопаснее пропущенной правки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileSig {
    /// Время последней модификации по `stat`.
    pub mtime: SystemTime,
    /// Размер файла в байтах.
    pub len: u64,
    /// FNV-1a первых [`HASH_PREFIX_LEN`] байт содержимого.
    pub hash64: u64,
}

/// Снимок дерева файлов: относительный путь → сигнатура.
pub type Snapshot = HashMap<PathBuf, FileSig>;

/// Снять сигнатуру одного файла.
///
/// Возвращает `Ok(None)`, если путь не существует или не является обычным
/// файлом (каталог, симлинк-оборвавшийся и т.п.) — для guard-логики оба
/// случая равнозначны «файла нет».
///
/// # Ошибки
/// Ошибки ввода-вывода, отличные от `NotFound` (например, `PermissionDenied`).
pub fn sign_file(path: &Path) -> io::Result<Option<FileSig>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let md = file.metadata()?;
    // На unix `File::open` успешно открывает и каталог — отсекаем по типу.
    if !md.is_file() {
        return Ok(None);
    }
    let hash64 = fnv1a_prefix(&file)?;
    Ok(Some(FileSig { mtime: md.modified()?, len: md.len(), hash64 }))
}

/// Проверка пути (относительного) против списка игноров.
///
/// * `.git` и `target` — как компонент пути на любом уровне (семантика
///   «голого» имени в `.gitignore`);
/// * `.theseus/sessions` — только этот конкретный префикс.
fn is_ignored(rel: &Path) -> bool {
    let mut comps = rel.components();
    let first = comps.next().map(Component::as_os_str);
    let second = comps.next().map(Component::as_os_str);
    if first == Some(OsStr::new(".theseus")) && second == Some(OsStr::new("sessions")) {
        return true;
    }
    rel.components().any(|c| {
        let name = c.as_os_str();
        name == ".git" || name == "target"
    })
}

/// Разница двух снимков дерева файлов.
///
/// Все три списка отсортированы — детерминированный вывод для логов/трассировки.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    /// Пути, появившиеся после базового снимка.
    pub added: Vec<PathBuf>,
    /// Пути, чья сигнатура изменилась (включая перезапись тем же путём).
    pub modified: Vec<PathBuf>,
    /// Пути, исчезнувшие после базового снимка.
    pub deleted: Vec<PathBuf>,
}

impl Diff {
    /// `true`, если изменений нет.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }

    /// Суммарное число изменённых путей.
    #[must_use]
    pub fn total(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len()
    }
}

impl fmt::Display for Diff {
    /// Компактный вид для логов: `+added ~modified -deleted`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "+{} ~{} -{}", self.added.len(), self.modified.len(), self.deleted.len())
    }
}

/// Поллинг-наблюдатель за деревом файлов workspace.
///
/// Сам по себе `Watcher` без состояния и потоков: только конфигурация
/// (корень + интервал) и чистые операции сканирования/сравнения. Фоновый
/// запуск — через [`WatchHandle`].
#[derive(Debug, Clone)]
pub struct Watcher {
    root: PathBuf,
    interval: Duration,
}

impl Watcher {
    /// Создать наблюдателя за каталогом `root` с интервалом опроса `interval`.
    ///
    /// Интервал клампится снизу до 1 мс (см. [`MIN_INTERVAL`]): нулевой
    /// превратил бы цикл наблюдения в busy-loop.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, interval: Duration) -> Self {
        Self { root: root.into(), interval: interval.max(MIN_INTERVAL) }
    }

    /// Корень наблюдения.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Интервал опроса (после клампа).
    #[must_use]
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Снять полный снимок дерева: обойти `root` рекурсивно и подписать
    /// каждый неигнорируемый файл.
    ///
    /// Нечитаемые записи (права, гонки удаления) пропускаются; отсутствующий
    /// или не-каталожный `root` даёт пустой снимок. Стоимость: на каждый
    /// файл — `open` + чтение до [`HASH_PREFIX_LEN`] байт; хэш пересчитывается
    /// всегда (иначе правка с сохранённым mtime прошла бы незамеченной).
    ///
    /// # Ошибки
    /// Ошибка `stat` корня, отличная от `NotFound`.
    pub fn scan(&self) -> io::Result<Snapshot> {
        let mut out = Snapshot::new();
        match fs::metadata(&self.root) {
            Ok(md) if md.is_dir() => walk(&self.root, &self.root, &mut out),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(out)
    }

    /// Сканировать и сравнить с базовым снимком.
    ///
    /// # Ошибки
    /// Те же, что у [`Watcher::scan`].
    pub fn watch_since(&self, baseline: &Snapshot) -> io::Result<Diff> {
        let current = self.scan()?;
        Ok(Self::diff_snapshots(baseline, &current))
    }

    /// Чистое сравнение двух снимков (без обращения к диску).
    #[must_use]
    pub fn diff_snapshots(baseline: &Snapshot, current: &Snapshot) -> Diff {
        let mut diff = Diff::default();
        for (path, sig) in current {
            match baseline.get(path) {
                None => diff.added.push(path.clone()),
                Some(old) if old != sig => diff.modified.push(path.clone()),
                Some(_) => {}
            }
        }
        for path in baseline.keys() {
            if !current.contains_key(path) {
                diff.deleted.push(path.clone());
            }
        }
        diff.added.sort();
        diff.modified.sort();
        diff.deleted.sort();
        diff
    }
}

/// Рекурсивный обход каталога `dir` (внутри `root`) со сбором сигнатур.
fn walk(root: &Path, dir: &Path, out: &mut Snapshot) {
    let Ok(entries) = fs::read_dir(dir) else {
        // Нечитаемый подкаталог (права, гонка удаления) — пропускаем ветку.
        return;
    };
    for entry in entries.flatten() {
        let Ok(ftype) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if ftype.is_dir() {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            if is_ignored(rel) {
                continue;
            }
            walk(root, &path, out);
        } else if ftype.is_file() {
            visit_file(root, &path, out);
        }
        // Симлинки и спецфайлы (fifo, сокеты...) сознательно пропускаем.
    }
}

/// Подписать один файл, если он не под игнором, и положить в снимок.
fn visit_file(root: &Path, path: &Path, out: &mut Snapshot) {
    let Ok(rel) = path.strip_prefix(root) else {
        return;
    };
    if is_ignored(rel) {
        return;
    }
    // Файл могли удалить между read_dir и чтением — Ok(None) его пропускает.
    if let Ok(Some(sig)) = sign_file(path) {
        out.insert(rel.to_path_buf(), sig);
    }
}

/// Цикл фонового наблюдателя: спит интервал порциями, сканирует, зовёт колбэк.
///
/// Базовый снимок снимается при старте; первый колбэк — только про реальные
/// изменения после старта. Если первый `scan` не удался, первый успешный
/// снимок просто становится базой (без ложного «всё добавлено»). Ошибки
/// отдельных `scan` пропускаются. Паника в `on_change` убивает поток.
fn watch_loop<F>(watcher: &Watcher, stop: &AtomicBool, on_change: F)
where
    F: Fn(Diff),
{
    let mut baseline = watcher.scan().ok();
    let slice = watcher.interval.min(STOP_POLL_SLICE);
    loop {
        // Спим интервал маленькими порциями, чтобы stop() отрабатывал
        // за время ~STOP_POLL_SLICE, а не за целый interval.
        let mut waited = Duration::ZERO;
        while waited < watcher.interval {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(slice);
            waited += slice;
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let Ok(current) = watcher.scan() else {
            continue;
        };
        if let Some(base) = &baseline {
            let diff = Watcher::diff_snapshots(base, &current);
            if !diff.is_empty() {
                on_change(diff);
            }
        }
        baseline = Some(current);
    }
}

/// Ручка фонового потока наблюдателя.
///
/// Владеет потоком: [`WatchHandle::stop`] (или `Drop`) выставляет флаг
/// остановки и дожидается завершения потока — «висючих» потоков не остаётся.
#[derive(Debug)]
pub struct WatchHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WatchHandle {
    /// Запустить фоновое наблюдение: `on_change` будет вызван из потока
    /// наблюдателя на каждый непустой [`Diff`]. Колбэк не должен
    /// блокироваться надолго — это замедлит следующий цикл опроса.
    #[must_use]
    pub fn start<F>(watcher: Watcher, on_change: F) -> Self
    where
        F: Fn(Diff) + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::spawn(move || watch_loop(&watcher, &flag, on_change));
        Self { stop, thread: Some(thread) }
    }

    /// `true`, если поток наблюдателя жив (запущен и ещё не завершился).
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Остановить наблюдение и присоединить поток.
    ///
    /// Идемпотентно. Завершается не дольше чем за [`STOP_POLL_SLICE`] +
    /// время одного `scan`. Паника потока при остановке проглатывается.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            drop(thread.join());
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Ошибка guard-режима `edit_file`: конфликт или сбой ввода-вывода.
#[derive(Debug)]
pub enum EditConflict {
    /// Файл изменился снаружи между фиксацией сигнатуры и проверкой
    /// (правка, удаление или появление ранее отсутствующего файла).
    ModifiedExternally {
        /// Путь, по которому обнаружен конфликт.
        path: PathBuf,
    },
    /// Ошибка ввода-вывода при чтении сигнатуры или при самой записи.
    Io {
        /// Путь, на котором произошла ошибка.
        path: PathBuf,
        /// Исходная ошибка.
        source: io::Error,
    },
}

impl EditConflict {
    /// Путь, из-за которого возникла ошибка/конфликт.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::ModifiedExternally { path } | Self::Io { path, .. } => path,
        }
    }

    /// `true`, если это именно конфликт (а не сбой IO).
    #[must_use]
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::ModifiedExternally { .. })
    }
}

impl fmt::Display for EditConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModifiedExternally { path } => write!(
                f,
                "конфликт: файл {} изменён снаружи с момента фиксации сигнатуры",
                path.display()
            ),
            Self::Io { path, source } => {
                write!(f, "ошибка доступа к {}: {source}", path.display())
            }
        }
    }
}

impl Error for EditConflict {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::ModifiedExternally { .. } => None,
        }
    }
}

/// Guard-режим для `edit_file`: детекция внешних правок между чтением
/// файла моделью и записью результата.
///
/// Последовательность: [`EditGuard::capture`] — до правки модели;
/// [`EditGuard::check`] / [`EditGuard::write_if_unchanged`] — перед записью;
/// [`EditGuard::refresh`] — после собственной успешной записи.
///
/// Между проверкой и записью остаётся неустранимое на `std` окно TOCTOU —
/// guard сужает его до минимума, но не делает запись транзакционной.
#[derive(Debug, Clone)]
pub struct EditGuard {
    path: PathBuf,
    before: Option<FileSig>,
}

impl EditGuard {
    /// Зафиксировать текущую сигнатуру `path` (`None` — файла пока нет).
    ///
    /// # Ошибки
    /// Ошибки ввода-вывода, отличные от `NotFound`.
    pub fn capture(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let before = sign_file(&path)?;
        Ok(Self { path, before })
    }

    /// Путь, за которым следит guard.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Сигнатура, зафиксированная при `capture`/`refresh`.
    #[must_use]
    pub fn before(&self) -> Option<FileSig> {
        self.before
    }

    /// Проверить, что файл не менялся снаружи с момента фиксации.
    ///
    /// Конфликтом считается любое расхождение сигнатур, включая переходы
    /// «был → удалён» и «не было → появился».
    ///
    /// # Ошибки
    /// [`EditConflict::ModifiedExternally`] при расхождении сигнатур,
    /// [`EditConflict::Io`] при сбое чтения.
    pub fn check(&self) -> Result<(), EditConflict> {
        let now = sign_file(&self.path)
            .map_err(|source| EditConflict::Io { path: self.path.clone(), source })?;
        if now == self.before {
            Ok(())
        } else {
            Err(EditConflict::ModifiedExternally { path: self.path.clone() })
        }
    }

    /// Паттерн «проверить и сразу записать»: `write` выполняется только
    /// если [`EditGuard::check`] прошёл.
    ///
    /// # Ошибки
    /// [`EditConflict::ModifiedExternally`] — запись не выполнялась;
    /// [`EditConflict::Io`] — сбой проверки или самой записи.
    pub fn write_if_unchanged<F, T>(&self, write: F) -> Result<T, EditConflict>
    where
        F: FnOnce(&Path) -> io::Result<T>,
    {
        self.check()?;
        write(&self.path)
            .map_err(|source| EditConflict::Io { path: self.path.clone(), source })
    }

    /// Перефиксировать сигнатуру после собственной успешной записи.
    ///
    /// # Ошибки
    /// Ошибки ввода-вывода, отличные от `NotFound`.
    pub fn refresh(&mut self) -> io::Result<()> {
        self.before = sign_file(&self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::UNIX_EPOCH;

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
            let dir = std::env::temp_dir()
                .join(format!("theseus-filewatcher-{}-{n}-{nanos}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(fs::remove_dir_all(&self.0));
        }
    }

    /// Записать текстовый файл, создав родительские каталоги.
    fn write_file(path: &Path, data: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, data).unwrap();
    }

    /// Наблюдатель с коротким интервалом для тестов.
    fn watcher_of(dir: &Path) -> Watcher {
        Watcher::new(dir, Duration::from_millis(10))
    }

    #[test]
    fn fnv1a64_reference_vectors() {
        // Канонические значения FNV-1a 64-бит.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn sign_file_missing_and_dir_return_none() {
        let tmp = TempDir::new();
        assert!(sign_file(&tmp.path().join("nope.txt")).unwrap().is_none());
        // Каталог успешно открывается File::open, но сигнатуры не имеет.
        assert!(sign_file(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn sign_file_captures_len_mtime_hash() {
        let tmp = TempDir::new();
        let file = tmp.path().join("a.txt");
        write_file(&file, "hello");
        let sig = sign_file(&file).unwrap().unwrap();
        assert_eq!(sig.len, 5);
        assert_eq!(sig.hash64, fnv1a64(b"hello"));
        // mtime вменяемый: не эпоха и не будущее.
        assert!(sig.mtime > SystemTime::UNIX_EPOCH);
        assert!(sig.mtime <= SystemTime::now());
    }

    #[test]
    fn scan_finds_files_recursively_and_signs_empty() {
        let tmp = TempDir::new();
        write_file(&tmp.path().join("top.txt"), "1");
        write_file(&tmp.path().join("src/main.rs"), "fn main() {}");
        write_file(&tmp.path().join("src/deep/nested.txt"), "deep");
        write_file(&tmp.path().join("empty.txt"), "");
        let snap = watcher_of(tmp.path()).scan().unwrap();
        assert_eq!(snap.len(), 4);
        assert!(snap.contains_key(Path::new("top.txt")));
        assert!(snap.contains_key(Path::new("src/main.rs")));
        assert!(snap.contains_key(Path::new("src/deep/nested.txt")));
        let empty = snap[Path::new("empty.txt")];
        assert_eq!(empty.len, 0);
        assert_eq!(empty.hash64, fnv1a64(b""));
    }

    #[test]
    fn scan_missing_root_is_empty_snapshot() {
        let tmp = TempDir::new();
        let missing = tmp.path().join("no-such-dir");
        let snap = watcher_of(&missing).scan().unwrap();
        assert!(snap.is_empty());
    }

    #[test]
    fn scan_ignores_vcs_build_and_sessions() {
        let tmp = TempDir::new();
        write_file(&tmp.path().join(".git/HEAD"), "ref: refs/heads/main");
        write_file(&tmp.path().join(".git/objects/ab/cdef"), "blob");
        write_file(&tmp.path().join("target/debug/build.out"), "bin");
        write_file(&tmp.path().join("nested/member/target/x.o"), "obj");
        write_file(&tmp.path().join(".theseus/sessions/s1.jsonl"), "{}");
        write_file(&tmp.path().join(".theseus/config.toml"), "model = \"k\"");
        write_file(&tmp.path().join("src/main.rs"), "fn main() {}");
        write_file(&tmp.path().join("keep.txt"), "keep");
        let snap = watcher_of(tmp.path()).scan().unwrap();
        assert_eq!(snap.len(), 3);
        assert!(snap.contains_key(Path::new("keep.txt")));
        assert!(snap.contains_key(Path::new("src/main.rs")));
        assert!(snap.contains_key(Path::new(".theseus/config.toml")));
        assert!(!snap.keys().any(|p| p.starts_with(".git")));
        assert!(!snap.keys().any(|p| p.starts_with("target")));
        assert!(!snap.keys().any(|p| p.starts_with(".theseus/sessions")));
        assert!(!snap.keys().any(|p| p.starts_with("nested/member/target")));
    }

    #[test]
    fn ignore_rules_match_gitignore_semantics() {
        assert!(is_ignored(Path::new(".git")));
        assert!(is_ignored(Path::new(".git/config")));
        assert!(is_ignored(Path::new("target")));
        assert!(is_ignored(Path::new("target/debug/a.out")));
        assert!(is_ignored(Path::new("crates/member/target/x.o")));
        assert!(is_ignored(Path::new(".theseus/sessions")));
        assert!(is_ignored(Path::new(".theseus/sessions/s.jsonl")));
        assert!(!is_ignored(Path::new(".theseus/config.toml")));
        assert!(!is_ignored(Path::new("src/target.rs")));
        assert!(!is_ignored(Path::new("src/main.rs")));
    }

    #[test]
    fn diff_added_modified_deleted_and_noop() {
        let tmp = TempDir::new();
        let w = watcher_of(tmp.path());
        write_file(&tmp.path().join("same.txt"), "same");
        write_file(&tmp.path().join("mod.txt"), "v1");
        write_file(&tmp.path().join("del.txt"), "bye");
        let baseline = w.scan().unwrap();
        // Без изменений дифф пустой.
        assert!(w.watch_since(&baseline).unwrap().is_empty());
        // Одну правим, одну удаляем, одну добавляем, одну не трогаем.
        write_file(&tmp.path().join("mod.txt"), "v2 longer content");
        fs::remove_file(tmp.path().join("del.txt")).unwrap();
        write_file(&tmp.path().join("new.txt"), "hello");
        let diff = w.watch_since(&baseline).unwrap();
        assert_eq!(diff.added, vec![PathBuf::from("new.txt")]);
        assert_eq!(diff.modified, vec![PathBuf::from("mod.txt")]);
        assert_eq!(diff.deleted, vec![PathBuf::from("del.txt")]);
        assert_eq!(diff.total(), 3);
        assert_eq!(diff.to_string(), "+1 ~1 -1");
        // Дифф от того же baseline идемпотентен.
        let again = w.watch_since(&baseline).unwrap();
        assert_eq!(diff, again);
    }

    #[test]
    fn diff_snapshots_pure_comparison() {
        // diff_snapshots не ходит на диск: собираем снимки руками.
        let mut base = Snapshot::new();
        let mut cur = Snapshot::new();
        let sig = FileSig { mtime: SystemTime::UNIX_EPOCH, len: 1, hash64: 7 };
        base.insert(PathBuf::from("a"), sig);
        cur.insert(PathBuf::from("b"), sig);
        let diff = Watcher::diff_snapshots(&base, &cur);
        assert_eq!(diff.added, vec![PathBuf::from("b")]);
        assert_eq!(diff.deleted, vec![PathBuf::from("a")]);
        assert!(diff.modified.is_empty());
    }

    #[test]
    fn hash_catches_content_change_with_same_mtime_and_len() {
        let tmp = TempDir::new();
        let file = tmp.path().join("f.bin");
        write_file(&file, "aaaaaaaa");
        let w = watcher_of(tmp.path());
        let baseline = w.scan().unwrap();
        let before = baseline[Path::new("f.bin")];
        // Перезаписываем другим содержимым той же длины и ВОЗВРАЩАЕМ mtime
        // назад: ни mtime, ни len не отличаются — сработать должен только хэш.
        write_file(&file, "bbbbbbbb");
        let f = fs::File::options().write(true).open(&file).unwrap();
        f.set_modified(before.mtime).unwrap();
        let diff = w.watch_since(&baseline).unwrap();
        assert_eq!(diff.modified, vec![PathBuf::from("f.bin")]);
        let after = w.scan().unwrap()[Path::new("f.bin")];
        assert_eq!(after.mtime, before.mtime);
        assert_eq!(after.len, before.len);
        assert_ne!(after.hash64, before.hash64);
    }

    #[test]
    fn mtime_only_change_is_reported_as_modified() {
        // Консервативная семантика: touch без смены содержимого — тоже modified.
        let tmp = TempDir::new();
        let file = tmp.path().join("t.txt");
        write_file(&file, "content");
        let w = watcher_of(tmp.path());
        let baseline = w.scan().unwrap();
        let before = baseline[Path::new("t.txt")];
        let f = fs::File::options().write(true).open(&file).unwrap();
        f.set_modified(before.mtime + Duration::from_secs(1)).unwrap();
        let diff = w.watch_since(&baseline).unwrap();
        assert_eq!(diff.modified, vec![PathBuf::from("t.txt")]);
        assert!(diff.added.is_empty() && diff.deleted.is_empty());
    }

    #[test]
    fn watcher_clamps_zero_interval() {
        let tmp = TempDir::new();
        assert_eq!(Watcher::new(tmp.path(), Duration::ZERO).interval(), MIN_INTERVAL);
        let d = Duration::from_millis(30);
        assert_eq!(Watcher::new(tmp.path(), d).interval(), d);
        assert_eq!(Watcher::new(tmp.path(), d).root(), tmp.path());
    }

    #[test]
    fn guard_passes_when_file_untouched() {
        let tmp = TempDir::new();
        let file = tmp.path().join("code.rs");
        write_file(&file, "fn a() {}");
        let guard = EditGuard::capture(&file).unwrap();
        assert!(guard.before().is_some());
        assert_eq!(guard.path(), file.as_path());
        guard.check().unwrap();
    }

    #[test]
    fn guard_conflict_on_external_edit() {
        let tmp = TempDir::new();
        let file = tmp.path().join("code.rs");
        write_file(&file, "fn a() {}");
        let guard = EditGuard::capture(&file).unwrap();
        write_file(&file, "fn a() { println!(\"user edit\"); }");
        let err = guard.check().unwrap_err();
        assert!(err.is_conflict());
        assert_eq!(err.path(), file.as_path());
        assert!(err.to_string().contains("code.rs"));
    }

    #[test]
    fn guard_conflict_on_external_delete_and_create() {
        let tmp = TempDir::new();
        let file = tmp.path().join("code.rs");
        // Удаление: было Some → стало None.
        write_file(&file, "x");
        let guard = EditGuard::capture(&file).unwrap();
        fs::remove_file(&file).unwrap();
        assert!(guard.check().unwrap_err().is_conflict());
        // Появление: было None → стало Some.
        let guard2 = EditGuard::capture(&file).unwrap();
        assert!(guard2.before().is_none());
        guard2.check().unwrap(); // файла всё ещё нет — чисто
        write_file(&file, "externally created");
        assert!(guard2.check().unwrap_err().is_conflict());
    }

    #[test]
    fn guard_write_if_unchanged_writes_and_refresh_rebases() {
        let tmp = TempDir::new();
        let file = tmp.path().join("out.txt");
        write_file(&file, "old");
        let mut guard = EditGuard::capture(&file).unwrap();
        guard.write_if_unchanged(|p| fs::write(p, "new")).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
        // После собственной записи refresh перебазирует сигнатуру —
        // следующая проверка снова чистая.
        guard.refresh().unwrap();
        guard.check().unwrap();
    }

    #[test]
    fn guard_write_blocked_on_conflict() {
        let tmp = TempDir::new();
        let file = tmp.path().join("out.txt");
        write_file(&file, "original");
        let guard = EditGuard::capture(&file).unwrap();
        write_file(&file, "user rewrote");
        let err = guard.write_if_unchanged(|p| fs::write(p, "model version")).unwrap_err();
        assert!(err.is_conflict());
        // Запись НЕ выполнилась — правка пользователя не потеряна.
        assert_eq!(fs::read_to_string(&file).unwrap(), "user rewrote");
    }

    #[cfg(unix)]
    #[test]
    fn guard_io_error_is_not_a_conflict() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let dir = tmp.path().join("locked");
        write_file(&dir.join("f.txt"), "x");
        let guard = EditGuard::capture(dir.join("f.txt")).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
        let err = guard.check().unwrap_err();
        // Возвращаем права, чтобы Drop смог прибрать каталог.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!err.is_conflict());
        assert!(err.source().is_some());
    }

    #[test]
    fn watch_handle_reports_change_and_stops_cleanly() {
        let tmp = TempDir::new();
        let file = tmp.path().join("watched.txt");
        write_file(&file, "v1");
        let (tx, rx) = mpsc::channel();
        let mut handle = WatchHandle::start(watcher_of(tmp.path()), move |diff| {
            tx.send(diff).unwrap();
        });
        assert!(handle.is_running());
        // Даём потоку снять базовый снимок до изменения файла.
        thread::sleep(Duration::from_millis(100));
        write_file(&file, "v2 with new content");
        let diff = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(diff.modified, vec![PathBuf::from("watched.txt")]);
        handle.stop();
        assert!(!handle.is_running());
        // Повторный stop безопасен.
        handle.stop();
    }

    #[test]
    fn watch_handle_silent_without_changes() {
        let tmp = TempDir::new();
        write_file(&tmp.path().join("a.txt"), "static");
        let fires = Arc::new(AtomicUsize::new(0));
        let fires2 = Arc::clone(&fires);
        let mut handle = WatchHandle::start(watcher_of(tmp.path()), move |_diff| {
            fires2.fetch_add(1, Ordering::Relaxed);
        });
        thread::sleep(Duration::from_millis(250));
        assert_eq!(fires.load(Ordering::Relaxed), 0);
        handle.stop();
        assert!(!handle.is_running());
    }

    #[test]
    fn watch_handle_drop_stops_thread() {
        let tmp = TempDir::new();
        let handle = WatchHandle::start(watcher_of(tmp.path()), |_diff| {});
        assert!(handle.is_running());
        // Drop обязан остановить и присоединить поток: если бы он «зависал»,
        // тест не завершился бы.
        drop(handle);
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_symlinks_and_does_not_loop() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new();
        write_file(&tmp.path().join("real.txt"), "data");
        // Ссылка на родителя: наивный обход ушёл бы в бесконечный цикл.
        symlink(tmp.path(), tmp.path().join("loop")).unwrap();
        symlink(tmp.path().join("real.txt"), tmp.path().join("alias.txt")).unwrap();
        let snap = watcher_of(tmp.path()).scan().unwrap();
        assert_eq!(snap.len(), 1);
        assert!(snap.contains_key(Path::new("real.txt")));
    }
}
