//! Расширенные хуки агента: события жизненного цикла + shell-команды.
//!
//! Модуль самодостаточен (std + serde_json + regex) и не зависит от `hooks.rs`:
//! здесь больше событий, regex-матчинг по имени инструмента, параллельный
//! запуск хуков одного события (по потоку на хук) и агрегация их итогов.
//!
//! Контракт хука (shell-команда, исполняется через `bash -c`):
//! - JSON-контекст события подаётся на stdin;
//! - exit 0 — успех; прочие коды — неуспех без побочных эффектов;
//! - exit 2 на `PreToolUse` — блокировка вызова инструмента, stderr уходит
//!   модели как причина (см. [`block_reason`]);
//! - stdout на `PostToolUse` — добавка к результату инструмента
//!   (см. [`collect_stdout`]).
//!
//! Ограничения: потомки хука, удерживающие унаследованный stdout/stderr после
//! выхода `bash`, задержат сбор вывода до своего завершения; убийство по
//! таймауту снимает только процесс `bash` (при одиночной команде `bash -c`
//! делает `exec`, так что убивается сама команда).

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Сколько байт stdout/stderr одного хука сохраняется в итоге; хвост сливается
/// в sink, чтобы ребёнок не заблокировался на переполненном канале.
pub const MAX_HOOK_OUTPUT: u64 = 64 * 1024;

/// Нижняя граница таймаута хука: нулевые/микроскопические значения из конфига
/// повышаются до неё, иначе даже мгновенная команда была бы убита.
const MIN_HOOK_TIMEOUT: Duration = Duration::from_millis(50);

/// Период опроса состояния дочернего процесса в цикле ожидания с таймаутом.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Событие жизненного цикла агента, на которое можно повесить хук.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    /// Перед вызовом инструмента; exit 2 хука отменяет вызов.
    PreToolUse,
    /// После вызова инструмента; stdout хука добавляется к результату.
    PostToolUse,
    /// Перед компактификацией контекста.
    PreCompact,
    /// После компактификации контекста.
    PostCompact,
    /// Старт сессии агента.
    SessionStart,
    /// Завершение сессии агента.
    SessionEnd,
    /// Уведомление агента (запрос внимания пользователя).
    Notification,
    /// Пользователь установил или сменил цель сессии.
    GoalSet,
    /// Пользователь отправил промпт (до начала хода); exit 2 хука блокирует
    /// промпт целиком (миграция из старого hooks.rs — V3 #2.2).
    UserPromptSubmit,
}

impl HookEvent {
    /// Все события в фиксированном порядке (итерация по конфигам, тесты).
    pub const ALL: [HookEvent; 9] = [
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::PreCompact,
        HookEvent::PostCompact,
        HookEvent::SessionStart,
        HookEvent::SessionEnd,
        HookEvent::Notification,
        HookEvent::GoalSet,
        HookEvent::UserPromptSubmit,
    ];

    /// Стабильное имя события (как в конфиге и в JSON-представлении).
    pub const fn as_str(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::Notification => "Notification",
            HookEvent::GoalSet => "GoalSet",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
        }
    }

    /// Разбор имени события из строки конфига; `None` при неизвестном имени.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "PreToolUse" => Self::PreToolUse,
            "PostToolUse" => Self::PostToolUse,
            "PreCompact" => Self::PreCompact,
            "PostCompact" => Self::PostCompact,
            "SessionStart" => Self::SessionStart,
            "SessionEnd" => Self::SessionEnd,
            "Notification" => Self::Notification,
            "GoalSet" => Self::GoalSet,
            "UserPromptSubmit" => Self::UserPromptSubmit,
            _ => return None,
        })
    }
}

impl fmt::Display for HookEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Спецификация одного хука: событие, необязательный regex на имя инструмента,
/// shell-команда и таймаут исполнения.
#[derive(Debug, Clone)]
pub struct HookMatcher {
    /// Событие, на которое срабатывает хук.
    pub event: HookEvent,
    /// Regex на имя инструмента; `None` — срабатывать всегда (для любого
    /// инструмента, а также для событий, у которых инструмента нет).
    pub tool_pattern: Option<Regex>,
    /// Shell-команда (исполняется через `bash -c`, JSON-контекст — на stdin).
    pub command: String,
    /// Таймаут исполнения; по истечении процесс убивается, итог помечается
    /// `timed_out`. Значения ниже ~50 мс повышаются до 50 мс.
    pub timeout: Duration,
}

impl HookMatcher {
    /// Создать матчер, скомпилировав regex из строки. `None` или пустая строка
    /// означают «без фильтра по инструменту». Ошибка компиляции regex
    /// возвращается вызывающему.
    pub fn new(
        event: HookEvent,
        tool_pattern: Option<&str>,
        command: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, regex::Error> {
        let tool_pattern = tool_pattern
            .filter(|p| !p.is_empty())
            .map(Regex::new)
            .transpose()?;
        Ok(Self { event, tool_pattern, command: command.into(), timeout })
    }

    /// Подходит ли хук под имя инструмента: без regex — всегда; с regex —
    /// только если имя известно и совпадает. Событие без имени инструмента
    /// regex-матчеры пропускают.
    pub fn matches_tool(&self, tool_name: Option<&str>) -> bool {
        match &self.tool_pattern {
            None => true,
            Some(re) => tool_name.is_some_and(|t| re.is_match(t)),
        }
    }
}

/// Итог исполнения одного хука.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOutcome {
    /// Команда хука (для логов и диагностики).
    pub command: String,
    /// Код завершения; -1, если процесс не вернул код (сигнал, таймаут,
    /// ошибка запуска/ожидания).
    pub exit_code: i32,
    /// Захваченный stdout (не более [`MAX_HOOK_OUTPUT`] байт). На `PostToolUse`
    /// добавляется к результату инструмента.
    pub stdout: String,
    /// Захваченный stderr (не более [`MAX_HOOK_OUTPUT`] байт). При блокировке
    /// `PreToolUse` уходит модели как причина.
    pub stderr: String,
    /// `true` только у `PreToolUse`-хука с exit 2: вызов инструмента отменяется.
    pub blocked: bool,
    /// `true`, если процесс был убит по таймауту.
    pub timed_out: bool,
}

impl HookOutcome {
    /// Успешное исполнение: код 0 и без таймаута.
    pub fn is_ok(&self) -> bool {
        self.exit_code == 0 && !self.timed_out
    }
}

/// Движок хуков: хранит спецификации и параллельно исполняет подходящие под
/// событие, агрегируя итоги в порядке спецификаций.
#[derive(Debug, Default)]
pub struct HookEngine {
    matchers: Vec<HookMatcher>,
}

impl HookEngine {
    /// Собрать движок из спецификаций; порядок сохраняется в выдаче `fire()`.
    pub fn from_specs(matchers: Vec<HookMatcher>) -> Self {
        Self { matchers }
    }

    /// Спецификации движка (инспекция, диагностика, тесты).
    pub fn matchers(&self) -> &[HookMatcher] {
        &self.matchers
    }

    /// Исполнить все хуки события `event` параллельно (по потоку на хук) и
    /// собрать итоги в порядке спецификаций.
    ///
    /// Имя инструмента извлекается из JSON-контекста (поля `tool_name`/`tool`,
    /// см. [`extract_tool_name`]); матчеры с `tool_pattern` при отсутствии
    /// имени инструмента пропускаются. Контекст каждому хуку передаётся на
    /// stdin как есть. Паника в потоке хука не роняет движок — превращается
    /// в итог с `exit_code == -1`.
    pub fn fire(&self, event: HookEvent, context_json: &str) -> Vec<HookOutcome> {
        let tool_name = extract_tool_name(context_json);
        // exit 2 блокирует: вызов инструмента (PreToolUse) и промпт (UserPromptSubmit)
        // — семантика старого hooks.rs, сохранённая при миграции (V3 #2.2)
        let block_on_exit2 = matches!(event, HookEvent::PreToolUse | HookEvent::UserPromptSubmit);
        let mut handles = Vec::new();
        for m in self
            .matchers
            .iter()
            .filter(|m| m.event == event && m.matches_tool(tool_name.as_deref()))
        {
            let command = m.command.clone();
            let command_for_err = command.clone();
            let context = context_json.to_string();
            let timeout = m.timeout;
            let handle =
                thread::spawn(move || run_hook(&command, &context, timeout, block_on_exit2));
            handles.push((command_for_err, handle));
        }
        let mut outcomes = Vec::with_capacity(handles.len());
        for (command, handle) in handles {
            let outcome = handle.join().unwrap_or_else(|_| HookOutcome {
                command,
                exit_code: -1,
                stdout: String::new(),
                stderr: "паника в потоке исполнения хука".to_string(),
                blocked: false,
                timed_out: false,
            });
            outcomes.push(outcome);
        }
        outcomes
    }
}

/// Извлечь имя инструмента из JSON-контекста события: строковые поля
/// `tool_name` или `tool` (в таком порядке приоритета). Невалидный JSON или
/// отсутствие поля — `None`.
pub fn extract_tool_name(context_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(context_json).ok()?;
    for key in ["tool_name", "tool"] {
        if let Some(name) = value.get(key).and_then(serde_json::Value::as_str) {
            return Some(name.to_string());
        }
    }
    None
}

/// Агрегация: сработал ли хоть один блокирующий хук (инструмент надо отменить).
pub fn any_blocked(outcomes: &[HookOutcome]) -> bool {
    outcomes.iter().any(|o| o.blocked)
}

/// Агрегация: причина блокировки для модели — stderr всех блокирующих хуков
/// через перевод строки; пустая строка, если блокировок нет.
pub fn block_reason(outcomes: &[HookOutcome]) -> String {
    join_trimmed(outcomes.iter().filter(|o| o.blocked).map(|o| &o.stderr))
}

/// Агрегация: непустые stdout хуков через перевод строки. На `PostToolUse`
/// результат добавляется к выводу инструмента.
pub fn collect_stdout(outcomes: &[HookOutcome]) -> String {
    join_trimmed(outcomes.iter().map(|o| &o.stdout))
}

/// Склеить непустые (после trim) куски текста через перевод строки.
fn join_trimmed<'a>(parts: impl Iterator<Item = &'a String>) -> String {
    let mut out = String::new();
    for part in parts {
        let trimmed = part.trim();
        if !trimmed.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(trimmed);
        }
    }
    out
}

/// Исполнить один хук и оформить итог; `blocked` ставится только в режиме
/// `PreToolUse` (block_on_exit2) при коде выхода 2.
fn run_hook(command: &str, context_json: &str, timeout: Duration, block_on_exit2: bool) -> HookOutcome {
    let proc = run_process(command, context_json, timeout);
    let blocked = block_on_exit2 && proc.exit_code == 2 && !proc.timed_out;
    HookOutcome {
        command: command.to_string(),
        exit_code: proc.exit_code,
        stdout: proc.stdout,
        stderr: proc.stderr,
        blocked,
        timed_out: proc.timed_out,
    }
}

/// Внутренний итог запуска процесса (до привязки к семантике хуков).
struct ProcOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

/// Запуск `bash -c` с ретраями на временной нехватке ресурсов: при массовом
/// параллельном спавне (полный прогон тест-сьюта) ядро может ответить EAGAIN/
/// EMFILE — это транзиентно, а не фатально. До 5 попыток с растущей паузой.
fn spawn_bash(command: &str) -> std::io::Result<Child> {
    let mut pause = Duration::from_millis(20);
    let mut attempt = 0u32;
    loop {
        match Command::new("bash")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => return Ok(child),
            Err(e) => {
                attempt += 1;
                let transient = matches!(
                    e.raw_os_error(),
                    Some(11) | Some(12) | Some(23) | Some(24) // EAGAIN/ENOMEM/ENFILE/EMFILE
                );
                if !transient || attempt >= 5 {
                    return Err(e);
                }
                thread::sleep(pause);
                pause *= 2;
            }
        }
    }
}

/// Запустить `bash -c <command>` с JSON-контекстом на stdin, собрать
/// stdout/stderr, убить процесс по таймауту. stdin, stdout и stderr
/// обслуживаются отдельными потоками, чтобы ребёнок не заблокировался на
/// каналах и ожидание не зависло.
fn run_process(command: &str, context_json: &str, timeout: Duration) -> ProcOutput {
    let spawned = spawn_bash(command);
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            return ProcOutput {
                exit_code: -1,
                stdout: String::new(),
                stderr: format!("не удалось запустить хук: {e}"),
                timed_out: false,
            };
        }
    };

    // Писатель stdin — отдельный поток: если ребёнок не читает stdin, запись
    // большого контекста не заблокирует поток ожидания.
    let writer = child.stdin.take().map(|mut stdin| {
        let data = context_json.to_string();
        thread::spawn(move || {
            let _ = stdin.write_all(data.as_bytes());
            // Drop stdin закрывает канал — ребёнок видит EOF.
        })
    });

    // Читатели непрерывно дренят каналы (сверх лимита — в sink), иначе ребёнок
    // мог бы встать на переполненном канале и никогда не завершиться.
    let stdout_reader = child.stdout.take().map(|pipe| thread::spawn(move || drain(pipe)));
    let stderr_reader = child.stderr.take().map(|pipe| thread::spawn(move || drain(pipe)));

    let timeout = timeout.max(MIN_HOOK_TIMEOUT);
    let deadline = Instant::now() + timeout;
    let (exit_code, timed_out, wait_error) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.code().unwrap_or(-1), false, None),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait(); // реапнуть зомби
                break (-1, true, None);
            }
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break (-1, false, Some(format!("ошибка ожидания процесса хука: {e}")));
            }
        }
    };

    if let Some(w) = writer {
        let _ = w.join();
    }
    let stdout = stdout_reader.map(|h| h.join().unwrap_or_default()).unwrap_or_default();
    let mut stderr = stderr_reader.map(|h| h.join().unwrap_or_default()).unwrap_or_default();

    if timed_out {
        append_note(&mut stderr, &format!("хук убит по таймауту ({} мс)", timeout.as_millis()));
    }
    if let Some(e) = wait_error {
        append_note(&mut stderr, &e);
    }
    ProcOutput { exit_code, stdout, stderr, timed_out }
}

/// Добавить строку-заметку к накопленному stderr (через перевод строки).
fn append_note(target: &mut String, note: &str) {
    if !target.is_empty() {
        target.push('\n');
    }
    target.push_str(note);
}

/// Слить канал до EOF: первые [`MAX_HOOK_OUTPUT`] байт сохраняются, остальное
/// уходит в sink, чтобы не блокировать писателя на той стороне.
fn drain<R: Read + Send + 'static>(mut pipe: R) -> String {
    let mut buf = Vec::new();
    let _ = pipe.by_ref().take(MAX_HOOK_OUTPUT).read_to_end(&mut buf);
    let _ = std::io::copy(&mut pipe, &mut std::io::sink());
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: Duration = Duration::from_secs(1);

    /// Короткий конструктор матчера для тестов.
    fn mk(event: HookEvent, pattern: Option<&str>, command: &str, timeout: Duration) -> HookMatcher {
        HookMatcher::new(event, pattern, command, timeout).expect("валидный regex в тесте")
    }

    // --- имена, парсинг и сериализация событий ---

    #[test]
    fn event_names_and_parse_roundtrip() {
        assert_eq!(HookEvent::ALL.len(), 9);
        assert_eq!(HookEvent::PreToolUse.as_str(), "PreToolUse");
        assert_eq!(HookEvent::PostToolUse.as_str(), "PostToolUse");
        assert_eq!(HookEvent::PreCompact.as_str(), "PreCompact");
        assert_eq!(HookEvent::PostCompact.as_str(), "PostCompact");
        assert_eq!(HookEvent::SessionStart.as_str(), "SessionStart");
        assert_eq!(HookEvent::SessionEnd.as_str(), "SessionEnd");
        assert_eq!(HookEvent::Notification.as_str(), "Notification");
        assert_eq!(HookEvent::GoalSet.as_str(), "GoalSet");
        for ev in HookEvent::ALL {
            assert_eq!(HookEvent::from_name(ev.as_str()), Some(ev));
            assert_eq!(ev.to_string(), ev.as_str());
        }
        assert_eq!(HookEvent::from_name("pre_tool_use"), None);
        assert_eq!(HookEvent::from_name("Nope"), None);
    }

    #[test]
    fn event_serde_roundtrip_uses_stable_names() {
        for ev in HookEvent::ALL {
            let json = serde_json::to_string(&ev).unwrap();
            assert_eq!(json, format!("\"{}\"", ev.as_str()));
            let back: HookEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(back, ev);
        }
    }

    // --- извлечение имени инструмента и фильтрация матчеров ---

    #[test]
    fn tool_name_extraction_variants() {
        assert_eq!(extract_tool_name(r#"{"tool_name":"Bash"}"#).as_deref(), Some("Bash"));
        assert_eq!(extract_tool_name(r#"{"tool":"Write"}"#).as_deref(), Some("Write"));
        // Приоритет tool_name над tool.
        assert_eq!(
            extract_tool_name(r#"{"tool_name":"Bash","tool":"Write"}"#).as_deref(),
            Some("Bash")
        );
        assert_eq!(extract_tool_name("{}"), None);
        assert_eq!(extract_tool_name("это не json"), None);
        assert_eq!(extract_tool_name(r#"{"tool_name":42}"#), None);
    }

    #[test]
    fn matcher_tool_filtering() {
        let any = mk(HookEvent::PreToolUse, None, "true", SEC);
        assert!(any.matches_tool(Some("Bash")));
        assert!(any.matches_tool(None));

        let re = mk(HookEvent::PreToolUse, Some("^(Write|Edit)$"), "true", SEC);
        assert!(re.matches_tool(Some("Write")));
        assert!(re.matches_tool(Some("Edit")));
        assert!(!re.matches_tool(Some("Read")));
        // Нет имени инструмента — regex-матчер молчит.
        assert!(!re.matches_tool(None));

        // Пустая строка паттерна эквивалентна отсутствию фильтра.
        let empty = mk(HookEvent::PreToolUse, Some(""), "true", SEC);
        assert!(empty.tool_pattern.is_none());
        assert!(empty.matches_tool(None));
    }

    #[test]
    fn matcher_invalid_regex_is_rejected() {
        assert!(HookMatcher::new(HookEvent::PreToolUse, Some("(["), "true", SEC).is_err());
    }

    // --- движок: пустые и несовпадающие конфигурации ---

    #[test]
    fn empty_engine_and_event_mismatch_fire_nothing() {
        let engine = HookEngine::from_specs(vec![]);
        assert!(engine.fire(HookEvent::PreToolUse, "{}").is_empty());

        let engine = HookEngine::from_specs(vec![mk(HookEvent::SessionStart, None, "echo x", SEC)]);
        assert_eq!(engine.matchers().len(), 1);
        assert!(engine.fire(HookEvent::SessionEnd, "{}").is_empty());
        assert!(engine.fire(HookEvent::GoalSet, "{}").is_empty());
    }

    // --- исполнение реальных команд ---

    #[test]
    fn echo_hook_succeeds_and_captures_stdout() {
        let engine = HookEngine::from_specs(vec![mk(HookEvent::SessionStart, None, "echo hello-hook", SEC)]);
        let out = engine.fire(HookEvent::SessionStart, "{}");
        assert_eq!(out.len(), 1);
        let o = &out[0];
        assert_eq!(o.exit_code, 0);
        assert_eq!(o.stdout.trim(), "hello-hook");
        assert!(o.stderr.is_empty());
        assert!(!o.blocked);
        assert!(!o.timed_out);
        assert!(o.is_ok());
        assert_eq!(o.command, "echo hello-hook");
    }

    #[test]
    fn context_json_is_delivered_to_stdin() {
        let ctx = r#"{"tool_name":"Bash","input":{"command":"ls -la"},"session":"s1"}"#;
        let engine = HookEngine::from_specs(vec![mk(HookEvent::PreToolUse, None, "cat", SEC)]);
        let out = engine.fire(HookEvent::PreToolUse, ctx);
        assert_eq!(out.len(), 1);
        // `cat` вернул stdin без изменений — контекст дошёл байт-в-байт.
        assert_eq!(out[0].stdout, ctx);
        assert_eq!(out[0].exit_code, 0);
    }

    #[test]
    fn stderr_is_captured_separately() {
        let engine = HookEngine::from_specs(vec![
            mk(HookEvent::SessionEnd, None, "echo out-line; echo err-line >&2", SEC),
        ]);
        let out = engine.fire(HookEvent::SessionEnd, "{}");
        assert_eq!(out[0].stdout.trim(), "out-line");
        assert_eq!(out[0].stderr.trim(), "err-line");
        assert!(out[0].is_ok());
    }

    // --- семантика блокировки ---

    #[test]
    fn exit2_on_pre_tool_use_blocks_with_reason() {
        let engine = HookEngine::from_specs(vec![mk(
            HookEvent::PreToolUse,
            None,
            "echo 'deny: опасная команда' >&2; exit 2",
            SEC,
        )]);
        let out = engine.fire(HookEvent::PreToolUse, r#"{"tool_name":"Bash"}"#);
        assert_eq!(out.len(), 1);
        assert!(out[0].blocked);
        assert_eq!(out[0].exit_code, 2);
        assert!(!out[0].is_ok());
        assert!(out[0].stderr.contains("опасная команда"));
        assert!(any_blocked(&out));
        assert!(block_reason(&out).contains("опасная команда"));
    }

    #[test]
    fn exit2_outside_pre_tool_use_does_not_block() {
        for ev in [HookEvent::PostToolUse, HookEvent::SessionEnd, HookEvent::GoalSet] {
            let engine = HookEngine::from_specs(vec![mk(ev, None, "exit 2", SEC)]);
            let out = engine.fire(ev, "{}");
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].exit_code, 2);
            assert!(!out[0].blocked, "exit 2 блокирует только PreToolUse");
            assert!(!any_blocked(&out));
        }
    }

    #[test]
    fn nonzero_exit_is_failure_but_not_block() {
        let engine = HookEngine::from_specs(vec![mk(HookEvent::PreToolUse, None, "false", SEC)]);
        let out = engine.fire(HookEvent::PreToolUse, r#"{"tool_name":"Bash"}"#);
        assert_eq!(out[0].exit_code, 1);
        assert!(!out[0].blocked);
        assert!(!out[0].is_ok());
    }

    // --- таймауты ---

    #[test]
    fn timeout_kills_long_running_hook() {
        let engine = HookEngine::from_specs(vec![mk(
            HookEvent::SessionStart,
            None,
            "sleep 5",
            Duration::from_millis(200),
        )]);
        let start = Instant::now();
        let out = engine.fire(HookEvent::SessionStart, "{}");
        let elapsed = start.elapsed();
        assert!(out[0].timed_out);
        assert_eq!(out[0].exit_code, -1);
        assert!(out[0].stderr.contains("таймаут"));
        assert!(
            elapsed < Duration::from_secs(4),
            "хук должен быть убит по таймауту, а не дожидаться sleep 5: {elapsed:?}"
        );
    }

    #[test]
    fn zero_timeout_is_clamped_not_instant_kill() {
        let engine =
            HookEngine::from_specs(vec![mk(HookEvent::SessionStart, None, "echo fast", Duration::ZERO)]);
        let out = engine.fire(HookEvent::SessionStart, "{}");
        assert_eq!(out[0].exit_code, 0);
        assert_eq!(out[0].stdout.trim(), "fast");
        assert!(!out[0].timed_out);
    }

    // --- параллельность и агрегация ---

    #[test]
    fn hooks_run_in_parallel_and_keep_spec_order() {
        let sleepy = |tag: &str| {
            mk(HookEvent::PostToolUse, None, &format!("sleep 1; echo {tag}"), Duration::from_secs(10))
        };
        let engine = HookEngine::from_specs(vec![sleepy("A"), sleepy("B"), sleepy("C")]);
        let start = Instant::now();
        let out = engine.fire(HookEvent::PostToolUse, r#"{"tool_name":"Read"}"#);
        let elapsed = start.elapsed();
        assert_eq!(out.len(), 3);
        // Порядок агрегации — порядок спецификаций, несмотря на параллельность.
        let tags: Vec<&str> = out.iter().map(|o| o.stdout.trim()).collect();
        assert_eq!(tags, ["A", "B", "C"]);
        assert!(
            elapsed < Duration::from_millis(2600),
            "три sleep 1 параллельно ≈ 1 с, последовательно было бы 3 с: {elapsed:?}"
        );
    }

    #[test]
    fn tool_pattern_routes_events_to_matching_hooks() {
        let engine = HookEngine::from_specs(vec![
            mk(HookEvent::PreToolUse, Some("^Bash$"), "echo bash-hook", SEC),
            mk(HookEvent::PreToolUse, Some("^(Write|Edit)$"), "echo write-hook", SEC),
        ]);
        let out = engine.fire(HookEvent::PreToolUse, r#"{"tool_name":"Bash"}"#);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].stdout.trim(), "bash-hook");

        let out = engine.fire(HookEvent::PreToolUse, r#"{"tool_name":"Edit"}"#);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].stdout.trim(), "write-hook");

        // Событие без имени инструмента в контексте: regex-матчеры молчат.
        assert!(engine.fire(HookEvent::PreToolUse, "{}").is_empty());
    }

    #[test]
    fn aggregation_helpers_join_outputs() {
        let engine = HookEngine::from_specs(vec![
            mk(HookEvent::PostToolUse, None, "echo first", SEC),
            mk(HookEvent::PostToolUse, None, "echo second", SEC),
            mk(HookEvent::PostToolUse, None, "true", SEC), // без вывода — в сборку не попадает
        ]);
        let out = engine.fire(HookEvent::PostToolUse, r#"{"tool_name":"Read"}"#);
        assert_eq!(collect_stdout(&out), "first\nsecond");
        assert!(!any_blocked(&out));
        assert!(block_reason(&out).is_empty());
        assert!(out.iter().all(HookOutcome::is_ok));
    }

    // --- граничные случаи вывода ---

    #[test]
    fn oversized_output_is_truncated_without_deadlock() {
        let engine = HookEngine::from_specs(vec![mk(
            HookEvent::SessionStart,
            None,
            "yes | head -c 200000",
            Duration::from_secs(10),
        )]);
        let out = engine.fire(HookEvent::SessionStart, "{}");
        assert_eq!(out[0].exit_code, 0);
        // Сохранено ровно столько, сколько разрешено лимитом; хвост слит в sink.
        assert_eq!(out[0].stdout.len(), MAX_HOOK_OUTPUT as usize);
    }

    #[test]
    fn empty_command_is_noop_success() {
        let engine = HookEngine::from_specs(vec![mk(HookEvent::SessionStart, None, "", SEC)]);
        let out = engine.fire(HookEvent::SessionStart, "{}");
        assert_eq!(out[0].exit_code, 0);
        assert!(out[0].stdout.is_empty());
        assert!(out[0].is_ok());
    }
}
