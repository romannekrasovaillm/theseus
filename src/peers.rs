//! Мост Тесея к внешним CLI-агентам пользователя (peer agents).
//!
//! Для второго мнения или делегирования куска работы Тесей умеет дёргать
//! соседние агентные CLI, установленные на машине, в их headless-режиме
//! (команды проверены на машине пользователя):
//!
//! - Claude Code — `claude --dangerously-skip-permissions -p {task}`
//!   (текстовый ответ в stdout; флаг — чтобы CLI не ждал интерактивного
//!   подтверждения разрешений и не висел до таймаута в headless-захвате);
//! - Kimi Code — `kimi -p {task}`;
//! - CodeWhale — `codewhale exec {task}`;
//! - Hermes Agent — `hermes -z {task}`;
//! - OpenClaw — `openclaw agent --local --session-id theseus
//!   --model deepseek/deepseek-v4-flash --message {task}`
//!   (embedded-режим; primary kimi-local у пользователя бит — прокси :18790
//!   режет тела >30 КБ, поэтому идём напрямую в deepseek).
//!
//! Безопасность — конструкцией: задача подставляется в argv напрямую
//! (`Command::new` + `.args`, shell в цепочке нет), поэтому метасимволы
//! в тексте задачи (`$()`, `;`, `|`, пробелы) остаются литералом одного
//! аргумента — инъекции исключены. stdin агента — /dev/null, stdout и
//! stderr дренируются потоками-насосами одновременно (дедлок-фри), на
//! каждый запуск — дедлайн с kill + reap.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Плейсхолдер задачи в [`PeerSpec::args`]: заменяется текстом задачи
/// внутри argv (без shell — см. документацию модуля).
pub const TASK_PLACEHOLDER: &str = "{task}";

/// Таймаут на `<binary> --version` при пробе агента.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// Максимум символов в строке версии (первая строка вывода `--version`).
const VERSION_MAX_CHARS: usize = 120;
/// Сколько символов хвоста stderr включаем в текст ошибки.
const STDERR_TAIL_CHARS: usize = 800;
/// Максимум символов ответа агента; дальше — обрезка с маркером.
const OUTPUT_MAX_CHARS: usize = 20_000;
/// Шаг опроса `try_wait` в дедлайн-цикле ожидания процесса.
const POLL_STEP: Duration = Duration::from_millis(15);
/// Верхний предел потоков при параллельной пробе нескольких агентов.
const PROBE_THREADS: usize = 5;

/// Спека внешнего агента: как его зовут, что запускать и как долго ждать.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSpec {
    /// Человекочитаемое имя (для таблиц и ошибок): «claude», «kimi», …
    pub name: String,
    /// Имя бинаря (ищется в `PATH`) либо готовый путь к нему.
    pub binary: String,
    /// Аргументы командной строки; `{task}` подставляется текстом задачи.
    pub args: Vec<String>,
    /// Таймаут по умолчанию на один вызов агента, секунд.
    pub default_timeout_secs: u64,
}

/// Результат пробы одного агента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerStatus {
    /// Бинарь нашёлся в `PATH` и исполняем.
    Installed {
        /// Полный путь к найденному бинарю.
        path: PathBuf,
        /// Версия: первая строка вывода `--version` (≤ 120 символов);
        /// «неизвестно», если агент на `--version` не ответил вовремя.
        version: String,
    },
    /// Бинаря нет в `PATH` (или файл существует, но не исполняем).
    Missing,
}

/// Пять штатных агентов-мостов с проверенными headless-режимами.
///
/// Таймауты по умолчанию: 300 с для claude/kimi/codewhale, 600 с для hermes
/// (медленнее — ревью-задачи ~5 минут), 180 с для openclaw.
/// (долгие рассуждения — норма), 180 с для openclaw (embedded-старт медленнее).
///
/// OpenClaw: работающая форма вызова на этой машине — `--local` (embedded,
/// без gateway) и `--session-id` (иначе требует --to/--session-key/--agent)
/// и `--model deepseek/deepseek-v4-flash`: дефолтный primary kimi-local
/// у пользователя ходит в прокси 127.0.0.1:18790, который режет тела
/// свыше 30 КБ (запрос агента со схемами инструментов больше) — прямые
/// вызовы deepseek работают (проверено живьём 19.07).
pub fn builtin_peers() -> Vec<PeerSpec> {
    let spec = |name: &str, binary: &str, args: &[&str], default_timeout_secs: u64| PeerSpec {
        name: name.to_string(),
        binary: binary.to_string(),
        args: args.iter().map(ToString::to_string).collect(),
        default_timeout_secs,
    };
    vec![
        // claude: --dangerously-skip-permissions — иначе в headless-захвате CLI
        // ждёт интерактивного подтверждения разрешений (write/bash) и молча
        // висит до таймаута 300с (живой кейс 21.07: два вызова убиты таймаутом
        // с пустым stderr). Свой гейт разрешений есть на уровне peer_ask.
        spec("claude", "claude", &["--dangerously-skip-permissions", "-p", TASK_PLACEHOLDER], 300),
        spec("kimi", "kimi", &["-p", TASK_PLACEHOLDER], 300),
        spec("codewhale", "codewhale", &["exec", TASK_PLACEHOLDER], 300),
        // hermes: таймаут 600с — ревью-задачи идут у него ~5 минут (несколько
        // последовательных проходов модели): при 300с убивались на финише
        // (живой кейс 22.07 — «Гермес завис»; замер зондом: ответ пришёл ~5.5 мин)
        spec("hermes", "hermes", &["-z", TASK_PLACEHOLDER], 600),
        spec("openclaw", "openclaw",
            &["agent", "--local", "--session-id", "theseus",
              "--model", "deepseek/deepseek-v4-flash", "--message", TASK_PLACEHOLDER],
            180),
    ]
}

/// Проба одного агента: поиск бинаря по компонентам `PATH` (с проверкой
/// executable-бита на unix) + запрос версии с таймаутом [`VERSION_TIMEOUT`].
pub fn probe_peer(spec: &PeerSpec) -> PeerStatus {
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    probe_with_path(spec, &dirs)
}

/// Проба с явным списком каталогов поиска — то же, что [`probe_peer`], но
/// без чтения `PATH` из окружения: для тестов и изолированных сред
/// (env не читается и тем более не мутируется).
pub fn probe_with_path(spec: &PeerSpec, search_dirs: &[PathBuf]) -> PeerStatus {
    let Some(path) = find_binary(&spec.binary, search_dirs) else {
        return PeerStatus::Missing;
    };
    let version = query_version(&path).unwrap_or_else(|| "неизвестно".to_string());
    PeerStatus::Installed { path, version }
}

/// Параллельная проба нескольких агентов: scoped-потоки, не более
/// [`PROBE_THREADS`] одновременно; порядок результата = порядок входа.
pub fn probe_peers(specs: &[PeerSpec]) -> Vec<(PeerSpec, PeerStatus)> {
    if specs.is_empty() {
        return Vec::new();
    }
    let threads = specs.len().min(PROBE_THREADS);
    let chunk = specs.len().div_ceil(threads);
    std::thread::scope(|scope| {
        // Спавним все потоки сразу (ленивая итерация по handles сериализовала
        // бы запуск после join предыдущего — поэтому цикл, а не цепочка).
        let mut handles = Vec::new();
        for part in specs.chunks(chunk) {
            handles.push(scope.spawn(move || {
                part.iter().map(|sp| (sp.clone(), probe_peer(sp))).collect::<Vec<_>>()
            }));
        }
        // probe_peer паниковать не должен; если поток всё же упал —
        // чанк теряется, но остальные агенты доезжают.
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

/// Вызов агента в headless-режиме: подстановка задачи в argv (без shell),
/// запуск в каталоге `cwd`, захват stdout/stderr с дедлайном `timeout`.
///
/// Успех = exit code 0 и непустой stdout; ответ возвращается trimmed и
/// обрезанным до [`OUTPUT_MAX_CHARS`] символов (с маркером обрезки).
/// Любой иной исход — ошибка с хвостом stderr (≤ [`STDERR_TAIL_CHARS`]
/// символов): таймаут (процесс убит и убран), ненулевой exit code,
/// пустой stdout, сбой spawn (бинарь не найден и т.п.).
pub fn peer_ask(spec: &PeerSpec, task: &str, cwd: &Path, timeout: Duration) -> Result<String> {
    let mut cmd = Command::new(&spec.binary);
    cmd.args(render_args(&spec.args, task)).current_dir(cwd);
    let cap = run_capture(&mut cmd, timeout)?;
    let Some(status) = cap.status else {
        bail!(
            "агент «{}» ({}) не ответил за {}с — процесс убит; {}. \
             НЕ повторяйте вызов к этому пиру с теми же аргументами — уменьшите \
             задачу, выберите другого пира или выполните работу сами \
             (живой кейс 21.07: повторный вызов сжёг ещё 300с)",
            spec.name, spec.binary, timeout.as_secs(), stderr_note(&cap.stderr)
        );
    };
    if !status.success() {
        bail!(
            "агент «{}» завершился неуспешно ({status}); {}",
            spec.name, stderr_note(&cap.stderr)
        );
    }
    let out = cap.stdout.trim();
    if out.is_empty() {
        bail!("агент «{}» вернул пустой ответ; {}", spec.name, stderr_note(&cap.stderr));
    }
    Ok(truncate_output(out))
}

/// Таблица проб для вывода пользователю: ✅/❌, имя, бинарь, версия
/// (или «не найден»). Колонки выравниваются по самой длинной строке.
pub fn format_peers(probed: &[(PeerSpec, PeerStatus)]) -> String {
    if probed.is_empty() {
        return "(агенты не заданы)".to_string();
    }
    let name_w = probed.iter().map(|(s, _)| s.name.chars().count()).max().unwrap_or(0).max(5);
    let bin_w = probed.iter().map(|(s, _)| s.binary.chars().count()).max().unwrap_or(0).max(6);
    // 3 ведущих пробела в шапке ≈ ширине «✅ » (эмодзи занимает 2 ячейки).
    let mut lines = vec![format!("   {:<name_w$}  {:<bin_w$}  статус", "агент", "бинарь")];
    for (spec, status) in probed {
        let (mark, info) = match status {
            PeerStatus::Installed { version, .. } => ("✅", version.as_str()),
            PeerStatus::Missing => ("❌", "не найден"),
        };
        lines.push(format!("{mark} {:<name_w$}  {:<bin_w$}  {info}", spec.name, spec.binary));
    }
    lines.join("\n")
}

/// Подстановка задачи в argv: `{task}` заменяется текстом внутри каждого
/// аргумента (в т.ч. внутри составного `--flag={task}`). Shell не участвует,
/// поэтому `$()`, `;`, `|`, пробелы задачи остаются литералом. Если
/// плейсхолдера нет ни в одном аргументе — задача идёт последним аргументом.
fn render_args(args: &[String], task: &str) -> Vec<String> {
    let has_placeholder = args.iter().any(|a| a.contains(TASK_PLACEHOLDER));
    let mut rendered: Vec<String> =
        args.iter().map(|a| a.replace(TASK_PLACEHOLDER, task)).collect();
    if !has_placeholder {
        rendered.push(task.to_string());
    }
    rendered
}

/// Поиск исполняемого файла `binary` по списку каталогов (обход как у PATH).
/// Имя с разделителем пути трактуется как готовый путь и проверяется напрямую.
fn find_binary(binary: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let direct = Path::new(binary);
    if direct.components().count() > 1 {
        return is_executable(direct).then(|| direct.to_path_buf());
    }
    dirs.iter().map(|d| d.join(binary)).find(|p| is_executable(p))
}

/// Файл существует и исполняем (на unix — выставлен хотя бы один x-бит).
fn is_executable(p: &Path) -> bool {
    let Ok(meta) = p.metadata() else {
        return false;
    };
    meta.is_file() && executable_bit(&meta)
}

#[cfg(unix)]
fn executable_bit(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable_bit(_meta: &std::fs::Metadata) -> bool {
    // Вне unix executable-бита нет — достаточно, что это файл.
    true
}

/// Версия агента: `<binary> --version` с жёстким таймаутом; первая непустая
/// строка stdout (или stderr, если stdout пуст), ≤ [`VERSION_MAX_CHARS`].
fn query_version(path: &Path) -> Option<String> {
    let mut cmd = Command::new(path);
    cmd.arg("--version");
    let cap = run_capture(&mut cmd, VERSION_TIMEOUT).ok()?;
    let text = if cap.stdout.trim().is_empty() { &cap.stderr } else { &cap.stdout };
    let first = text.lines().next()?.trim();
    if first.is_empty() {
        return None;
    }
    Some(first.chars().take(VERSION_MAX_CHARS).collect())
}

/// Итог одного захваченного запуска.
struct Captured {
    /// `None` — дедлайн истёк (процесс уже убит и убран).
    status: Option<ExitStatus>,
    stdout: String,
    stderr: String,
}

/// Spawn + потоки-насосы обоих каналов (дедлок-фри) + дедлайн с kill+reap.
/// stdin ребёнка — /dev/null: агент не должен блокироваться на вводе.
fn run_capture(cmd: &mut Command, timeout: Duration) -> Result<Captured> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("не удалось запустить «{}»", cmd.get_program().to_string_lossy()))?;
    let out_pipe = child.stdout.take().context("stdout не запайплен")?;
    let err_pipe = child.stderr.take().context("stderr не запайплен")?;
    let out_pump = spawn_pump(out_pipe);
    let err_pump = spawn_pump(err_pipe);
    let status = wait_deadline(&mut child, timeout)?;
    // Пайпы закрыты (exit или kill) — насосы завершаются по EOF.
    let stdout = out_pump.join().unwrap_or_default();
    let stderr = err_pump.join().unwrap_or_default();
    Ok(Captured {
        status,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// Насос: дочитывает поток до EOF в буфер. Без насосов ребёнок, написавший
/// больше размера pipe-буфера в stderr, повис бы вместе с нами (дедлок).
fn spawn_pump<R: Read + Send + 'static>(mut reader: R) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        buf
    })
}

/// Ожидание процесса с дедлайном: опрос `try_wait` малым шагом; по таймауту —
/// kill и обязательный reap (`wait`), чтобы не оставлять зомби.
/// `Ok(None)` — таймаут (процесс уже убит и убран).
fn wait_deadline(child: &mut Child, timeout: Duration) -> Result<Option<ExitStatus>> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().context("ошибка try_wait")? {
            return Ok(Some(status));
        }
        if start.elapsed() >= timeout {
            child.kill().context("kill по таймауту")?;
            child.wait().context("reap после kill")?;
            return Ok(None);
        }
        std::thread::sleep(POLL_STEP);
    }
}

/// Обрезка длинного ответа: первые [`OUTPUT_MAX_CHARS`] символов + маркер.
fn truncate_output(out: &str) -> String {
    let total = out.chars().count();
    if total <= OUTPUT_MAX_CHARS {
        return out.to_string();
    }
    let head: String = out.chars().take(OUTPUT_MAX_CHARS).collect();
    format!("{head}\n…[Тесей: вывод обрезан, показано {OUTPUT_MAX_CHARS} из {total} символов]")
}

/// Хвост строки длиной ≤ `max` символов (char-safe, с маркером «…» слева).
fn tail_chars(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.trim().to_string();
    }
    let tail: String = s.chars().skip(total - max).collect();
    format!("…{}", tail.trim())
}

/// Фрагмент «stderr: …» для сообщения об ошибке (или «stderr пуст»).
fn stderr_note(stderr: &str) -> String {
    let tail = tail_chars(stderr, STDERR_TAIL_CHARS);
    if tail.is_empty() { "stderr пуст".to_string() } else { format!("stderr: {tail}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static SEQ: AtomicU64 = AtomicU64::new(0);
    /// Сериализует «запись мока + spawn» между параллельными тестами.
    static MOCK_LOCK: Mutex<()> = Mutex::new(());

    /// Уникальный tempdir на тест (pid + счётчик): параллельные тесты
    /// не дерутся за одни пути. Env (`PATH` и прочее) не трогаем нигде.
    fn temp_dir(tag: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("theseus_peers_{}_{}_{}", std::process::id(), tag, n));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Мок-бинарь на bash: исполняемый скрипт `dir/name` с телом `body`.
    /// Вызывать только через [`with_mock`].
    fn mock_binary(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/bash\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// Создать мок и выполнить `f` (обычно — spawn мока через peer_ask/probe)
    /// под глобальной блокировкой. Гонка без лока: fork при spawn наследует
    /// открытые на запись fd из других потоков-тестов (запись своего мока), и
    /// exec такого файла в окне fork→exec даёт ETXTBSY («Text file busy»).
    fn with_mock<R>(dir: &Path, name: &str, body: &str, f: impl FnOnce(&Path) -> R) -> R {
        let _guard = MOCK_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mock = mock_binary(dir, name, body);
        f(&mock)
    }

    /// Спека, указывающая на готовый путь к мок-бинарю.
    fn spec_for(path: &Path, args: &[&str]) -> PeerSpec {
        PeerSpec {
            name: "mock".to_string(),
            binary: path.to_string_lossy().into_owned(),
            args: args.iter().map(ToString::to_string).collect(),
            default_timeout_secs: 60,
        }
    }

    #[test]
    fn render_args_substitutes_task_literally() {
        let args = vec!["-p".to_string(), TASK_PLACEHOLDER.to_string()];
        let task = "$(rm -rf ~); echo hi | cat";
        let rendered = render_args(&args, task);
        assert_eq!(rendered, vec!["-p".to_string(), task.to_string()]);
    }

    #[test]
    fn render_args_substitutes_inside_composite_arg() {
        let args = vec!["--message={task}".to_string(), TASK_PLACEHOLDER.to_string()];
        let rendered = render_args(&args, "привет");
        assert_eq!(rendered, vec!["--message=привет".to_string(), "привет".to_string()]);
    }

    #[test]
    fn render_args_appends_task_when_placeholder_absent() {
        let rendered = render_args(&["-p".to_string()], "задача");
        assert_eq!(rendered, vec!["-p".to_string(), "задача".to_string()]);
    }

    #[test]
    fn builtin_peers_contains_all_five_agents() {
        let peers = builtin_peers();
        assert_eq!(peers.len(), 5);
        let names: Vec<&str> = peers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["claude", "kimi", "codewhale", "hermes", "openclaw"]);
        for p in &peers {
            assert_eq!(p.name, p.binary, "бинарь совпадает с именем: {p:?}");
        }
        let timeouts: Vec<u64> = peers.iter().map(|p| p.default_timeout_secs).collect();
        assert_eq!(timeouts, [300, 300, 300, 600, 180]);
    }

    #[test]
    fn builtin_peers_every_spec_has_task_placeholder() {
        for p in builtin_peers() {
            assert!(
                p.args.iter().any(|a| a.contains(TASK_PLACEHOLDER)),
                "у «{}» нет плейсхолдера {TASK_PLACEHOLDER}: {:?}", p.name, p.args
            );
        }
    }

    /// Ключевой тест безопасности: задача с `$()`, `;`, пробелами доезжает
    /// до агента как ОДИН argv-элемент литералом — shell её не интерпретирует.
    #[test]
    fn peer_ask_passes_task_as_single_argv_without_shell() {
        let dir = temp_dir("argv");
        let argv_file = dir.join("argv.txt");
        let canary = dir.join("pwned");
        let task = format!("сделай $(rm -rf ~) ; touch '{}'", canary.display());
        let body = format!("printf '%s\\n' \"$@\" > '{}'\necho ok", argv_file.display());
        let out = with_mock(&dir, "mock-agent", &body, |mock| {
            let spec = spec_for(mock, &["-p", TASK_PLACEHOLDER]);
            peer_ask(&spec, &task, &dir, Duration::from_secs(10)).unwrap()
        });
        assert_eq!(out, "ok");
        let dumped = fs::read_to_string(&argv_file).unwrap();
        let lines: Vec<&str> = dumped.lines().collect();
        assert_eq!(lines.len(), 2, "argv должен содержать ровно 2 элемента: {dumped:?}");
        assert_eq!(lines[0], "-p");
        assert_eq!(lines[1], task, "задача доехала литералом, одним аргументом");
        assert!(!canary.exists(), "shell-инъекции не было: канарейка не создана");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn peer_ask_returns_trimmed_stdout_on_success() {
        let dir = temp_dir("ok");
        let out = with_mock(&dir, "mock-agent", "echo '  ответ агента  '", |mock| {
            let spec = spec_for(mock, &["-p", TASK_PLACEHOLDER]);
            peer_ask(&spec, "любая задача", &dir, Duration::from_secs(10)).unwrap()
        });
        assert_eq!(out, "ответ агента");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn peer_ask_timeout_kills_process_fast() {
        let dir = temp_dir("timeout");
        let (elapsed, err) = with_mock(&dir, "mock-agent", "exec sleep 30", |mock| {
            let spec = spec_for(mock, &["-p", TASK_PLACEHOLDER]);
            let t0 = Instant::now();
            let err = peer_ask(&spec, "задача", &dir, Duration::from_secs(1)).unwrap_err();
            (t0.elapsed(), err)
        });
        let msg = format!("{err:#}");
        assert!(msg.contains("не ответил"), "ожидали таймаут-ошибку: {msg}");
        assert!(elapsed < Duration::from_secs(8), "kill должен быть быстрым: {elapsed:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn peer_ask_nonzero_exit_reports_stderr() {
        let dir = temp_dir("fail");
        let err = with_mock(&dir, "mock-agent",
            "echo 'фатальная ошибка: токены кончились' >&2; exit 3", |mock| {
                let spec = spec_for(mock, &["-p", TASK_PLACEHOLDER]);
                peer_ask(&spec, "задача", &dir, Duration::from_secs(10)).unwrap_err()
            });
        let msg = format!("{err:#}");
        assert!(msg.contains("exit status: 3"), "код выхода в ошибке: {msg}");
        assert!(msg.contains("токены кончились"), "хвост stderr в ошибке: {msg}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn peer_ask_stderr_tail_is_bounded() {
        let dir = temp_dir("tail");
        // «q» не встречается в служебном тексте ошибки — считаем только хвост stderr.
        let err = with_mock(&dir, "mock-agent",
            "head -c 5000 /dev/zero | tr '\\0' 'q' >&2; exit 1", |mock| {
                let spec = spec_for(mock, &["-p", TASK_PLACEHOLDER]);
                peer_ask(&spec, "задача", &dir, Duration::from_secs(10)).unwrap_err()
            });
        let msg = format!("{err:#}");
        let qs = msg.chars().filter(|c| *c == 'q').count();
        assert!(qs <= STDERR_TAIL_CHARS, "хвост stderr ограничен {STDERR_TAIL_CHARS}: {qs}");
        assert!(msg.contains('…'), "усечение помечено маркером: {msg}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn peer_ask_empty_stdout_is_error() {
        let dir = temp_dir("empty");
        let err = with_mock(&dir, "mock-agent", "exit 0", |mock| {
            let spec = spec_for(mock, &["-p", TASK_PLACEHOLDER]);
            peer_ask(&spec, "задача", &dir, Duration::from_secs(10)).unwrap_err()
        });
        assert!(format!("{err:#}").contains("пустой ответ"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn peer_ask_truncates_huge_output() {
        let dir = temp_dir("huge");
        let out = with_mock(&dir, "mock-agent",
            "head -c 30000 /dev/zero | tr '\\0' 'A'", |mock| {
                let spec = spec_for(mock, &["-p", TASK_PLACEHOLDER]);
                peer_ask(&spec, "задача", &dir, Duration::from_secs(10)).unwrap()
            });
        let big_letters = out.chars().filter(|c| *c == 'A').count();
        assert_eq!(big_letters, OUTPUT_MAX_CHARS, "оставлены первые {OUTPUT_MAX_CHARS}");
        assert!(out.contains("обрезан"), "есть маркер обрезки: {}", &out[out.len() - 120..]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_with_path_finds_executable_in_dirs() {
        let dir = temp_dir("probe");
        let empty = dir.join("empty");
        fs::create_dir_all(&empty).unwrap();
        let bin_dir = dir.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let spec = PeerSpec {
            name: "mock".to_string(),
            binary: "mock-agent".to_string(),
            args: vec![],
            default_timeout_secs: 60,
        };
        let status = with_mock(&bin_dir, "mock-agent", "echo '1.2.3'", |_mock| {
            probe_with_path(&spec, &[empty.clone(), bin_dir.clone()])
        });
        let want = bin_dir.join("mock-agent");
        assert_eq!(status, PeerStatus::Installed { path: want, version: "1.2.3".to_string() });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_with_path_reports_missing() {
        let dir = temp_dir("miss");
        let spec = PeerSpec {
            name: "ghost".to_string(),
            binary: "definitely-not-installed-agent-xyz".to_string(),
            args: vec![],
            default_timeout_secs: 60,
        };
        assert_eq!(probe_with_path(&spec, std::slice::from_ref(&dir)), PeerStatus::Missing);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_with_path_ignores_non_executable_file() {
        let dir = temp_dir("noexec");
        let file = dir.join("mock-agent");
        fs::write(&file, "я не скрипт\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let spec = PeerSpec {
            name: "mock".to_string(),
            binary: "mock-agent".to_string(),
            args: vec![],
            default_timeout_secs: 60,
        };
        assert_eq!(probe_with_path(&spec, std::slice::from_ref(&dir)), PeerStatus::Missing);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_version_takes_first_line_only() {
        let dir = temp_dir("vline");
        let spec = PeerSpec {
            name: "mock".to_string(),
            binary: "mock-agent".to_string(),
            args: vec![],
            default_timeout_secs: 60,
        };
        let status = with_mock(&dir, "mock-agent",
            "echo 'v9.8.7-rc1'\necho 'эта строка не должна попасть'",
            |_mock| probe_with_path(&spec, std::slice::from_ref(&dir)));
        match status {
            PeerStatus::Installed { version, .. } => assert_eq!(version, "v9.8.7-rc1"),
            other => panic!("ожидали Installed: {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_version_truncates_to_120_chars() {
        let dir = temp_dir("vlong");
        let spec = PeerSpec {
            name: "mock".to_string(),
            binary: "mock-agent".to_string(),
            args: vec![],
            default_timeout_secs: 60,
        };
        let status = with_mock(&dir, "mock-agent",
            "head -c 300 /dev/zero | tr '\\0' 'v'; echo",
            |_mock| probe_with_path(&spec, std::slice::from_ref(&dir)));
        match status {
            PeerStatus::Installed { version, .. } =>
                assert_eq!(version.chars().count(), VERSION_MAX_CHARS),
            other => panic!("ожидали Installed: {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_version_timeout_falls_back_to_unknown() {
        let dir = temp_dir("vslow");
        let spec = PeerSpec {
            name: "mock".to_string(),
            binary: "mock-agent".to_string(),
            args: vec![],
            default_timeout_secs: 60,
        };
        let (elapsed, status) = with_mock(&dir, "mock-agent", "exec sleep 30", |_mock| {
            let t0 = Instant::now();
            let status = probe_with_path(&spec, std::slice::from_ref(&dir));
            (t0.elapsed(), status)
        });
        match status {
            PeerStatus::Installed { version, .. } =>
                assert_eq!(version, "неизвестно", "таймаут версии → «неизвестно»"),
            other => panic!("бинарь есть — должен быть Installed: {other:?}"),
        }
        assert!(elapsed < Duration::from_secs(12), "5с таймаут версии: {elapsed:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_peers_preserves_input_order() {
        let specs: Vec<PeerSpec> = ["aa", "bb", "cc", "dd", "ee"]
            .iter()
            .map(|n| PeerSpec {
                name: n.to_string(),
                binary: format!("definitely-not-installed-{n}-xyz"),
                args: vec![TASK_PLACEHOLDER.to_string()],
                default_timeout_secs: 60,
            })
            .collect();
        let probed = probe_peers(&specs);
        assert_eq!(probed.len(), specs.len());
        for (want, (got_spec, got_status)) in specs.iter().zip(probed.iter()) {
            assert_eq!(&got_spec.name, &want.name, "порядок входа сохраняется");
            assert_eq!(*got_status, PeerStatus::Missing);
        }
    }

    #[test]
    fn format_peers_renders_marks_and_columns() {
        let specs = builtin_peers();
        let probed: Vec<(PeerSpec, PeerStatus)> = vec![
            (specs[0].clone(), PeerStatus::Installed {
                path: PathBuf::from("/home/user/.local/bin/claude"),
                version: "2.1.34 (Claude Code)".to_string(),
            }),
            (specs[1].clone(), PeerStatus::Missing),
        ];
        let table = format_peers(&probed);
        assert!(table.contains('✅'), "есть ✅: {table}");
        assert!(table.contains('❌'), "есть ❌: {table}");
        assert!(table.contains("claude") && table.contains("kimi"));
        assert!(table.contains("2.1.34 (Claude Code)"), "версия показана: {table}");
        assert!(table.contains("не найден"), "отсутствующий помечен: {table}");
        assert_eq!(table.lines().count(), 3, "шапка + две строки: {table}");
    }

    #[test]
    fn format_peers_empty_input() {
        assert_eq!(format_peers(&[]), "(агенты не заданы)");
    }
}
