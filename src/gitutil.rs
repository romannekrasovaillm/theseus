//! Git-утилиты харнесса поверх CLI `git` (образец — `codex-rs/git-utils`).
//!
//! Лёгкая синхронная обёртка вокруг системного бинарника `git`: никаких
//! привязок к libgit2, только `std::process::Command`. Харнессу git нужен
//! для контекста сессии — текущая ветка, «грязность» рабочего дерева, сводка
//! статуса, последние коммиты, размер незакоммиченного diff'а; всё это
//! собирает [`GitRepo`].
//!
//! Принципы (по урокам codex):
//!
//! * **Таймаут у каждого вызова.** Любой запуск git ограничен [`GIT_TIMEOUT`]
//!   (2 с): зависшая сетевая ФС или гигантский репозиторий не должны вешать
//!   агента. Реализация — паттерн «потоки + дедлайн»: stdout/stderr
//!   вычитывают потоки-насосы (без них git с выводом больше pipe-буфера
//!   заблокировался бы на `write`, и мы получили бы ложный таймаут), а
//!   вызывающий поток опрашивает `try_wait` до дедлайна. По таймауту процесс
//!   убивается и дожидается (`wait` — чтобы не оставлять зомби).
//! * **Детерминированный текст.** `%cr` в `git log` переводится gettext'ом
//!   (при русской локали — «2 часа назад»), поэтому каждый вызов идёт с
//!   `LC_ALL=C` — форматы, которые мы разбираем, стабильны.
//! * **Изоляция от окружения.** Из окружения вызова вычищаются `GIT_DIR` и
//!   `GIT_WORK_TREE` (иначе git может молча уйти в чужой репозиторий) и
//!   выставляется `GIT_TERMINAL_PROMPT=0` (git никогда не ждёт ввод пароля —
//!   харнесс не интерактивен).
//! * **Никаких паник.** Все методы возвращают `Option` или пустые значения
//!   при любой ошибке git (нет бинарника, не репозиторий, таймаут):
//!   git-контекст для агента — nice-to-have, а не повод падать.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Таймаут одного вызова `git`: агент не должен виснуть на медленной ФС.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Пауза между опросами `try_wait` в цикле ожидания с дедлайном.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Сводка `git status`: расхождение с upstream и число изменённых файлов.
///
/// Собирается разбором `git status --porcelain=v2 --branch`
/// ([`GitRepo::status_summary`]); при любом сбое git возвращается
/// `Default` (все нули).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatus {
    /// Коммитов впереди upstream (`+A` в заголовке `# branch.ab`).
    /// Ненулевое только при настроенном upstream.
    pub ahead: u64,
    /// Коммитов позади upstream (`-B` в заголовке `# branch.ab`).
    pub behind: u64,
    /// Число изменённых отслеживаемых файлов: обычные правки (строки `1 `),
    /// переименования (`2 `) и неразрешённые конфликты слияния (`u `),
    /// staged и unstaged вместе.
    pub modified: u64,
    /// Число неотслеживаемых файлов (строки `? `). Игнорируемые gitignore'ом
    /// файлы не считаются (`--ignored` не запрашивается).
    pub untracked: u64,
}

/// Одна запись `git log` в человекочитаемом виде.
///
/// Заполняется [`GitRepo::recent_log`] из формата `%h%x1f%s%x1f%cr`:
/// поля режутся по unit separator'у, поэтому subject с пробелами и
/// двоеточиями разбор не ломает.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Короткий хэш коммита (`%h`).
    pub hash: String,
    /// Первая строка сообщения коммита (`%s`).
    pub subject: String,
    /// Относительный возраст коммита (`%cr`, напр. «2 hours ago»).
    /// Текст зависит от git и не предназначен для машинного разбора.
    pub ago: String,
}

/// Контекст git-репозитория: корень + методы-запросы к CLI git.
///
/// Создаётся только через [`GitRepo::discover`], поэтому `root` всегда
/// указывает на настоящий корень репозитория на момент обнаружения.
/// Все методы не паникуют: любой сбой git отображается в `Option`/пустое
/// значение.
#[derive(Debug, Clone)]
pub struct GitRepo {
    /// Абсолютный путь к корню рабочего дерева (вывод `rev-parse --show-toplevel`).
    root: PathBuf,
}

/// Сырой результат вызова git: признак успешного кода выхода и stdout.
/// stderr вычитывается (чтобы не блокировать git), но вызывающим не нужен.
struct GitOutput {
    success: bool,
    stdout: String,
}

/// Путь к git-бинарнику: обычно просто `"git"` (резолв через PATH), но тесты
/// могут подменить его через `THESEUS_GIT_BIN` — так подмена не задевает
/// остальные тесты процесса (в отличие от перезаписи PATH, которая ломает
/// спавн `bash`/`python3` в соседних модулях).
fn git_binary() -> std::ffi::OsString {
    std::env::var_os("THESEUS_GIT_BIN").unwrap_or_else(|| "git".into())
}

/// Запустить `git <args>` в каталоге `cwd` и собрать stdout с таймаутом
/// [`GIT_TIMEOUT`].
///
/// `None` — если git не запустился (нет бинарника, неисполняемый файл в
/// `PATH`), упал при ожидании или был убит по таймауту. Ненулевой код выхода
/// — не `None`, а `success == false`: решает вызывающий.
fn run_git(args: &[&str], cwd: &Path) -> Option<GitOutput> {
    let mut child = Command::new(git_binary())
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // чужие GIT_DIR/GIT_WORK_TREE уводят git в другой репозиторий
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        // харнесс не интерактивен — никогда не спрашивать учётку
        .env("GIT_TERMINAL_PROMPT", "0")
        // %cr и человекочитаемые сводки переводятся gettext'ом
        .env("LC_ALL", "C")
        .spawn()
        .ok()?;

    // Потоки-насосы: без вычитки вывод больше pipe-буфера (64 КБ) повесил бы
    // git на `write`, и цикл ниже убил бы живой процесс по таймауту.
    let mut out_pipe = child.stdout.take()?;
    let mut err_pipe = child.stderr.take()?;
    let out_pump = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_pump = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });

    // Опрос с дедлайном: try_wait + короткая пауза до GIT_TIMEOUT.
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) | Err(_) => {
                // таймаут или сбой ожидания: убиваем и дожидаемся ребёнка,
                // иначе останется зомби до конца процесса харнесса
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };

    // После завершения (или убийства) процесса насосы видят EOF и сами
    // заканчиваются — дожидаемся их и забираем накопленный stdout.
    let stdout = out_pump.join().ok()?;
    let _ = err_pump.join();
    let status = status?;
    Some(GitOutput {
        success: status.success(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
    })
}

/// `git <args>` с нулевым кодом выхода → stdout; любой сбой → `None`.
fn git_stdout(args: &[&str], cwd: &Path) -> Option<String> {
    let out = run_git(args, cwd)?;
    out.success.then_some(out.stdout)
}

impl GitRepo {
    /// Найти репозиторий, начиная от пути `from` (каталог или файл внутри
    /// него): git сам поднимается вверх по дереву каталогов
    /// (`rev-parse --show-toplevel`). Если `from` — файл, стартуем с его
    /// родительского каталога.
    ///
    /// `None` — вне репозитория, при несуществующем пути, недоступном или
    /// зависшем git. Голый (bare) репозиторий не считается находкой:
    /// рабочего дерева у него нет, `--show-toplevel` падает.
    pub fn discover(from: &Path) -> Option<Self> {
        let start = if from.is_file() { from.parent()? } else { from };
        let out = git_stdout(&["rev-parse", "--show-toplevel"], start)?;
        let root = PathBuf::from(out.trim());
        if root.as_os_str().is_empty() {
            return None;
        }
        Some(Self { root })
    }

    /// Корень репозитория, найденный [`GitRepo::discover`].
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Имя текущей ветки (`symbolic-ref --short HEAD`).
    ///
    /// На unborn HEAD (репозиторий без коммитов) symbolic-ref честно отвечает
    /// именем будущей ветки. На detached HEAD имени нет — возвращается
    /// короткий хэш коммита (`rev-parse --short HEAD`). `None` — только если
    /// git не смог ответить вовсе.
    pub fn current_branch(&self) -> Option<String> {
        if let Some(name) = git_stdout(&["symbolic-ref", "--quiet", "--short", "HEAD"], &self.root)
        {
            let name = name.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
        // detached HEAD: ветки нет — отдаём короткий хэш как идентификатор
        let sha = git_stdout(&["rev-parse", "--short", "HEAD"], &self.root)?;
        let sha = sha.trim();
        if sha.is_empty() {
            None
        } else {
            Some(sha.to_string())
        }
    }

    /// true, если есть незакоммиченные изменения отслеживаемых файлов или
    /// неотслеживаемые файлы (`git status --porcelain` непуст).
    ///
    /// Ошибка git трактуется как «чисто»: нет данных — нет предупреждения.
    /// Для деталей (ahead/behind, счётчики по видам) см.
    /// [`GitRepo::status_summary`].
    pub fn is_dirty(&self) -> bool {
        git_stdout(&["status", "--porcelain"], &self.root)
            .is_some_and(|out| !out.trim().is_empty())
    }

    /// Сводка статуса из `git status --porcelain=v2 --branch`.
    ///
    /// Формат v2 стабилен и не локализуется: заголовки `# branch.*`, затем
    /// по строке на запись — `1 ` обычное изменение, `2 ` переименование,
    /// `u ` конфликт слияния, `? ` неотслеживаемый файл, `! ` игнорируемый
    /// (без `--ignored` не встречается). ahead/behind берутся из заголовка
    /// `# branch.ab +A -B`, который git печатает только при настроенном
    /// upstream. Любой сбой git → [`GitStatus::default`].
    pub fn status_summary(&self) -> GitStatus {
        let mut status = GitStatus::default();
        let Some(out) = git_stdout(
            &["status", "--porcelain=v2", "--branch", "--untracked-files=normal"],
            &self.root,
        ) else {
            return status;
        };
        for line in out.lines() {
            if let Some(ab) = line.strip_prefix("# branch.ab ") {
                // формат строго "+<ahead> -<behind>"
                let mut parts = ab.split_whitespace();
                if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
                    status.ahead = a.trim_start_matches('+').parse().unwrap_or(0);
                    status.behind = b.trim_start_matches('-').parse().unwrap_or(0);
                }
            } else if line.starts_with("? ") {
                status.untracked += 1;
            } else if line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("u ") {
                status.modified += 1;
            }
            // остальные заголовки `# branch.*` для сводки не нужны
        }
        status
    }

    /// До `n` последних коммитов, новые первыми (как `git log`).
    ///
    /// Формат записи `%h%x1f%s%x1f%cr`: поля разделены unit separator'ом,
    /// записи — переводами строк (subject по определению однострочный).
    /// `log.showSignature` принудительно выключается — иначе при
    /// пользовательском конфиге gpg-вывод вклинился бы между записями.
    /// Репозиторий без коммитов или любой сбой git → пустой вектор.
    pub fn recent_log(&self, n: usize) -> Vec<LogEntry> {
        if n == 0 {
            return Vec::new();
        }
        let max_count = format!("--max-count={n}");
        let Some(out) = git_stdout(
            &[
                "-c",
                "log.showSignature=false",
                "log",
                &max_count,
                "--pretty=format:%h%x1f%s%x1f%cr",
            ],
            &self.root,
        ) else {
            return Vec::new();
        };
        out.lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\u{1f}');
                let hash = parts.next()?.trim();
                let subject = parts.next()?;
                let ago = parts.next()?.trim();
                if hash.is_empty() {
                    return None;
                }
                Some(LogEntry {
                    hash: hash.to_string(),
                    subject: subject.to_string(),
                    ago: ago.to_string(),
                })
            })
            .collect()
    }

    /// Статистика незастейдженного diff'а: `(файлов, добавлено строк,
    /// удалено строк)` — разбор `git diff --shortstat`.
    ///
    /// Это сводка «рабочее дерево против индекса»: staged-изменения и
    /// untracked-файлы `git diff` без аргументов не показывает. Чистое
    /// дерево → `Some((0, 0, 0))`, сбой git → `None`.
    pub fn diff_stat(&self) -> Option<(u64, u64, u64)> {
        let out = git_stdout(&["diff", "--shortstat"], &self.root)?;
        Some(parse_shortstat(&out))
    }
}

/// Разбор вывода `git diff --shortstat`:
/// « 3 files changed, 10 insertions(+), 2 deletions(-)». Части могут
/// отсутствовать (нет удалений — нет третьей части), единственное число —
/// без «s» («1 file changed»). Пустой вывод (чистое дерево) → `(0, 0, 0)`.
fn parse_shortstat(text: &str) -> (u64, u64, u64) {
    let mut files = 0;
    let mut add = 0;
    let mut del = 0;
    for part in text.split(',') {
        let part = part.trim();
        let Some(num) = part
            .split_whitespace()
            .next()
            .and_then(|word| word.parse::<u64>().ok())
        else {
            continue;
        };
        if part.contains("file") {
            files = num;
        } else if part.contains("insertion") {
            add = num;
        } else if part.contains("deletion") {
            del = num;
        }
    }
    (files, add, del)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Все тесты модуля сериализованы одним мьютексом: cargo test гоняет их
    /// параллельными потоками, а тесты с «битым git» временно подменяют PATH
    /// всего процесса — без сериализации соседние тесты ловили бы флаки
    /// (их git внезапно переставал бы запускаться).
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_lock() -> MutexGuard<'static, ()> {
        // восстанавливаемся из poison: паника одного теста не должна
        // каскадом валить остальные
        TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Минимальный tempdir без внешних крейтов (паттерн filewatcher.rs):
    /// уникальное имя + чистка в Drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("theseus-gitutil-{pid}-{n}-{nanos}-{tag}"));
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

    /// Подмена git-бинарника через THESEUS_GIT_BIN с автовосстановлением —
    /// даже при панике в тесте. PATH не трогаем: его перезапись процессно-глобальна
    /// и ломала спавн bash/python3 в тестах соседних модулей (hooks_ext, mcp_ext).
    struct GitBinGuard(Option<OsString>);

    impl GitBinGuard {
        /// Перенаправить `git_binary()` на скрипт `dir/git`.
        fn set(dir: &Path) -> Self {
            let old = std::env::var_os("THESEUS_GIT_BIN");
            std::env::set_var("THESEUS_GIT_BIN", dir.join("git"));
            Self(old)
        }
    }

    impl Drop for GitBinGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(old) => std::env::set_var("THESEUS_GIT_BIN", old),
                None => std::env::remove_var("THESEUS_GIT_BIN"),
            }
        }
    }

    /// Прогнать `git <args>` в каталоге; в тестах паника при сбое уместна.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "git {args:?} failed: {stderr}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Репозиторий с одним коммитом: init (ветка main) + локальный user +
    /// файл `file.txt` («один\nдва\nтри\n») + коммит «первый коммит».
    fn init_repo(tag: &str) -> TempDir {
        let dir = TempDir::new(tag);
        git(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
        git(dir.path(), &["config", "user.name", "Theseus Test"]);
        git(dir.path(), &["config", "user.email", "theseus@example.com"]);
        // страховка от экзотики глобального конфига машины
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        git(dir.path(), &["config", "core.hooksPath", "/dev/null"]);
        fs::write(dir.path().join("file.txt"), "один\nдва\nтри\n").unwrap();
        git(dir.path(), &["add", "file.txt"]);
        git(dir.path(), &["commit", "--quiet", "-m", "первый коммит"]);
        dir
    }

    /// Исполняемый shell-скрипт `git` с заданным телом в отдельном temp-каталоге.
    fn fake_git(body: &str) -> TempDir {
        let dir = TempDir::new("fake-git");
        let script = dir.path().join("git");
        fs::write(&script, body).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    /// Канонические пути для сравнения (temp-каталоги могут быть симлинками).
    fn same_path(a: &Path, b: &Path) -> bool {
        fs::canonicalize(a).unwrap() == fs::canonicalize(b).unwrap()
    }

    #[test]
    fn discover_finds_root_from_subdirectory() {
        let _guard = test_lock();
        let repo = init_repo("discover-subdir");
        let nested = repo.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        let found = GitRepo::discover(&nested).unwrap();
        assert!(same_path(found.root(), repo.path()));
    }

    #[test]
    fn discover_accepts_file_inside_repo() {
        let _guard = test_lock();
        let repo = init_repo("discover-file");
        let file = repo.path().join("file.txt");
        let found = GitRepo::discover(&file).unwrap();
        assert!(same_path(found.root(), repo.path()));
    }

    #[test]
    fn discover_outside_repo_returns_none() {
        let _guard = test_lock();
        let plain = TempDir::new("not-a-repo");
        assert!(GitRepo::discover(plain.path()).is_none());
        // несуществующий путь — тоже None, без паники
        assert!(GitRepo::discover(&plain.path().join("нет/такого/каталога")).is_none());
    }

    #[test]
    fn discover_with_broken_git_returns_none() {
        let _guard = test_lock();
        let repo = init_repo("broken-git");
        // git, падающий с кодом 1
        let failing = fake_git("#!/bin/sh\nexit 1\n");
        {
            let _path = GitBinGuard::set(failing.path());
            assert!(GitRepo::discover(repo.path()).is_none());
        }
        // git, который вообще не исполняемый — spawn не состоится
        let not_exec = TempDir::new("not-exec-git");
        fs::write(not_exec.path().join("git"), "#!/bin/sh\nexit 1\n").unwrap();
        {
            let _path = GitBinGuard::set(not_exec.path());
            assert!(GitRepo::discover(repo.path()).is_none());
        }
        // git, «успешно» молчащий: пустой toplevel тоже отбрасываем
        let silent = fake_git("#!/bin/sh\nexit 0\n");
        {
            let _path = GitBinGuard::set(silent.path());
            assert!(GitRepo::discover(repo.path()).is_none());
        }
    }

    #[test]
    fn discover_kills_hanging_git_by_timeout() {
        let _guard = test_lock();
        let repo = init_repo("hanging-git");
        // sleep по абсолютному пути: бинарник подменён через THESEUS_GIT_BIN,
        // внешние команды скрипту не нужны — sh завершился бы мгновенно
        // вместо зависания без exec /bin/sleep
        let hanging = fake_git("#!/bin/sh\nexec /bin/sleep 30\n");
        let _path = GitBinGuard::set(hanging.path());
        let started = Instant::now();
        assert!(GitRepo::discover(repo.path()).is_none());
        let elapsed = started.elapsed();
        // git убит по GIT_TIMEOUT (2 с), а не дождавшись sleep 30
        assert!(elapsed >= GIT_TIMEOUT, "вернулся подозрительно рано: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(15), "не убился по таймауту: {elapsed:?}");
    }

    #[test]
    fn current_branch_reports_branch_name() {
        let _guard = test_lock();
        let repo = init_repo("branch");
        let git_repo = GitRepo::discover(repo.path()).unwrap();
        assert_eq!(git_repo.current_branch().as_deref(), Some("main"));
    }

    #[test]
    fn current_branch_detached_returns_short_hash() {
        let _guard = test_lock();
        let repo = init_repo("detached");
        let sha = git(repo.path(), &["rev-parse", "--short", "HEAD"]);
        git(repo.path(), &["checkout", "--quiet", "--detach", "HEAD"]);
        let git_repo = GitRepo::discover(repo.path()).unwrap();
        assert_eq!(git_repo.current_branch().as_deref(), Some(sha.trim()));
    }

    #[test]
    fn methods_work_on_repo_without_commits() {
        let _guard = test_lock();
        let unborn = TempDir::new("unborn");
        git(unborn.path(), &["init", "--quiet", "--initial-branch=main"]);
        let git_repo = GitRepo::discover(unborn.path()).unwrap();
        // unborn HEAD: symbolic-ref знает имя будущей ветки
        assert_eq!(git_repo.current_branch().as_deref(), Some("main"));
        // остальные методы отвечают пустыми значениями, не паникой
        assert!(git_repo.recent_log(5).is_empty());
        assert!(!git_repo.is_dirty());
        assert_eq!(git_repo.status_summary(), GitStatus::default());
        assert_eq!(git_repo.diff_stat(), Some((0, 0, 0)));
    }

    #[test]
    fn is_dirty_false_on_clean_repo() {
        let _guard = test_lock();
        let repo = init_repo("clean");
        assert!(!GitRepo::discover(repo.path()).unwrap().is_dirty());
    }

    #[test]
    fn is_dirty_true_with_untracked_file() {
        let _guard = test_lock();
        let repo = init_repo("dirty-untracked");
        fs::write(repo.path().join("новый.txt"), "данные").unwrap();
        assert!(GitRepo::discover(repo.path()).unwrap().is_dirty());
    }

    #[test]
    fn is_dirty_true_with_modified_tracked_file() {
        let _guard = test_lock();
        let repo = init_repo("dirty-modified");
        fs::write(repo.path().join("file.txt"), "перезаписано\n").unwrap();
        assert!(GitRepo::discover(repo.path()).unwrap().is_dirty());
    }

    #[test]
    fn status_summary_counts_modified_and_untracked() {
        let _guard = test_lock();
        let repo = init_repo("status-counts");
        fs::write(repo.path().join("file.txt"), "изменено\n").unwrap(); // tracked-правка
        fs::write(repo.path().join("u1.txt"), "1").unwrap();
        fs::write(repo.path().join("u2.txt"), "2").unwrap();
        let status = GitRepo::discover(repo.path()).unwrap().status_summary();
        assert_eq!(status.modified, 1);
        assert_eq!(status.untracked, 2);
        // без upstream расхождения нет
        assert_eq!(status.ahead, 0);
        assert_eq!(status.behind, 0);
    }

    #[test]
    fn status_summary_tracks_ahead_and_behind() {
        let _guard = test_lock();
        let origin = init_repo("status-origin");
        let host = TempDir::new("status-clone");
        let clone = host.path().join("work");
        let origin_str = origin.path().to_str().unwrap();
        git(host.path(), &["clone", "--quiet", origin_str, "work"]);
        git(&clone, &["config", "user.name", "Theseus Test"]);
        git(&clone, &["config", "user.email", "theseus@example.com"]);
        git(&clone, &["config", "commit.gpgsign", "false"]);
        git(&clone, &["config", "core.hooksPath", "/dev/null"]);

        // коммит в клоне, не ушедший в origin → ahead 1
        fs::write(clone.join("ahead.txt"), "вперёд").unwrap();
        git(&clone, &["add", "ahead.txt"]);
        git(&clone, &["commit", "--quiet", "-m", "коммит в клоне"]);
        let status = GitRepo::discover(&clone).unwrap().status_summary();
        assert_eq!(status.ahead, 1);
        assert_eq!(status.behind, 0);

        // коммит в origin + fetch в клоне → расхождение: ahead 1, behind 1
        fs::write(origin.path().join("behind.txt"), "назад").unwrap();
        git(origin.path(), &["add", "behind.txt"]);
        git(origin.path(), &["commit", "--quiet", "-m", "коммит в origin"]);
        git(&clone, &["fetch", "--quiet", "origin"]);
        let status = GitRepo::discover(&clone).unwrap().status_summary();
        assert_eq!(status.ahead, 1);
        assert_eq!(status.behind, 1);
    }

    #[test]
    fn recent_log_returns_entries_newest_first() {
        let _guard = test_lock();
        let repo = init_repo("log-order");
        fs::write(repo.path().join("second.txt"), "2").unwrap();
        git(repo.path(), &["add", "second.txt"]);
        git(repo.path(), &["commit", "--quiet", "-m", "второй: добавил second.txt"]);
        let log = GitRepo::discover(repo.path()).unwrap().recent_log(10);
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].subject, "второй: добавил second.txt");
        assert_eq!(log[1].subject, "первый коммит");
        // короткий хэш совпадает с rev-parse, возраст заполнен
        let head = git(repo.path(), &["rev-parse", "--short", "HEAD"]);
        assert_eq!(log[0].hash, head.trim());
        assert!(!log[0].ago.is_empty());
        assert!(!log[1].ago.is_empty());
    }

    #[test]
    fn recent_log_respects_limit() {
        let _guard = test_lock();
        let repo = init_repo("log-limit");
        for i in 0..3 {
            let name = format!("f{i}.txt");
            fs::write(repo.path().join(name), "x").unwrap();
            git(repo.path(), &["add", "."]);
            let msg = format!("коммит {i}");
            git(repo.path(), &["commit", "--quiet", "-m", &msg]);
        }
        let log = GitRepo::discover(repo.path()).unwrap().recent_log(2);
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].subject, "коммит 2");
        assert_eq!(log[1].subject, "коммит 1");
        // n = 0 — пустой вектор без вызова git
        assert!(GitRepo::discover(repo.path()).unwrap().recent_log(0).is_empty());
    }

    #[test]
    fn diff_stat_counts_additions_and_deletions() {
        let _guard = test_lock();
        let repo = init_repo("diff-counts");
        // «два» → «ДВА» (1 del + 1 add) и две новые строки (2 add)
        fs::write(repo.path().join("file.txt"), "один\nДВА\nтри\nчетыре\nпять\n").unwrap();
        let stat = GitRepo::discover(repo.path()).unwrap().diff_stat();
        assert_eq!(stat, Some((1, 3, 1)));
    }

    #[test]
    fn diff_stat_clean_repo_returns_zeros() {
        let _guard = test_lock();
        let repo = init_repo("diff-clean");
        // untracked-файл git diff не виден — статистика остаётся нулевой
        fs::write(repo.path().join("untracked.txt"), "x").unwrap();
        let stat = GitRepo::discover(repo.path()).unwrap().diff_stat();
        assert_eq!(stat, Some((0, 0, 0)));
    }

    #[test]
    fn parse_shortstat_handles_partial_forms() {
        let _guard = test_lock();
        assert_eq!(parse_shortstat(""), (0, 0, 0));
        assert_eq!(parse_shortstat(" 1 file changed, 1 insertion(+)\n"), (1, 1, 0));
        assert_eq!(parse_shortstat(" 2 files changed, 3 deletions(-)\n"), (2, 0, 3));
        assert_eq!(
            parse_shortstat(" 5 files changed, 10 insertions(+), 4 deletions(-)\n"),
            (5, 10, 4)
        );
    }

    #[test]
    fn methods_degrade_gracefully_when_repo_vanishes() {
        let _guard = test_lock();
        let repo = init_repo("vanished");
        let git_repo = GitRepo::discover(repo.path()).unwrap();
        // выносим .git — репозиторий «исчез» из-под живого GitRepo
        fs::remove_dir_all(repo.path().join(".git")).unwrap();
        assert!(git_repo.current_branch().is_none());
        assert!(!git_repo.is_dirty());
        assert_eq!(git_repo.status_summary(), GitStatus::default());
        assert!(git_repo.recent_log(3).is_empty());
        assert!(git_repo.diff_stat().is_none());
    }
}
