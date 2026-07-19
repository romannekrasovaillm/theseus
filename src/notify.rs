//! Уведомления пользователя (образец: codex-rs `notify`, уведомления Claude Code).
//!
//! Харнесс сигнализирует о событиях, требующих внимания человека: задача завершена,
//! нужно разрешение, произошла ошибка, закончился ход агента.
//!
//! Каналы доставки (реализации [`Notifier`]):
//! - [`BellNotifier`] — терминальный звонок (`\x07`);
//! - [`CommandNotifier`] — произвольная shell-команда с безопасной подстановкой
//!   `{event}` / `{message}`: значения экранируются одинарными кавычками (см. [`shell_quote`]),
//!   поэтому инъекция через текст сообщения невозможна;
//! - [`LogNotifier`] — журнал в формате JSON Lines (по строке на событие).
//!
//! [`NotifyManager`] — точка входа: проверяет `enabled` и подписку на событие,
//! троттлит повторные события одного типа и рассылает по всем каналам.
//!
//! Пример:
//! ```no_run
//! use theseus::notify::{NotifyConfig, NotifyEvent, NotifyManager};
//!
//! fn main() -> anyhow::Result<()> {
//!     let config = NotifyConfig { enabled: true, ..NotifyConfig::default() };
//!     let mut manager = NotifyManager::new(config)?;
//!     manager.notify(NotifyEvent::TaskComplete, "сборка завершена")?;
//!     Ok(())
//! }
//! ```

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Троттлинг повторных событий одного типа по умолчанию, секунды.
pub const DEFAULT_THROTTLE_SECS: u64 = 30;

/// Таймаут выполнения команды уведомления по умолчанию.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Сколько первых символов stderr попадает в текст ошибки команды.
const STDERR_PREVIEW_CHARS: usize = 300;

/// Событие, о котором харнесс уведомляет пользователя.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyEvent {
    /// Задача выполнена целиком (аналог «Stop» у Claude Code).
    TaskComplete,
    /// Агент ждёт разрешения пользователя (инструмент, sandbox и т.п.).
    NeedsPermission,
    /// Ошибка выполнения, требующая внимания.
    Error,
    /// Очередной ход агента завершён (без финальной постановки задачи).
    TurnDone,
}

impl NotifyEvent {
    /// Все события — удобно для подписки «по умолчанию» и тестов.
    pub const fn all() -> [NotifyEvent; 4] {
        [
            Self::TaskComplete,
            Self::NeedsPermission,
            Self::Error,
            Self::TurnDone,
        ]
    }

    /// Каноническое имя события (snake_case) — используется в подстановке
    /// `{event}`, в JSON Lines и при сериализации конфига.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::TaskComplete => "task_complete",
            Self::NeedsPermission => "needs_permission",
            Self::Error => "error",
            Self::TurnDone => "turn_done",
        }
    }
}

impl fmt::Display for NotifyEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for NotifyEvent {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::all()
            .into_iter()
            .find(|e| e.as_str() == s)
            .ok_or_else(|| {
                let known = Self::all().iter().map(NotifyEvent::as_str).collect::<Vec<_>>().join(", ");
                anyhow!("неизвестное событие уведомления `{s}`; допустимые значения: {known}")
            })
    }
}

/// Итог обработки события менеджером уведомлений.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// Уведомление отправлено по всем каналам (или каналов нет — считается успехом).
    Sent,
    /// Уведомления выключены конфигом (`enabled = false`).
    Disabled,
    /// На этот тип события нет подписки (`events` не содержит событие).
    NotSubscribed,
    /// Событие отброшено троттлингом: предыдущее такого же типа было недавно.
    Throttled,
}

/// Источник времени — инжектируется, чтобы тесты могли управлять троттлингом.
pub trait Clock {
    /// Текущее время в секундах Unix.
    fn now_unix_secs(&self) -> u64;
}

/// Системные часы (`SystemTime::now`); до эпохи Unix — 0.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_secs(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
    }
}

/// Канал доставки уведомления.
pub trait Notifier {
    /// Доставить уведомление о событии `event` с текстом `message`.
    ///
    /// Ошибка означает «канал недоступен» (команда упала, журнал не открылся) —
    /// вызывающая сторона решает, критично ли это; паниковать реализации не должны.
    fn notify(&self, event: NotifyEvent, message: &str) -> Result<()>;
}

/// Куда писать звонок.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum BellStream {
    Stdout,
    #[default]
    Stderr,
}

/// Терминальный звонок: пишет `\x07` (BEL) в поток — терминал сам рисует
/// индикатор/проигрывает звук. Сообщение не выводится, это чистый сигнал.
#[derive(Debug, Clone, Copy, Default)]
pub struct BellNotifier {
    stream: BellStream,
}

impl BellNotifier {
    /// Звонок в stderr (не путается с полезным выводом программы в stdout).
    pub fn new() -> Self {
        Self { stream: BellStream::Stderr }
    }

    /// Звонок в stdout (для пайплайнов, где stderr занят логами).
    pub fn stdout() -> Self {
        Self { stream: BellStream::Stdout }
    }

    fn emit(self, sink: &mut dyn Write) -> Result<()> {
        sink.write_all(b"\x07").context("не удалось записать BEL в поток")?;
        sink.flush().context("не удалось сбросить поток после BEL")?;
        Ok(())
    }
}

impl Notifier for BellNotifier {
    fn notify(&self, _event: NotifyEvent, _message: &str) -> Result<()> {
        match self.stream {
            BellStream::Stdout => self.emit(&mut io::stdout().lock()),
            BellStream::Stderr => self.emit(&mut io::stderr().lock()),
        }
    }
}

/// Экранирование строки для POSIX shell одинарными кавычками.
///
/// Внутри одинарных кавычек shell не выполняет ни подстановок (`$`, обратные
/// кавычки), ни разбиения на слова; единственный спецсимвол — сама кавычка,
/// которая кодируется классическим `'\''`. Результат всегда безопасно вставлять
/// в команду как один аргумент.
pub fn shell_quote(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('\'');
    for c in raw.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Подстановка `{event}` / `{message}` в шаблон команды.
///
/// Подставляемые значения уже экранированы через [`shell_quote`], поэтому вокруг
/// плейсхолдеров в шаблоне кавычки ставить не нужно: `notify-send {event} {message}`.
/// Литеральные фигурные скобки записываются удвоением: `{{` и `}}`.
///
/// Ошибки: неизвестный плейсхолдер, незакрытый `{`, непарная `}`.
pub fn render_template(template: &str, event: NotifyEvent, message: &str) -> Result<String> {
    let mut out = String::with_capacity(template.len() + message.len() + 8);
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '{' => {
                let mut name = String::new();
                let mut closed = false;
                for inner in chars.by_ref() {
                    if inner == '}' {
                        closed = true;
                        break;
                    }
                    name.push(inner);
                }
                if !closed {
                    return Err(anyhow!("шаблон команды: незакрытый `{{`"));
                }
                match name.as_str() {
                    "event" => out.push_str(&shell_quote(event.as_str())),
                    "message" => out.push_str(&shell_quote(message)),
                    _ => {
                        return Err(anyhow!(
                            "шаблон команды: неизвестный плейсхолдер `{{{name}}}`; допустимые: {{event}}, {{message}}"
                        ));
                    }
                }
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '}' => return Err(anyhow!("шаблон команды: непарная `}}` (для литерала пишите `}}}}`)")),
            _ => out.push(c),
        }
    }
    Ok(out)
}

/// Выполнить команду через `sh -c`, дождаться с таймаутом, вернуть вывод.
///
/// Поток с `wait_with_output` при таймауте не убивается (дочерний процесс
/// доживает до своего выхода фоном) — так же сделано в `hooks.rs` крейта.
fn run_command(rendered: &str, timeout: Duration) -> Result<std::process::Output> {
    let child = Command::new("sh")
        .arg("-c")
        .arg(rendered)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("не удалось запустить команду уведомления `{rendered}`"))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let out = child.wait_with_output();
        let _ = tx.send(out);
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result.context("ошибка ожидания команды уведомления"),
        Err(_) => Err(anyhow!("таймаут {timeout:?}: команда уведомления не завершилась")),
    }
}

/// Уведомление произвольной shell-командой (например `notify-send {event} {message}`).
///
/// Шаблон валидируется в конструкторе, подстановка — на каждое событие.
/// Ненулевой код возврата (включая 127 «команда не найдена») — ошибка, не паника.
pub struct CommandNotifier {
    template: String,
    timeout: Duration,
}

impl CommandNotifier {
    /// Создать с таймаутом по умолчанию ([`DEFAULT_COMMAND_TIMEOUT`]).
    pub fn new(template: &str) -> Result<Self> {
        Self::with_timeout(template, DEFAULT_COMMAND_TIMEOUT)
    }

    /// Создать с явным таймаутом выполнения команды.
    pub fn with_timeout(template: &str, timeout: Duration) -> Result<Self> {
        if template.trim().is_empty() {
            return Err(anyhow!("шаблон команды уведомления пуст"));
        }
        // Валидация плейсхолдеров и скобок — сразу, а не на первом событии.
        render_template(template, NotifyEvent::TaskComplete, "")?;
        Ok(Self { template: template.to_string(), timeout })
    }

    /// Шаблон команды, как его передали в конструктор.
    pub fn template(&self) -> &str {
        &self.template
    }

    /// Таймаут выполнения команды.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl Notifier for CommandNotifier {
    fn notify(&self, event: NotifyEvent, message: &str) -> Result<()> {
        let rendered = render_template(&self.template, event, message)?;
        let out = run_command(&rendered, self.timeout)?;
        if out.status.success() {
            return Ok(());
        }
        let status = out.status;
        let stderr = String::from_utf8_lossy(&out.stderr);
        let preview: String = stderr.trim().chars().take(STDERR_PREVIEW_CHARS).collect();
        Err(anyhow!("команда уведомления `{rendered}` завершилась: {status}; stderr: {preview}"))
    }
}

/// Форматирование секунд Unix как ISO 8601 в UTC (`YYYY-MM-DDTHH:MM:SSZ`).
///
/// Реализовано на std (без chrono): алгоритм `civil_from_days` Говарда Хиннанта.
/// Для значений до эпохи не предназначена — принимает `u64`.
pub fn iso8601_utc(unix_secs: u64) -> String {
    let secs_of_day = unix_secs % 86_400;
    let hour = secs_of_day / 3_600;
    let minute = secs_of_day % 3_600 / 60;
    let second = secs_of_day % 60;
    let z = (unix_secs / 86_400) as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097; // день эпохи [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // год эпохи [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // день года [0, 365], 1 марта = 0
    let mp = (5 * doy + 2) / 153; // месяц от марта [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Журнал уведомлений в формате JSON Lines: одна строка — один объект
/// `{"ts": "…", "event": "…", "message": "…"}`; файл дополняется, не перезаписывается.
pub struct LogNotifier {
    path: PathBuf,
    clock: Box<dyn Clock>,
}

impl LogNotifier {
    /// Журнал с системными часами; файл создаётся при первой записи.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_clock(path, Box::new(SystemClock))
    }

    /// Журнал с инжектируемыми часами (для тестов и встраивания).
    pub fn with_clock(path: impl Into<PathBuf>, clock: Box<dyn Clock>) -> Self {
        Self { path: path.into(), clock }
    }

    /// Путь к файлу журнала.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Notifier for LogNotifier {
    fn notify(&self, event: NotifyEvent, message: &str) -> Result<()> {
        let record = serde_json::json!({
            "ts": iso8601_utc(self.clock.now_unix_secs()),
            "event": event.as_str(),
            "message": message,
        });
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("не удалось открыть журнал уведомлений `{}`", self.path.display()))?;
        writeln!(file, "{record}")
            .with_context(|| format!("не удалось записать в журнал уведомлений `{}`", self.path.display()))?;
        Ok(())
    }
}

/// Конфигурация уведомлений (секция конфига харнесса; сериализуется в toml/json).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    /// Глобальный выключатель: `false` — все события игнорируются.
    pub enabled: bool,
    /// Подписка: только эти типы событий доставляются.
    pub events: HashSet<NotifyEvent>,
    /// Шаблон shell-команды (см. [`render_template`]); `None` — команда не запускается.
    pub command: Option<String>,
    /// Минимальный интервал между двумя событиями одного типа, секунды; 0 — без троттлинга.
    pub throttle_secs: u64,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            events: NotifyEvent::all().into_iter().collect(),
            command: None,
            throttle_secs: DEFAULT_THROTTLE_SECS,
        }
    }
}

/// Менеджер уведомлений: фильтрация, троттлинг, рассылка по каналам.
///
/// Потокобезопасность не гарантируется (часы могут быть `Rc`-заглушкой в тестах);
/// типичное использование — один менеджер на поток UI/агента.
pub struct NotifyManager {
    config: NotifyConfig,
    notifiers: Vec<Box<dyn Notifier>>,
    last_sent: HashMap<NotifyEvent, u64>,
    clock: Box<dyn Clock>,
}

impl NotifyManager {
    /// Конфигурация по умолчанию: системные часы, [`BellNotifier`] плюс
    /// [`CommandNotifier`], если в конфиге задан `command` (невалидный шаблон — ошибка).
    /// Журнал подключается отдельно через [`NotifyManager::add_notifier`].
    pub fn new(config: NotifyConfig) -> Result<Self> {
        let mut notifiers: Vec<Box<dyn Notifier>> = vec![Box::new(BellNotifier::new())];
        if let Some(cmd) = &config.command {
            notifiers.push(Box::new(CommandNotifier::new(cmd)?));
        }
        Ok(Self::with_notifiers(config, Box::new(SystemClock), notifiers))
    }

    /// Полный контроль над каналами и часами (тесты, встраивание):
    /// ничего не добавляется автоматически.
    pub fn with_notifiers(
        config: NotifyConfig,
        clock: Box<dyn Clock>,
        notifiers: Vec<Box<dyn Notifier>>,
    ) -> Self {
        Self { config, notifiers, last_sent: HashMap::new(), clock }
    }

    /// Подключить дополнительный канал (например [`LogNotifier`]).
    pub fn add_notifier(&mut self, notifier: impl Notifier + 'static) {
        self.notifiers.push(Box::new(notifier));
    }

    /// Текущая конфигурация.
    pub fn config(&self) -> &NotifyConfig {
        &self.config
    }

    /// Обработать событие: фильтры → троттлинг → рассылка.
    ///
    /// Время последней отправки фиксируется до рассылки, поэтому даже упавшая
    /// доставка троттлит повторы (защита от шторма ошибок). Ошибка одного канала
    /// не отменяет остальные: все каналы опрашиваются, ошибки собираются в одну.
    pub fn notify(&mut self, event: NotifyEvent, message: &str) -> Result<NotifyOutcome> {
        if !self.config.enabled {
            return Ok(NotifyOutcome::Disabled);
        }
        if !self.config.events.contains(&event) {
            return Ok(NotifyOutcome::NotSubscribed);
        }
        let now = self.clock.now_unix_secs();
        let throttle = self.config.throttle_secs;
        if throttle > 0 {
            if let Some(&prev) = self.last_sent.get(&event) {
                if now.saturating_sub(prev) < throttle {
                    return Ok(NotifyOutcome::Throttled);
                }
            }
        }
        self.last_sent.insert(event, now);
        let mut failed = Vec::new();
        for notifier in &self.notifiers {
            if let Err(e) = notifier.notify(event, message) {
                failed.push(format!("{e:#}"));
            }
        }
        if failed.is_empty() {
            Ok(NotifyOutcome::Sent)
        } else {
            let details = failed.join("; ");
            Err(anyhow!("уведомление `{event}` не доставлено: {details}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};

    static UNIQUE: AtomicU64 = AtomicU64::new(0);

    /// Уникальный путь во временном каталоге (без стороннего tempdir).
    fn temp_path(tag: &str) -> PathBuf {
        let n = UNIQUE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("theseus-notify-{tag}-{}-{n}.log", std::process::id()))
    }

    /// Часы-заглушка с ручным переводом времени.
    #[derive(Clone)]
    struct MockClock(Rc<Cell<u64>>);

    impl MockClock {
        fn new(start: u64) -> Self {
            Self(Rc::new(Cell::new(start)))
        }

        fn advance(&self, secs: u64) {
            self.0.set(self.0.get() + secs);
        }
    }

    impl Clock for MockClock {
        fn now_unix_secs(&self) -> u64 {
            self.0.get()
        }
    }

    /// Канал-записывашка: складывает события в общий буфер.
    #[derive(Clone)]
    struct Recorder(Rc<RefCell<Vec<(NotifyEvent, String)>>>);

    impl Recorder {
        fn new() -> Self {
            Self(Rc::new(RefCell::new(Vec::new())))
        }

        fn taken(&self) -> Vec<(NotifyEvent, String)> {
            self.0.borrow().to_vec()
        }
    }

    impl Notifier for Recorder {
        fn notify(&self, event: NotifyEvent, message: &str) -> Result<()> {
            self.0.borrow_mut().push((event, message.to_string()));
            Ok(())
        }
    }

    /// Канал, который всегда падает.
    struct Failer;

    impl Notifier for Failer {
        fn notify(&self, _event: NotifyEvent, _message: &str) -> Result<()> {
            Err(anyhow!("сбой доставки"))
        }
    }

    fn enabled_config() -> NotifyConfig {
        NotifyConfig { enabled: true, ..NotifyConfig::default() }
    }

    fn manager(cfg: NotifyConfig, clock: &MockClock, notifiers: Vec<Box<dyn Notifier>>) -> NotifyManager {
        NotifyManager::with_notifiers(cfg, Box::new(clock.clone()), notifiers)
    }

    // ---- iso8601_utc ----

    #[test]
    fn iso8601_epoch_zero() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn iso8601_known_dates() {
        assert_eq!(iso8601_utc(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(iso8601_utc(951_782_400), "2000-02-29T00:00:00Z"); // високосный день
        assert_eq!(iso8601_utc(1_609_459_200), "2021-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_784_374_876), "2026-07-18T11:41:16Z");
    }

    // ---- shell_quote ----

    #[test]
    fn quote_plain_string() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
    }

    #[test]
    fn quote_single_quote_is_escaped() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("'"), "''\\'''");
    }

    #[test]
    fn quote_empty_string() {
        assert_eq!(shell_quote(""), "''");
    }

    // ---- render_template ----

    #[test]
    fn render_basic_substitution() {
        let got = render_template("notify-send {event} {message}", NotifyEvent::Error, "boom").unwrap();
        assert_eq!(got, "notify-send 'error' 'boom'");
    }

    #[test]
    fn render_literal_braces() {
        let got = render_template("echo {{done}} {event}", NotifyEvent::TaskComplete, "").unwrap();
        assert_eq!(got, "echo {done} 'task_complete'");
    }

    #[test]
    fn render_unknown_placeholder_is_error() {
        let err = render_template("echo {host}", NotifyEvent::Error, "m").unwrap_err();
        assert!(err.to_string().contains("плейсхолдер"));
    }

    #[test]
    fn render_unbalanced_braces_are_errors() {
        assert!(render_template("echo {", NotifyEvent::Error, "m").is_err());
        assert!(render_template("echo {event", NotifyEvent::Error, "m").is_err());
        assert!(render_template("echo }", NotifyEvent::Error, "m").is_err());
    }

    #[test]
    fn render_without_placeholders_is_identity() {
        let tpl = "uptime && echo ok";
        assert_eq!(render_template(tpl, NotifyEvent::TurnDone, "ignored").unwrap(), tpl);
    }

    // ---- CommandNotifier ----

    #[test]
    fn command_new_validates_template() {
        assert!(CommandNotifier::new("").is_err());
        assert!(CommandNotifier::new("   ").is_err());
        assert!(CommandNotifier::new("echo {host}").is_err());
        assert!(CommandNotifier::new("echo {message").is_err());
        assert!(CommandNotifier::new("echo {message}").is_ok());
    }

    #[test]
    fn command_substitution_is_injection_proof() {
        // Враждебное сообщение: кавычки, $, обратные кавычки, $(), перевод строки.
        let message = "it's \"quoted\"; $HOME `echo PWNED` $(id) && touch /tmp/pwned\nвторая строка";
        let rendered = render_template("printf '%s' {message}", NotifyEvent::Error, message).unwrap();
        let out = run_command(&rendered, Duration::from_secs(5)).unwrap();
        assert!(out.status.success());
        // Вывод совпал с сообщением байт-в-байт: ни одна подстановка shell не сработала.
        assert_eq!(String::from_utf8(out.stdout).unwrap(), message);
        assert!(!Path::new("/tmp/pwned").exists());
    }

    #[test]
    fn command_substitutes_event_name() {
        let rendered = render_template("printf '%s' {event}", NotifyEvent::NeedsPermission, "m").unwrap();
        let out = run_command(&rendered, Duration::from_secs(5)).unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "needs_permission");
    }

    #[test]
    fn command_not_found_is_error_not_panic() {
        let notifier = CommandNotifier::new("definitely-not-a-real-binary-xyz-737 {message}").unwrap();
        let err = notifier.notify(NotifyEvent::Error, "boom").unwrap_err();
        // sh возвращает 127 «command not found» — это Err, а не паника.
        let text = err.to_string();
        assert!(text.contains("127"), "неожиданный текст ошибки: {text}");
    }

    #[test]
    fn command_nonzero_exit_reports_status_and_stderr() {
        let notifier = CommandNotifier::new("echo oops >&2; exit 3").unwrap();
        let err = notifier.notify(NotifyEvent::Error, "m").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("exit status: 3"), "неожиданный текст ошибки: {text}");
        assert!(text.contains("oops"), "stderr не попал в ошибку: {text}");
    }

    #[test]
    fn command_timeout_is_error() {
        let notifier = CommandNotifier::with_timeout("sleep 5", Duration::from_millis(150)).unwrap();
        let err = notifier.notify(NotifyEvent::TurnDone, "m").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("таймаут"), "неожиданный текст ошибки: {text}");
    }

    // ---- BellNotifier ----

    #[test]
    fn bell_writes_single_bel_byte() {
        let mut buf: Vec<u8> = Vec::new();
        BellNotifier::new().emit(&mut buf).unwrap();
        assert_eq!(buf, b"\x07");
        BellNotifier::stdout().emit(&mut buf).unwrap();
        assert_eq!(buf.len(), 2);
    }

    // ---- LogNotifier ----

    #[test]
    fn log_writes_jsonl_and_appends() {
        let path = temp_path("jsonl");
        let clock = MockClock::new(1_609_459_200);
        let log = LogNotifier::with_clock(&path, Box::new(clock));
        log.notify(NotifyEvent::Error, "boom").unwrap();
        log.notify(NotifyEvent::TurnDone, "ход завершён").unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["ts"], "2021-01-01T00:00:00Z");
        assert_eq!(first["event"], "error");
        assert_eq!(first["message"], "boom");
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["event"], "turn_done");
        assert_eq!(second["message"], "ход завершён");

        // Второй инстанс на том же пути дописывает, а не перезаписывает.
        LogNotifier::new(&path).notify(NotifyEvent::TaskComplete, "ещё").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 3);
        assert!(content.ends_with('\n'));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn log_bad_path_is_error_not_panic() {
        let path = std::env::temp_dir().join("theseus-notify-no-such-dir-zzz").join("f.log");
        let err = LogNotifier::new(&path).notify(NotifyEvent::Error, "x").unwrap_err();
        assert!(err.to_string().contains("не удалось открыть"));
    }

    // ---- NotifyManager ----

    #[test]
    fn manager_disabled_skips_everything() {
        let clock = MockClock::new(1_000);
        let rec = Recorder::new();
        let mut mgr = manager(NotifyConfig::default(), &clock, vec![Box::new(rec.clone())]);
        assert_eq!(mgr.notify(NotifyEvent::Error, "boom").unwrap(), NotifyOutcome::Disabled);
        assert!(rec.taken().is_empty());
    }

    #[test]
    fn manager_filters_unsubscribed_events() {
        let clock = MockClock::new(1_000);
        let rec = Recorder::new();
        let cfg = NotifyConfig {
            events: [NotifyEvent::Error].into_iter().collect(),
            ..enabled_config()
        };
        let mut mgr = manager(cfg, &clock, vec![Box::new(rec.clone())]);
        assert_eq!(mgr.notify(NotifyEvent::TaskComplete, "t").unwrap(), NotifyOutcome::NotSubscribed);
        assert_eq!(mgr.notify(NotifyEvent::Error, "e").unwrap(), NotifyOutcome::Sent);
        assert_eq!(rec.taken(), vec![(NotifyEvent::Error, "e".to_string())]);
    }

    #[test]
    fn manager_throttles_repeated_events_with_boundary() {
        let clock = MockClock::new(1_000_000);
        let rec = Recorder::new();
        let cfg = NotifyConfig { throttle_secs: 60, ..enabled_config() };
        let mut mgr = manager(cfg, &clock, vec![Box::new(rec.clone())]);

        assert_eq!(mgr.notify(NotifyEvent::Error, "1").unwrap(), NotifyOutcome::Sent);
        clock.advance(1);
        assert_eq!(mgr.notify(NotifyEvent::Error, "2").unwrap(), NotifyOutcome::Throttled);
        clock.advance(58); // всего 59 сек — ещё рано
        assert_eq!(mgr.notify(NotifyEvent::Error, "3").unwrap(), NotifyOutcome::Throttled);
        clock.advance(1); // ровно 60 сек — граница: уже не троттлится
        assert_eq!(mgr.notify(NotifyEvent::Error, "4").unwrap(), NotifyOutcome::Sent);
        let msgs: Vec<String> = rec.taken().into_iter().map(|(_, m)| m).collect();
        assert_eq!(msgs, vec!["1".to_string(), "4".to_string()]);
    }

    #[test]
    fn manager_zero_throttle_never_throttles() {
        let clock = MockClock::new(1_000);
        let rec = Recorder::new();
        let cfg = NotifyConfig { throttle_secs: 0, ..enabled_config() };
        let mut mgr = manager(cfg, &clock, vec![Box::new(rec.clone())]);
        assert_eq!(mgr.notify(NotifyEvent::Error, "1").unwrap(), NotifyOutcome::Sent);
        assert_eq!(mgr.notify(NotifyEvent::Error, "2").unwrap(), NotifyOutcome::Sent);
        assert_eq!(rec.taken().len(), 2);
    }

    #[test]
    fn manager_throttle_is_per_event_type() {
        let clock = MockClock::new(1_000);
        let cfg = NotifyConfig { throttle_secs: 60, ..enabled_config() };
        let mut mgr = manager(cfg, &clock, vec![]);
        assert_eq!(mgr.notify(NotifyEvent::Error, "e1").unwrap(), NotifyOutcome::Sent);
        // Другой тип события не попадает под троттлинг первого.
        assert_eq!(mgr.notify(NotifyEvent::TaskComplete, "t1").unwrap(), NotifyOutcome::Sent);
        assert_eq!(mgr.notify(NotifyEvent::Error, "e2").unwrap(), NotifyOutcome::Throttled);
    }

    #[test]
    fn manager_fanout_failure_still_reaches_others() {
        let clock = MockClock::new(1_000);
        let rec = Recorder::new();
        let mut mgr = manager(
            enabled_config(),
            &clock,
            vec![Box::new(Failer), Box::new(rec.clone())],
        );
        let err = mgr.notify(NotifyEvent::TaskComplete, "готово").unwrap_err();
        assert!(err.to_string().contains("сбой доставки"));
        assert_eq!(rec.taken().len(), 1);
    }

    #[test]
    fn manager_new_builds_from_config() {
        let ok = NotifyManager::new(NotifyConfig {
            command: Some("notify-send {message}".to_string()),
            ..enabled_config()
        });
        assert!(ok.is_ok());
        let bad = NotifyManager::new(NotifyConfig {
            command: Some("echo {nope}".to_string()),
            ..enabled_config()
        });
        assert!(bad.is_err());
    }

    // ---- NotifyEvent / NotifyConfig сериализация ----

    #[test]
    fn event_from_str_display_roundtrip() {
        for event in NotifyEvent::all() {
            let s = event.as_str();
            assert_eq!(s.parse::<NotifyEvent>().unwrap(), event);
            assert_eq!(event.to_string(), s);
        }
        assert!("TaskComplete".parse::<NotifyEvent>().is_err()); // только snake_case
        assert!("nope".parse::<NotifyEvent>().is_err());
    }

    #[test]
    fn event_serde_uses_snake_case() {
        let json = serde_json::to_string(&NotifyEvent::NeedsPermission).unwrap();
        assert_eq!(json, "\"needs_permission\"");
        let back: NotifyEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, NotifyEvent::NeedsPermission);
    }

    #[test]
    fn config_serde_roundtrip_and_defaults() {
        let cfg = NotifyConfig {
            enabled: true,
            events: [NotifyEvent::Error, NotifyEvent::TurnDone].into_iter().collect(),
            command: Some("notify-send {event} {message}".to_string()),
            throttle_secs: 5,
        };
        let text = serde_json::to_string(&cfg).unwrap();
        let back: NotifyConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(cfg, back);

        // Частичный конфиг: отсутствующие поля — из Default.
        let partial: NotifyConfig = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(partial.enabled);
        assert_eq!(partial.events.len(), 4);
        assert!(partial.command.is_none());
        assert_eq!(partial.throttle_secs, DEFAULT_THROTTLE_SECS);
    }
}
