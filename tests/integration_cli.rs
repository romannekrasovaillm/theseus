//! Интеграционные тесты бинарника `theseus` в headless-режиме против мок-сервера
//! SSE (`theseus::mock_sse`): полный цикл «процесс → HTTP → журнал запросов».
//!
//! Прогон: `cargo test --test integration_cli` (cargo сам собирает бинарник,
//! путь берётся из `CARGO_BIN_EXE_theseus`; фолбэки — env `THESEUS_BIN` и
//! `target/debug/theseus` от корня манифеста).
//!
//! ## Контракт `theseus::mock_sse` (используемая тестами часть API)
//! - `MockServer::start(Vec<MockResponse>) -> std::io::Result<MockServer>` —
//!   поднимает HTTP-сервер на `127.0.0.1:0`, обрабатывает `POST /chat/completions`,
//!   отвечает SSE-кадрами `data: {...}\n\n` + `data: [DONE]\n\n` по сценарию
//!   (один ответ на один запрос; при исчерпании сценария — безопасный текстовый
//!   ответ, не зависание);
//! - `MockServer::port(&self) -> u16` — фактический порт;
//! - `MockServer::requests(&self) -> Vec<RecordedRequest>` — снимок журнала
//!   запросов в порядке поступления;
//! - `RecordedRequest { pub method: String, pub path: String, pub body: String }` —
//!   сырое тело POST (JSON chat/completions);
//! - `MockResponse::text(&str) -> MockResponse` — текстовый ответ (finish_reason=stop);
//! - `MockResponse::tool_call(&str, &str) -> MockResponse` — один tool_call
//!   (имя инструмента, JSON-аргументы; finish_reason=tool_calls);
//! - `MockResponse::with_finish_reason(self, &str) -> MockResponse` — переопределить
//!   finish_reason (нужен сценарий эскалации по «length»).
//!
//! ## Нюансы харнесса, учтённые в тестах
//! - После инструмента `finish` агент делает один внеплановый не-стрим запрос
//!   (консолидация памяти `consolidate_memory`: системный промпт всегда ≥ 800
//!   символов, поэтому ветка срабатывает всегда, когда задан `HOME`). Отсюда:
//!   сценарии завершаются запасным `spare()`-ответом, а проверки журнала идут
//!   по префиксу (`reqs[0..N]`), а не по точной длине журнала.
//! - Изоляция от пользовательского окружения: дочернему процессу выставляем
//!   `HOME=<tempdir>` (нет `~/.config/theseus`, `~/.theseus`), `cwd=<tempdir>`
//!   (нет `./.theseus/config.toml`), `DEEPSEEK_API_KEY=test` и снимаем
//!   `THESEUS_BASE_URL`/`THESEUS_API_KEY` (могут быть в env хозяйской сессии).
//! - stdout процесса в тестах — пайп (не tty) → срабатывает ветка `run_headless`.
//! - Дефолт `max_output_tokens = 8192`; эскалация по finish_reason=length — ×2
//!   ровно один раз (см. agent/mod.rs, флаг output_escalated).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use theseus::mock_sse::*;

/// Верхний лимит ожидания одного дочернего прогона. Мок отвечает мгновенно,
/// агентные ходы — локальные; 60 с — с большим запасом (и защита от зависаний).
const TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Временный каталог (std only, без tempfile)
// ---------------------------------------------------------------------------

/// Уникальный временный каталог под workspace теста; удаляется в Drop.
struct TempWs(PathBuf);

impl TempWs {
    /// Создать каталог `<tmp>/theseus_it_<pid>_<ns>_<seq>_<tag>`.
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "theseus_it_{}_{nanos}_{seq}_{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("не создать tempdir {}: {e}", dir.display()));
        TempWs(dir)
    }

    /// Путь каталога.
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempWs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Дочерний процесс: страж (kill в Drop) и сбор вывода с таймаутом
// ---------------------------------------------------------------------------

/// Страж дочернего процесса: при Drop убивает и реапит процесс (никаких зомби
/// и висящих theseus после упавшего теста).
struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    /// Запустить команду и обернуть процесс стражом.
    fn spawn(cmd: &mut Command) -> std::io::Result<Self> {
        cmd.spawn().map(|c| ChildGuard { child: Some(c) })
    }

    /// Дождаться завершения с таймаутом, собрать stdout/stderr.
    ///
    /// Чтение пайпов — в отдельных потоках (исключает дедлок на заполненном
    /// буфере пайпа). При таймауте процесс убивается, частичный вывод
    /// сохраняется и помечается `timed_out`.
    fn wait_with_output(mut self, timeout: Duration) -> RunOutcome {
        let mut child = match self.child.take() {
            Some(c) => c,
            None => panic!("ChildGuard: повторное ожидание"),
        };
        let stdout_reader = match child.stdout.take() {
            Some(p) => pipe_to_vec(p),
            None => panic!("stdout не был запайплен"),
        };
        let stderr_reader = match child.stderr.take() {
            Some(p) => pipe_to_vec(p),
            None => panic!("stderr не был запайплен"),
        };
        let deadline = Instant::now() + timeout;
        let (status, timed_out) = loop {
            match child.try_wait() {
                Ok(Some(st)) => break (st, false),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let st = child
                        .wait()
                        .unwrap_or_else(|e| panic!("reap после kill: {e}"));
                    break (st, true);
                }
                Err(e) => panic!("try_wait дочернего процесса: {e}"),
            }
        };
        let stdout = String::from_utf8_lossy(
            &stdout_reader
                .join()
                .unwrap_or_else(|_| panic!("поток-читатель stdout запаниковал")),
        )
        .into_owned();
        let stderr = String::from_utf8_lossy(
            &stderr_reader
                .join()
                .unwrap_or_else(|_| panic!("поток-читатель stderr запаниковал")),
        )
        .into_owned();
        RunOutcome { status, stdout, stderr, timed_out }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Читатель пайпа в отдельном потоке: дочитывает до EOF, отдаёт байты.
fn pipe_to_vec(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

/// Итог дочернего прогона: статус, оба потока вывода, флаг таймаута.
struct RunOutcome {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

impl RunOutcome {
    /// Процесс обязан завершиться сам и с кодом 0; иначе паника с полным дампом.
    fn assert_ok(&self) {
        assert!(
            !self.timed_out,
            "процесс превысил таймаут {TIMEOUT:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout, self.stderr
        );
        assert!(
            self.status.success(),
            "код выхода: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.status.code(),
            self.stdout,
            self.stderr
        );
    }
}

// ---------------------------------------------------------------------------
// Запуск theseus
// ---------------------------------------------------------------------------

/// Путь к бинарнику theseus: cargo-подстановка → env THESEUS_BIN → target/debug.
fn theseus_bin() -> PathBuf {
    if let Some(p) = option_env!("CARGO_BIN_EXE_theseus") {
        return PathBuf::from(p);
    }
    if let Ok(p) = std::env::var("THESEUS_BIN") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/theseus")
}

/// Команда без API-ключа: изоляция HOME/cwd, снятие THESEUS_*, пайпы потоков.
fn bare_command(ws: &TempWs) -> Command {
    let mut cmd = Command::new(theseus_bin());
    cmd.env("HOME", ws.path())
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("THESEUS_API_KEY")
        .env_remove("THESEUS_BASE_URL")
        .current_dir(ws.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Базовая команда: тестовый API-ключ поверх изолированного окружения.
fn base_command(ws: &TempWs) -> Command {
    let mut cmd = bare_command(ws);
    cmd.env("DEEPSEEK_API_KEY", "integration-test-key");
    cmd
}

/// Headless-прогон против мок-сервера: `-w <ws> --base-url <url> --yolo -p <prompt>`.
fn spawn_headless(
    ws: &TempWs,
    mock_url: &str,
    prompt: &str,
    extra: &[&str],
) -> std::io::Result<ChildGuard> {
    let mut cmd = base_command(ws);
    cmd.arg("-w")
        .arg(ws.path())
        .arg("--base-url")
        .arg(mock_url)
        .arg("--yolo")
        .arg("-p")
        .arg(prompt);
    for a in extra {
        cmd.arg(a);
    }
    ChildGuard::spawn(&mut cmd)
}

// ---------------------------------------------------------------------------
// Утилиты мока и журнала запросов
// ---------------------------------------------------------------------------

/// Базовый URL мок-сервера для `--base-url`.
fn mock_url(server: &MockServer) -> String {
    format!("http://127.0.0.1:{}", server.port())
}

/// Ответ-сценарий: вызов инструмента finish с заданным резюме.
fn finish_call(summary: &str) -> MockResponse {
    MockResponse::tool_call("finish", &serde_json::json!({"summary": summary}).to_string())
}

/// Ответ-сценарий: вызов инструмента bash с заданной командой.
fn bash_call(command: &str) -> MockResponse {
    MockResponse::tool_call("bash", &serde_json::json!({"command": command}).to_string())
}

/// Запасной ответ в конце сценария: поглощает внеплановый запрос консолидации
/// памяти после finish (и любой иной запрос сверх сценария), ничего не ломая.
fn spare() -> MockResponse {
    MockResponse::text("EMPTY")
}

/// Тело записанного запроса как JSON (паника с дампом, если не JSON).
fn body_json(req: &RecordedRequest) -> serde_json::Value {
    serde_json::from_str(&req.body)
        .unwrap_or_else(|e| panic!("тело запроса не JSON: {e}\n---\n{}", req.body))
}

/// Поле max_tokens тела запроса (0, если отсутствует).
fn max_tokens(req: &RecordedRequest) -> u64 {
    body_json(req)["max_tokens"].as_u64().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

/// Сценарий 1: простая задача — мок сразу отвечает tool_call finish{summary}.
/// Процесс обязан завершиться кодом 0, а в stdout — событие FINISH с резюме.
#[test]
fn finish_tool_exits_zero_and_prints_finish() {
    let ws = TempWs::new("finish_ok");
    let server = MockServer::start(vec![
        finish_call("задача выполнена: мок-проверка"),
        spare(),
    ])
    .expect("старт мок-сервера");
    let out = spawn_headless(&ws, &mock_url(&server), "сделай простую задачу", &[])
        .expect("spawn theseus")
        .wait_with_output(TIMEOUT);
    out.assert_ok();
    assert!(
        out.stdout.contains("FINISH"),
        "в stdout нет события FINISH:\n{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("задача выполнена: мок-проверка"),
        "в stdout нет резюме finish:\n{}",
        out.stdout
    );
    assert!(
        !server.requests().is_empty(),
        "мок обязан получить хотя бы один запрос"
    );
}

/// Сценарий 2: цикл с bash — мок просит bash{command:"echo hello-from-mock"},
/// затем finish. Результат echo обязан попасть в тело следующего запроса
/// (tool-сообщение модели).
#[test]
fn bash_echo_loop_result_visible_in_next_request() {
    let ws = TempWs::new("echo_loop");
    let server = MockServer::start(vec![
        bash_call("echo hello-from-mock"),
        finish_call("эхо получено и обработано"),
        spare(),
    ])
    .expect("старт мок-сервера");
    let out = spawn_headless(&ws, &mock_url(&server), "выполни echo и завершись", &[])
        .expect("spawn theseus")
        .wait_with_output(TIMEOUT);
    out.assert_ok();
    let reqs = server.requests();
    assert!(
        reqs.len() >= 2,
        "ожидалось ≥2 запросов (ход + повтор после tool_result), в журнале: {}",
        reqs.len()
    );
    assert!(
        reqs[1].body.contains("hello-from-mock"),
        "во втором запросе нет результата echo:\n{}",
        reqs[1].body
    );
    assert!(
        out.stdout.contains("FINISH"),
        "процесс обязан дойти до finish:\n{}",
        out.stdout
    );
}

/// Сценарий 3: finish_reason=length → одноразовая эскалация max_output ×2.
/// Повторный запрос обязан нести удвоенный max_tokens (8192 → 16384).
#[test]
fn finish_reason_length_doubles_max_tokens() {
    let ws = TempWs::new("escalate");
    let server = MockServer::start(vec![
        MockResponse::text("обрезанный на середине ответ").with_finish_reason("length"),
        finish_call("завершено после эскалации"),
        spare(),
    ])
    .expect("старт мок-сервера");
    let out = spawn_headless(&ws, &mock_url(&server), "длинный ответ", &[])
        .expect("spawn theseus")
        .wait_with_output(TIMEOUT);
    out.assert_ok();
    let reqs = server.requests();
    assert!(
        reqs.len() >= 2,
        "ожидался повторный запрос после эскалации, в журнале: {}",
        reqs.len()
    );
    let first = max_tokens(&reqs[0]);
    let second = max_tokens(&reqs[1]);
    assert_eq!(first, 8192, "дефолт max_output_tokens");
    assert_eq!(
        second,
        first * 2,
        "finish_reason=length → max_output ×2 на повторном запросе"
    );
    assert!(
        out.stdout.contains("16384"),
        "в stdout обязано быть событие эскалации:\n{}",
        out.stdout
    );
}

/// Граничный случай сценария 3: эскалация одноразовая — второй подряд
/// finish_reason=length НЕ удваивает max_tokens повторно (16384, не 32768).
#[test]
fn length_escalation_is_one_shot() {
    let ws = TempWs::new("escalate_once");
    let server = MockServer::start(vec![
        MockResponse::text("первый обрезок").with_finish_reason("length"),
        MockResponse::text("второй обрезок").with_finish_reason("length"),
        finish_call("после двух обрезков"),
        spare(),
    ])
    .expect("старт мок-сервера");
    let out = spawn_headless(&ws, &mock_url(&server), "ещё длинный ответ", &[])
        .expect("spawn theseus")
        .wait_with_output(TIMEOUT);
    out.assert_ok();
    let reqs = server.requests();
    assert!(
        reqs.len() >= 3,
        "ожидалось ≥3 запросов (два length + finish), в журнале: {}",
        reqs.len()
    );
    assert_eq!(max_tokens(&reqs[0]), 8192, "стартовый max_output");
    assert_eq!(max_tokens(&reqs[1]), 16384, "после первой эскалации");
    assert_eq!(
        max_tokens(&reqs[2]),
        16384,
        "повторная эскалация запрещена (output_escalated уже поднят)"
    );
}

/// Сценарий 4: hard-deny — bash{command:"rm -rf /"} отклоняется движком прав
/// даже в --yolo; текст отказа (hard-deny + DENIED) обязан уйти модели
/// в следующем запросе.
#[test]
fn hard_deny_rm_rf_visible_to_model() {
    let ws = TempWs::new("hard_deny");
    let server = MockServer::start(vec![
        bash_call("rm -rf /"),
        finish_call("понял отказ, деструктив не выполнялся"),
        spare(),
    ])
    .expect("старт мок-сервера");
    let out = spawn_headless(&ws, &mock_url(&server), "удали всё", &[])
        .expect("spawn theseus")
        .wait_with_output(TIMEOUT);
    out.assert_ok();
    assert!(
        out.stdout.contains("hard-deny"),
        "в stdout обязано быть решение Deny(hard-deny):\n{}",
        out.stdout
    );
    let reqs = server.requests();
    assert!(
        reqs.len() >= 2,
        "ожидался повторный запрос после отказа, в журнале: {}",
        reqs.len()
    );
    assert!(
        reqs[1].body.contains("DENIED") && reqs[1].body.contains("hard-deny"),
        "отказ обязан быть виден модели в теле второго запроса:\n{}",
        reqs[1].body
    );
}

/// Сценарий 4б (граничный): read-only инструмент read_file исполняется
/// параллельной веткой, его результат тоже возвращается модели.
#[test]
fn read_file_tool_result_flows_back() {
    let ws = TempWs::new("read_file");
    let marker = "МАРКЕР_СОДЕРЖИМОГО_42";
    std::fs::write(ws.path().join("note.txt"), marker).expect("запись note.txt");
    let server = MockServer::start(vec![
        MockResponse::tool_call("read_file", "{\"path\": \"note.txt\"}"),
        finish_call("файл прочитан"),
        spare(),
    ])
    .expect("старт мок-сервера");
    let out = spawn_headless(&ws, &mock_url(&server), "прочитай note.txt", &[])
        .expect("spawn theseus")
        .wait_with_output(TIMEOUT);
    out.assert_ok();
    let reqs = server.requests();
    assert!(
        reqs.len() >= 2,
        "ожидался повторный запрос с результатом read_file, в журнале: {}",
        reqs.len()
    );
    assert!(
        reqs[1].body.contains(marker),
        "содержимое файла обязано попасть в следующий запрос:\n{}",
        reqs[1].body
    );
}

/// Граничный случай: неизвестный инструмент — ошибка исполнения возвращается
/// модели как tool_result «ERROR: unknown tool …», цикл продолжается до finish.
#[test]
fn unknown_tool_error_flows_back_to_model() {
    let ws = TempWs::new("unknown_tool");
    let server = MockServer::start(vec![
        MockResponse::tool_call("no_such_tool", "{}"),
        finish_call("понял, такого инструмента нет"),
        spare(),
    ])
    .expect("старт мок-сервера");
    let out = spawn_headless(&ws, &mock_url(&server), "вызови несуществующий инструмент", &[])
        .expect("spawn theseus")
        .wait_with_output(TIMEOUT);
    out.assert_ok();
    let reqs = server.requests();
    assert!(
        reqs.len() >= 2,
        "ожидался повторный запрос после ошибки инструмента, в журнале: {}",
        reqs.len()
    );
    assert!(
        reqs[1].body.contains("ERROR") && reqs[1].body.contains("unknown tool"),
        "ошибка неизвестного инструмента обязана попасть в запрос:\n{}",
        reqs[1].body
    );
}

/// Граничный случай: ответ модели без tool_calls — агент обязан подтолкнуть
/// её continuation-сообщением «Continue with tool calls …», а не завершиться.
#[test]
fn text_only_reply_triggers_continuation_prompt() {
    let ws = TempWs::new("text_only");
    let server = MockServer::start(vec![
        MockResponse::text("я ещё думаю над задачей"),
        finish_call("додумал и завершил"),
        spare(),
    ])
    .expect("старт мок-сервера");
    let out = spawn_headless(&ws, &mock_url(&server), "порассуждай и закончи", &[])
        .expect("spawn theseus")
        .wait_with_output(TIMEOUT);
    out.assert_ok();
    let reqs = server.requests();
    assert!(
        reqs.len() >= 2,
        "ожидался второй запрос после text-only ответа, в журнале: {}",
        reqs.len()
    );
    assert!(
        reqs[1].body.contains("call finish(summary) now"),
        "во втором запросе обязан быть continuation-промпт:\n{}",
        reqs[1].body
    );
}

/// Граничный случай: модель не вызывает finish — цикл обрывается по --max-turns,
/// процесс всё равно завершается кодом 0 с диагностикой лимита в stdout.
#[test]
fn max_turns_limit_terminates_loop() {
    let ws = TempWs::new("max_turns");
    let server = MockServer::start(vec![
        bash_call("echo turn"),
        bash_call("echo turn"),
        bash_call("echo turn"),
        spare(),
    ])
    .expect("старт мок-сервера");
    let out = spawn_headless(&ws, &mock_url(&server), "крутись вечно", &["--max-turns", "3"])
        .expect("spawn theseus")
        .wait_with_output(TIMEOUT);
    // QA-TH-AGENT-002: обрыв по лимиту ходов — ненулевой код (3) для CI,
    // диагностика лимита остаётся в stdout
    assert!(!out.timed_out, "процесс превысил таймаут");
    assert_eq!(
        out.status.code(),
        Some(3),
        "обрыв по max-turns обязан завершаться кодом 3 (QA-TH-AGENT-002): {:?}",
        out.status.code()
    );
    assert!(
        out.stdout.contains("лимит ходов (3)"),
        "в stdout обязана быть диагностика лимита ходов:\n{}",
        out.stdout
    );
    // finish не вызывался → консолидации памяти нет → ровно 3 запроса
    let reqs = server.requests();
    assert_eq!(
        reqs.len(),
        3,
        "без finish журнал обязан содержать ровно max-turns запросов"
    );
}

/// Граничный случай окружения: без DEEPSEEK_API_KEY (и без конфигов в
/// изолированном HOME) процесс обязан падать быстро и с понятной ошибкой,
/// не уходя в сеть.
#[test]
fn missing_api_key_fails_fast() {
    let ws = TempWs::new("no_key");
    let mut cmd = bare_command(&ws); // без ключа; HOME/cwd изолированы
    cmd.arg("-w")
        .arg(ws.path())
        .arg("--base-url")
        .arg("http://127.0.0.1:1") // недостижимый адрес: сеть не должна понадобиться
        .arg("--yolo")
        .arg("-p")
        .arg("что-нибудь");
    let out = ChildGuard::spawn(&mut cmd)
        .expect("spawn theseus")
        .wait_with_output(TIMEOUT);
    assert!(!out.timed_out, "процесс завис без API-ключа:\n{}", out.stderr);
    assert!(
        !out.status.success(),
        "без API-ключа процесс обязан завершаться ошибкой"
    );
    assert!(
        out.stderr.contains("API-ключ") || out.stderr.contains("DEEPSEEK_API_KEY"),
        "в stderr обязана быть подсказка про ключ:\n{}",
        out.stderr
    );
}

/// Сценарий 5а: headless `--sessions` на пустом workspace — честный листинг
/// «сессий нет», код 0, без обращения к API.
#[test]
fn sessions_listing_on_empty_workspace() {
    let ws = TempWs::new("sessions_empty");
    let mut cmd = base_command(&ws);
    cmd.arg("-w").arg(ws.path()).arg("--sessions");
    let out = ChildGuard::spawn(&mut cmd)
        .expect("spawn theseus")
        .wait_with_output(TIMEOUT);
    out.assert_ok();
    assert!(
        out.stdout.contains("сессий нет"),
        "ожидалось сообщение о пустом листинге:\n{}",
        out.stdout
    );
}

/// Сценарий 5б: после headless-прогона снимок сессии сохраняется в
/// `<ws>/.theseus/session-*.json` и виден в листинге `--sessions`.
#[test]
fn sessions_listing_shows_saved_session_after_run() {
    let ws = TempWs::new("sessions_saved");
    let server = MockServer::start(vec![finish_call("сессия сохранена"), spare()])
        .expect("старт мок-сервера");
    let out = spawn_headless(&ws, &mock_url(&server), "поработай и завершись", &[])
        .expect("spawn theseus")
        .wait_with_output(TIMEOUT);
    out.assert_ok();
    // файл сессии на диске
    let dir = ws.path().join(".theseus");
    let sessions: Vec<String> = std::fs::read_dir(&dir)
        .expect("read_dir .theseus")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("session-") && n.ends_with(".json"))
        .collect();
    assert_eq!(
        sessions.len(),
        1,
        "ожидался ровно один session-*.json, найдено: {sessions:?}"
    );
    // листинг через CLI
    let mut cmd = base_command(&ws);
    cmd.arg("-w").arg(ws.path()).arg("--sessions");
    let listing = ChildGuard::spawn(&mut cmd)
        .expect("spawn theseus")
        .wait_with_output(TIMEOUT);
    listing.assert_ok();
    assert!(
        listing.stdout.contains("session-") && listing.stdout.contains(".json"),
        "листинг обязан показать файл сессии:\n{}",
        listing.stdout
    );
    assert!(
        !listing.stdout.contains("сессий нет"),
        "пустой листинг после сохранённой сессии — регресс:\n{}",
        listing.stdout
    );
}
