//! Снапшот и трекинг shell-окружения агента.
//!
//! Модуль состоит из двух частей (образец — codex `shell-command` и задача
//! `user-shell`: агент должен видеть, чем окружение после хода отличается от
//! окружения до него, и выполнять команды в живом shell):
//!
//! 1. [`ShellEnv`] — снапшот окружения (переменные, cwd, путь к shell).
//!    Умеет сниматься с текущего процесса ([`ShellEnv::capture`]), сравниваться
//!    с предыдущим снапшотом ([`ShellEnv::diff`] → [`EnvDiff`]) и применяться
//!    к произвольной команде ([`ShellEnv::apply_to`]).
//!
//! 2. [`PersistentShell`] — долгоживущий shell-процесс по схеме coproc:
//!    stdin — пайп, stdout/stderr перекачиваются потоками-насосами в каналы,
//!    конец каждой команды помечается уникальным маркером с кодом выхода.
//!    Благодаря этому `cd`, `export`, функции и alias'ы переживают вызовы
//!    [`PersistentShell::exec`], а cwd отслеживается после каждой команды.
//!
//! Почему `bash --norc` без `-i`: интерактивный bash при stdin-пайпе печатает
//! в stderr приглашение PS1 и эхо каждой прочитанной строки, что засоряет
//! stderr пользовательской команды. Неинтерактивный bash читает команды из
//! пайпа по мере поступления и не шумит. rc-файлы не читаем (`--norc`):
//! окружение агента должно быть воспроизводимым. Если bash недоступен,
//! берётся `sh` — маркерный протокол POSIX-совместим.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// Префикс маркера кода выхода в stdout: `THESEUS_EC_<код>_<токен>`.
const EC_MARKER_PREFIX: &str = "THESEUS_EC_";
/// Префикс маркера конца stderr-блока: `THESEUS_DONE_<токен>`.
const DONE_MARKER_PREFIX: &str = "THESEUS_DONE_";

/// Таймаут [`PersistentShell::exec`] по умолчанию. Длинные команды агент
/// должен уводить в фоновые задачи, а не держать синхронный exec часами.
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(120);

/// Счётчик для генератора токенов маркеров.
static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Уникальный токен маркера: hex(pid) + hex(счётчик). Только hex-символы —
/// токен вставляется в shell-строку без экранирования и не содержит `_`,
/// поэтому парсер может резать маркер по первому подчёркиванию.
fn next_token() -> String {
    let pid = std::process::id();
    let n = TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{pid:x}{n:x}")
}

/// Определяет путь к shell: `$SHELL` (если файл существует) → bash → `/bin/sh`.
fn detect_shell_path(vars: &BTreeMap<String, String>) -> PathBuf {
    if let Some(sh) = vars.get("SHELL") {
        if !sh.is_empty() && Path::new(sh).exists() {
            return PathBuf::from(sh);
        }
    }
    for candidate in ["/bin/bash", "/usr/bin/bash", "/bin/sh"] {
        if Path::new(candidate).exists() {
            return PathBuf::from(candidate);
        }
    }
    PathBuf::from("/bin/sh")
}

/// true, если имя исполняемого файла похоже на bash (нужен флаг `--norc`).
fn is_bash(shell_path: &Path) -> bool {
    shell_path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().contains("bash"))
}

/// Снапшот shell-окружения: переменные, рабочий каталог, путь к shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellEnv {
    /// Переменные окружения (детерминированно отсортированы по имени).
    pub vars: BTreeMap<String, String>,
    /// Рабочий каталог в момент снятия снапшота.
    pub cwd: PathBuf,
    /// Путь к исполняемому файлу shell.
    pub shell_path: PathBuf,
}

impl ShellEnv {
    /// Снимает снапшот окружения ТЕКУЩЕГО процесса.
    ///
    /// Не-UTF8 переменные конвертируются lossy (паники, в отличие от
    /// `std::env::vars`, нет). Если cwd определить не удалось (например,
    /// каталог удалён из-под процесса), используется `/`.
    pub fn capture() -> Self {
        let vars = std::env::vars_os()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
            .collect();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let shell_path = detect_shell_path(&vars);
        ShellEnv { vars, cwd, shell_path }
    }

    /// Вычисляет разницу ЭТОГО (более нового) снапшота против `older`.
    pub fn diff(&self, older: &ShellEnv) -> EnvDiff {
        let mut diff = EnvDiff::default();
        for (name, value) in &self.vars {
            match older.vars.get(name) {
                None => {
                    diff.added.insert(name.clone(), value.clone());
                }
                Some(old_value) if old_value != value => {
                    diff.changed.insert(name.clone(), (old_value.clone(), value.clone()));
                }
                Some(_) => {}
            }
        }
        for (name, old_value) in &older.vars {
            if !self.vars.contains_key(name) {
                diff.removed.insert(name.clone(), old_value.clone());
            }
        }
        diff
    }

    /// Применяет снапшот к команде: окружение ПОЛНОСТЬЮ заменяется
    /// (`env_clear` + все переменные снапшота), выставляется cwd.
    ///
    /// Внимание: с очищенным `PATH` поиск программы по имени не сработает —
    /// вызывайте программу по абсолютному пути или держите `PATH` в снапшоте.
    pub fn apply_to(&self, cmd: &mut Command) {
        cmd.env_clear();
        cmd.envs(&self.vars);
        cmd.current_dir(&self.cwd);
    }
}

/// Разница двух снапшотов окружения (новый против старого).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvDiff {
    /// Появившиеся переменные: имя → новое значение.
    pub added: BTreeMap<String, String>,
    /// Исчезнувшие переменные: имя → прежнее значение.
    pub removed: BTreeMap<String, String>,
    /// Изменившиеся переменные: имя → (было, стало).
    pub changed: BTreeMap<String, (String, String)>,
}

impl EnvDiff {
    /// true, если различий нет.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    /// Однострочная сводка вида `+2 добавлено, -1 удалено, ~3 изменено`.
    pub fn summary(&self) -> String {
        if self.is_empty() {
            return "окружение не изменилось".to_string();
        }
        let mut parts = Vec::new();
        if !self.added.is_empty() {
            let n = self.added.len();
            parts.push(format!("+{n} добавлено"));
        }
        if !self.removed.is_empty() {
            let n = self.removed.len();
            parts.push(format!("-{n} удалено"));
        }
        if !self.changed.is_empty() {
            let n = self.changed.len();
            parts.push(format!("~{n} изменено"));
        }
        parts.join(", ")
    }
}

/// Результат одного [`PersistentShell::exec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    /// Накопленный stdout команды (служебные маркеры вырезаны).
    pub stdout: String,
    /// Накопленный stderr команды (служебные маркеры вырезаны).
    pub stderr: String,
    /// Код выхода (`$?` сразу после команды).
    pub code: i32,
}

impl Output {
    /// true, если код выхода равен нулю.
    pub fn success(&self) -> bool {
        self.code == 0
    }
}

/// Какой поток shell читаем (вместо булева флага).
#[derive(Debug, Clone, Copy)]
enum Pipe {
    Out,
    Err,
}

impl Pipe {
    fn name(self) -> &'static str {
        match self {
            Pipe::Out => "stdout",
            Pipe::Err => "stderr",
        }
    }
}

/// Причина, по которой строка из shell не получена.
#[derive(Debug, Clone, Copy)]
enum RecvFailure {
    Timeout,
    Closed,
}

/// Ждёт одну строку из канала, но не дольше дедлайна.
fn recv_line(rx: &Receiver<String>, deadline: Instant) -> Result<String, RecvFailure> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(RecvFailure::Timeout);
    }
    match rx.recv_timeout(remaining) {
        Ok(line) => Ok(line),
        Err(RecvTimeoutError::Timeout) => Err(RecvFailure::Timeout),
        Err(RecvTimeoutError::Disconnected) => Err(RecvFailure::Closed),
    }
}

/// Поток-насос: перекачивает строки из пайпа процесса в канал до EOF.
/// Читает непрерывно, чтобы ребёнок не заблокировался на полном пайпе.
fn pump_lines<R: Read>(reader: R, tx: Sender<String>) {
    let mut buf = BufReader::new(reader);
    let mut line = String::new();
    loop {
        match buf.read_line(&mut line) {
            // EOF или ошибка чтения — выходим; дроп tx закроет канал.
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if line.ends_with('\n') {
                    line.pop();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                }
                if tx.send(std::mem::take(&mut line)).is_err() {
                    break;
                }
            }
        }
    }
}

/// Ищет в строке stdout маркер `THESEUS_EC_<код>_<токен>`.
/// Возвращает (текст до маркера, код): текст нужен, когда вывод команды не
/// заканчивался переводом строки и маркер «приклеился» к последней строке.
/// Чужие или битые вхождения префикса пропускаются.
fn split_ec_marker(line: &str, token: &str) -> Option<(String, i32)> {
    let mut offset = 0;
    while let Some(rel) = line[offset..].find(EC_MARKER_PREFIX) {
        let idx = offset + rel;
        let after = &line[idx + EC_MARKER_PREFIX.len()..];
        let parsed = after
            .split_once('_')
            .filter(|(_, tok)| *tok == token)
            .and_then(|(code_str, _)| code_str.parse::<i32>().ok());
        if let Some(code) = parsed {
            return Some((line[..idx].to_string(), code));
        }
        offset = idx + EC_MARKER_PREFIX.len();
    }
    None
}

/// Ищет в строке stderr маркер `THESEUS_DONE_<токен>`; возвращает текст до него.
fn split_done_marker(line: &str, token: &str) -> Option<String> {
    let mut offset = 0;
    while let Some(rel) = line[offset..].find(DONE_MARKER_PREFIX) {
        let idx = offset + rel;
        if &line[idx + DONE_MARKER_PREFIX.len()..] == token {
            return Some(line[..idx].to_string());
        }
        offset = idx + DONE_MARKER_PREFIX.len();
    }
    None
}

/// Долгоживущий shell-процесс (схема coproc): состояние — cwd, export'ы,
/// функции — переживает вызовы [`exec`](PersistentShell::exec).
///
/// Протокол: после команды пользователя в shell отправляется служебный
/// «хвост» — subshell печатает в stdout маркер `THESEUS_EC_<$?>_<токен>`,
/// в stderr — маркер `THESEUS_DONE_<токен>`, затем `pwd` (в stdout) для
/// трекинга cwd. Токен уникален на каждый exec, поэтому случайное совпадение
/// с пользовательским выводом практически исключено. Хвост выполняется в
/// subshell, чтобы служебная переменная не протекала в окружение shell.
///
/// При таймауте процесс убивается (состояние теряется, кроме отслеживаемого
/// cwd) и прозрачно перезапускается при следующем exec.
///
/// Известное ограничение: команда с незакрытой кавычкой/скобкой «проглотит»
/// служебный хвост — такой exec завершится таймаутом.
#[derive(Debug)]
pub struct PersistentShell {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout_rx: Option<Receiver<String>>,
    stderr_rx: Option<Receiver<String>>,
    cwd: PathBuf,
    shell_path: PathBuf,
}

impl PersistentShell {
    /// Запускает shell по умолчанию (`$SHELL` → bash → `/bin/sh`) в текущем
    /// каталоге процесса.
    pub fn spawn() -> Result<Self> {
        let env = ShellEnv::capture();
        Self::with_options(&env.shell_path, &env.cwd)
    }

    /// Запускает конкретный shell в конкретном стартовом каталоге.
    pub fn with_options(shell_path: &Path, cwd: &Path) -> Result<Self> {
        let mut shell = PersistentShell {
            child: None,
            stdin: None,
            stdout_rx: None,
            stderr_rx: None,
            cwd: cwd.to_path_buf(),
            shell_path: shell_path.to_path_buf(),
        };
        shell.respawn()?;
        Ok(shell)
    }

    /// Текущий отслеживаемый рабочий каталог shell.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Путь к исполняемому файлу shell.
    pub fn shell_path(&self) -> &Path {
        &self.shell_path
    }

    /// Выполняет команду в персистентном shell с общим таймаутом.
    ///
    /// # Errors
    /// - истёк `timeout`: процесс убит, следующий exec перезапустит его
    ///   (export'ы и функции потеряны, cwd сохранён);
    /// - shell умер на середине команды (например, команда `exit`);
    /// - ошибка записи в stdin shell.
    pub fn exec(&mut self, command: &str, timeout: Duration) -> Result<Output> {
        self.ensure_alive()?;
        let token = next_token();
        let tail = format!(
            "( __theseus_ec=$?; printf '{EC_MARKER_PREFIX}%s_{token}\\n' \"$__theseus_ec\"; printf '{DONE_MARKER_PREFIX}{token}\\n' >&2; pwd )"
        );
        let payload = format!("{command}\n{tail}\n");
        {
            let stdin = self.stdin.as_mut().context("stdin shell недоступен")?;
            if let Err(e) = stdin.write_all(payload.as_bytes()).and_then(|()| stdin.flush()) {
                self.kill();
                return Err(anyhow!(e).context("не удалось отправить команду в shell"));
            }
        }
        let deadline = Instant::now() + timeout;

        // stdout — до маркера кода выхода.
        let mut stdout = String::new();
        let code = loop {
            let line = self.next_line(Pipe::Out, deadline, timeout)?;
            match split_ec_marker(&line, &token) {
                Some((before, code)) => {
                    stdout.push_str(&before);
                    break code;
                }
                None => {
                    stdout.push_str(&line);
                    stdout.push('\n');
                }
            }
        };

        // Следующая строка stdout — pwd для трекинга cwd.
        let pwd_line = self.next_line(Pipe::Out, deadline, timeout)?;
        if !pwd_line.is_empty() {
            self.cwd = PathBuf::from(pwd_line);
        }

        // stderr — до маркера конца.
        let mut stderr = String::new();
        loop {
            let line = self.next_line(Pipe::Err, deadline, timeout)?;
            match split_done_marker(&line, &token) {
                Some(before) => {
                    stderr.push_str(&before);
                    break;
                }
                None => {
                    stderr.push_str(&line);
                    stderr.push('\n');
                }
            }
        }

        Ok(Output { stdout, stderr, code })
    }

    /// Снимает снапшот окружения ПЕРСИСТЕНТНОГО shell (выполняет `env`).
    ///
    /// Переменные со значениями, содержащими переводы строк, парсятся
    /// некорректно — известное ограничение построчного формата `env`.
    pub fn snapshot_env(&mut self, timeout: Duration) -> Result<ShellEnv> {
        let out = self.exec("env", timeout)?;
        let mut vars = BTreeMap::new();
        for line in out.stdout.lines() {
            if let Some((name, value)) = line.split_once('=') {
                vars.insert(name.to_string(), value.to_string());
            }
        }
        Ok(ShellEnv { vars, cwd: self.cwd.clone(), shell_path: self.shell_path.clone() })
    }

    /// Одна строка из выбранного потока с учётом дедлайна.
    /// При сбое процесс убивается — каналы уже не восстановить.
    fn next_line(&mut self, pipe: Pipe, deadline: Instant, timeout: Duration) -> Result<String> {
        let what = pipe.name();
        let rx = match pipe {
            Pipe::Out => self.stdout_rx.as_ref(),
            Pipe::Err => self.stderr_rx.as_ref(),
        }
        .context("канал чтения shell закрыт")?;
        match recv_line(rx, deadline) {
            Ok(line) => Ok(line),
            Err(RecvFailure::Timeout) => {
                self.kill();
                Err(anyhow!(
                    "таймаут {timeout:?} при чтении {what} shell; процесс убит и будет перезапущен при следующем exec"
                ))
            }
            Err(RecvFailure::Closed) => {
                self.kill();
                Err(anyhow!("shell завершился во время чтения {what}; будет перезапущен при следующем exec"))
            }
        }
    }

    /// Если процесс мёртв (или его нет) — перезапускает в отслеживаемом cwd.
    fn ensure_alive(&mut self) -> Result<()> {
        let alive = match self.child.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        };
        if !alive {
            self.kill();
            self.respawn()?;
        }
        Ok(())
    }

    /// (Пере)запуск shell-процесса и потоков-насосов stdout/stderr.
    fn respawn(&mut self) -> Result<()> {
        self.kill();
        let mut cmd = Command::new(&self.shell_path);
        // --norc — только bash; у sh/zsh своя rc-семантика, не трогаем.
        if is_bash(&self.shell_path) {
            cmd.arg("--norc");
        }
        let mut child = cmd
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("не удалось запустить shell {}", self.shell_path.display()))?;

        let stdin = child.stdin.take().context("stdin shell недоступен")?;
        let stdout = child.stdout.take().context("stdout shell недоступен")?;
        let stderr = child.stderr.take().context("stderr shell недоступен")?;
        let (out_tx, out_rx) = channel();
        let (err_tx, err_rx) = channel();
        thread::spawn(move || pump_lines(stdout, out_tx));
        thread::spawn(move || pump_lines(stderr, err_tx));

        self.child = Some(child);
        self.stdin = Some(stdin);
        self.stdout_rx = Some(out_rx);
        self.stderr_rx = Some(err_rx);
        Ok(())
    }

    /// Убивает процесс (с реaping'ом зомби) и освобождает каналы.
    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdin = None;
        self.stdout_rx = None;
        self.stderr_rx = None;
    }
}

impl Drop for PersistentShell {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Щедрый таймаут для обычных команд в тестах.
    const T: Duration = Duration::from_secs(15);

    fn sh() -> PersistentShell {
        PersistentShell::spawn().expect("shell должен запускаться")
    }

    fn env_with(pairs: &[(&str, &str)]) -> ShellEnv {
        ShellEnv {
            vars: pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            cwd: PathBuf::from("/tmp"),
            shell_path: PathBuf::from("/bin/bash"),
        }
    }

    #[test]
    fn exec_echo_returns_stdout_and_zero_code() {
        let mut shell = sh();
        let out = shell.exec("echo hello-theseus", T).unwrap();
        assert_eq!(out.stdout, "hello-theseus\n");
        assert_eq!(out.code, 0);
        assert!(out.success());
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn stderr_is_captured_separately() {
        let mut shell = sh();
        let out = shell.exec("echo out-line; echo err-line >&2", T).unwrap();
        assert_eq!(out.stdout, "out-line\n");
        assert_eq!(out.stderr, "err-line\n");
        assert!(out.success());
    }

    #[test]
    fn nonzero_exit_code_is_propagated() {
        let mut shell = sh();
        let out = shell.exec("sh -c 'exit 42'", T).unwrap();
        assert_eq!(out.code, 42);
        assert!(!out.success());
        // ...а следующая команда в том же shell снова может быть успешной.
        assert_eq!(shell.exec("true", T).unwrap().code, 0);
        assert_eq!(shell.exec("false", T).unwrap().code, 1);
    }

    #[test]
    fn cd_persists_across_execs() {
        let mut shell = sh();
        shell.exec("cd /tmp", T).unwrap();
        let out = shell.exec("pwd", T).unwrap();
        assert_eq!(out.stdout.trim(), "/tmp");
        assert_eq!(shell.cwd(), Path::new("/tmp"));
    }

    #[test]
    fn failed_cd_keeps_previous_cwd() {
        let mut shell = sh();
        let before = shell.cwd().to_path_buf();
        let out = shell.exec("cd /no/such/dir-theseus", T).unwrap();
        assert!(!out.success());
        assert_eq!(shell.cwd(), before.as_path());
    }

    #[test]
    fn export_persists_across_execs() {
        let mut shell = sh();
        shell.exec("export THESEUS_PERSIST=forty-two", T).unwrap();
        let out = shell.exec("echo \"val=$THESEUS_PERSIST\"", T).unwrap();
        assert_eq!(out.stdout, "val=forty-two\n");
    }

    #[test]
    fn shell_function_persists_across_execs() {
        let mut shell = sh();
        shell.exec("myfn() { echo fn-says-hi; }", T).unwrap();
        let out = shell.exec("myfn", T).unwrap();
        assert_eq!(out.stdout, "fn-says-hi\n");
    }

    #[test]
    fn multiline_output_is_preserved() {
        let mut shell = sh();
        let out = shell.exec("printf 'a\\nb\\nc\\n'", T).unwrap();
        assert_eq!(out.stdout, "a\nb\nc\n");
    }

    #[test]
    fn output_without_trailing_newline_is_preserved() {
        let mut shell = sh();
        let out = shell.exec("printf 'no-newline'", T).unwrap();
        assert_eq!(out.stdout, "no-newline");
    }

    #[test]
    fn fake_marker_text_in_output_does_not_confuse_parser() {
        let mut shell = sh();
        let out = shell
            .exec("echo 'THESEUS_EC_9_faketoken THESEUS_DONE_faketoken'", T)
            .unwrap();
        assert_eq!(out.code, 0);
        assert!(out.stdout.contains("THESEUS_EC_9_faketoken"));
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn empty_command_is_noop_with_zero_code() {
        let mut shell = sh();
        let out = shell.exec("", T).unwrap();
        assert_eq!(out.code, 0);
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn timeout_returns_err_and_shell_recovers() {
        let mut shell = sh();
        let err = shell.exec("sleep 10", Duration::from_secs(1)).unwrap_err();
        assert!(err.to_string().contains("таймаут"), "{err}");
        // Процесс убит; следующий exec прозрачно перезапускает shell.
        let out = shell.exec("echo recovered", T).unwrap();
        assert_eq!(out.stdout, "recovered\n");
    }

    #[test]
    fn cwd_survives_timeout_respawn() {
        let env = ShellEnv::capture();
        let mut shell = PersistentShell::with_options(&env.shell_path, Path::new("/tmp")).unwrap();
        shell.exec("cd /var", T).unwrap();
        assert_eq!(shell.cwd(), Path::new("/var"));
        let _ = shell.exec("sleep 10", Duration::from_secs(1)).unwrap_err();
        // После перезапуска cwd восстановлен из трекера, а не из домашнего каталога.
        let out = shell.exec("pwd", T).unwrap();
        assert_eq!(out.stdout.trim(), "/var");
        assert_eq!(shell.cwd(), Path::new("/var"));
    }

    #[test]
    fn shell_recovers_after_exit_command() {
        let mut shell = sh();
        let err = shell.exec("exit", T).unwrap_err();
        assert!(err.to_string().contains("завершился"), "{err}");
        let out = shell.exec("echo back", T).unwrap();
        assert_eq!(out.stdout, "back\n");
    }

    #[test]
    fn snapshot_env_sees_exported_vars() {
        let mut shell = sh();
        shell.exec("export THESEUS_SNAP_PROBE=snap-value", T).unwrap();
        let env = shell.snapshot_env(T).unwrap();
        assert_eq!(env.vars.get("THESEUS_SNAP_PROBE").map(String::as_str), Some("snap-value"));
        assert_eq!(env.cwd, shell.cwd().to_path_buf());
        assert_eq!(env.shell_path, shell.shell_path().to_path_buf());
    }

    #[test]
    fn env_capture_contains_path_and_cwd() {
        let env = ShellEnv::capture();
        assert!(env.vars.contains_key("PATH"));
        assert_eq!(env.cwd, std::env::current_dir().unwrap());
        assert!(!env.shell_path.as_os_str().is_empty());
    }

    #[test]
    fn env_diff_detects_added_removed_changed() {
        let older = env_with(&[("KEEP", "1"), ("GONE", "old"), ("MUT", "before")]);
        let newer = env_with(&[("KEEP", "1"), ("NEW", "fresh"), ("MUT", "after")]);
        let diff = newer.diff(&older);
        assert!(!diff.is_empty());
        assert_eq!(diff.added.get("NEW").map(String::as_str), Some("fresh"));
        assert_eq!(diff.removed.get("GONE").map(String::as_str), Some("old"));
        assert_eq!(diff.changed.get("MUT"), Some(&("before".to_string(), "after".to_string())));
        assert!(!diff.changed.contains_key("KEEP"));
        let summary = diff.summary();
        assert!(summary.contains("+1"), "{summary}");
        assert!(summary.contains("-1"), "{summary}");
        assert!(summary.contains("~1"), "{summary}");
    }

    #[test]
    fn env_diff_of_identical_snapshots_is_empty() {
        let a = env_with(&[("A", "1"), ("B", "2")]);
        let diff = a.diff(&a);
        assert!(diff.is_empty());
        assert_eq!(diff.summary(), "окружение не изменилось");
    }

    #[test]
    fn apply_to_replaces_env_and_sets_cwd() {
        let mut env = env_with(&[("THESEUS_APPLY_PROBE", "probe-value")]);
        env.cwd = PathBuf::from("/tmp");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("echo \"${HOME-unset}\"; echo \"$THESEUS_APPLY_PROBE\"; pwd");
        env.apply_to(&mut cmd);
        let out = cmd.output().unwrap();
        // HOME родителя не протёк (env_clear), переменная снапшота видна, cwd = /tmp.
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "unset\nprobe-value\n/tmp\n");
    }

    #[test]
    fn shell_env_and_diff_serde_roundtrip() {
        let env = env_with(&[("A", "1"), ("B", "x=y")]);
        let json = serde_json::to_string(&env).unwrap();
        let back: ShellEnv = serde_json::from_str(&json).unwrap();
        assert_eq!(env, back);

        let diff = env_with(&[("A", "1"), ("C", "3")]).diff(&env_with(&[("A", "0"), ("B", "2")]));
        let json = serde_json::to_string(&diff).unwrap();
        let back: EnvDiff = serde_json::from_str(&json).unwrap();
        assert_eq!(diff, back);
    }
}
