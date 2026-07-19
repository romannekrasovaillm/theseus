//! Расширенные проверки `doctor` — дополнение к `crate::doctor` (он не меняется).
//! Окружение и ФС харнесса: свободное место (`/proc/mounts` + `df`, без libc),
//! git-репозиторий, доступность bash/python3/cargo по `PATH`, regex-движок,
//! каталог сессий, квота транскриптов, сдвиг часов, прокси, UTF-8-локаль.
//! Модуль самодостаточный: `std` + `regex`. Проверки — данные (`Check` с
//! fn-указателем), их можно гонять по одной и на tempdir-фикстурах.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Идентификатор расширенной проверки (по одной на каждый вид).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckId {
    DiskSpace, GitRepo, ShellAvail, PythonAvail, RegexEngine,
    SessionDir, TranscriptQuota, ClockSkew, EnvProxy, LocaleUtf8,
}

impl CheckId {
    /// Стабильный slug для отчётов и машинного вывода.
    pub fn slug(&self) -> &'static str {
        match self {
            CheckId::DiskSpace => "disk_space",
            CheckId::GitRepo => "git_repo",
            CheckId::ShellAvail => "shell_avail",
            CheckId::PythonAvail => "python_avail",
            CheckId::RegexEngine => "regex_engine",
            CheckId::SessionDir => "session_dir",
            CheckId::TranscriptQuota => "transcript_quota",
            CheckId::ClockSkew => "clock_skew",
            CheckId::EnvProxy => "env_proxy",
            CheckId::LocaleUtf8 => "locale_utf8",
        }
    }
}

/// Итог одной проверки: Ok / Warn / Fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    /// Иконка для терминального отчёта (в стиле `crate::doctor`).
    pub fn icon(&self) -> &'static str {
        match self {
            Status::Ok => "✅",
            Status::Warn => "⚠️ ",
            Status::Fail => "❌",
        }
    }
}

/// Результат одной проверки: статус, деталь и подсказка по исправлению.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: Status,
    pub detail: String,
    pub fix_hint: Option<String>,
}

impl Outcome {
    /// Успешный результат.
    pub fn ok(detail: impl Into<String>) -> Self {
        Self { status: Status::Ok, detail: detail.into(), fix_hint: None }
    }

    /// Предупреждение с подсказкой по исправлению.
    pub fn warn(detail: impl Into<String>, fix_hint: impl Into<String>) -> Self {
        Self { status: Status::Warn, detail: detail.into(), fix_hint: Some(fix_hint.into()) }
    }

    /// Провал с подсказкой по исправлению.
    pub fn fail(detail: impl Into<String>, fix_hint: impl Into<String>) -> Self {
        Self { status: Status::Fail, detail: detail.into(), fix_hint: Some(fix_hint.into()) }
    }
}

/// Контекст прогона проверок.
pub struct CheckCtx<'a> {
    /// Корень проекта (workspace).
    pub workspace: &'a Path,
    /// Путь к конфигу харнесса, если известен (зарезервировано для будущих
    /// проверок конфигурации; текущий набор его не читает).
    pub config_path: Option<&'a Path>,
}

impl<'a> CheckCtx<'a> {
    /// Контекст только с workspace, без пути к конфигу.
    pub fn new(workspace: &'a Path) -> Self {
        Self { workspace, config_path: None }
    }

    /// Контекст с путём к конфигу.
    pub fn with_config(workspace: &'a Path, config_path: &'a Path) -> Self {
        Self { workspace, config_path: Some(config_path) }
    }
}

/// Одна проверка: id, человекочитаемое имя и указатель на реализацию.
#[derive(Debug, Clone, Copy)]
pub struct Check {
    pub id: CheckId,
    pub name: &'static str,
    pub run: fn(&CheckCtx) -> Outcome,
}

/// Одна строка отчёта: проверка и её результат.
#[derive(Debug, Clone)]
pub struct ReportEntry {
    pub id: CheckId,
    pub name: &'static str,
    pub outcome: Outcome,
}

/// Отчёт по набору проверок: записи и счётчики по статусам.
#[derive(Debug, Clone)]
pub struct Report {
    pub entries: Vec<ReportEntry>,
    pub ok: usize,
    pub warn: usize,
    pub fail: usize,
}

impl Report {
    /// Всего проверок в отчёте.
    pub fn total(&self) -> usize {
        self.entries.len()
    }

    /// true, если нет ни одного провала.
    pub fn is_healthy(&self) -> bool {
        self.fail == 0
    }

    /// Код выхода в стиле `doctor`: 0 — здоров, 1 — есть провалы.
    pub fn exit_code(&self) -> i32 {
        if self.fail == 0 { 0 } else { 1 }
    }

    /// Терминальный формат: иконка + имя + деталь (+ подсказка), в конце — сводка.
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::from("theseus doctor — расширенные проверки\n\n");
        for e in &self.entries {
            let _ = writeln!(out, "  {} {:<18} {}", e.outcome.status.icon(), e.name, e.outcome.detail);
            if let Some(hint) = &e.outcome.fix_hint {
                let _ = writeln!(out, "     💡 {hint}");
            }
        }
        let _ = writeln!(out, "\nИтог: {} ok / {} warn / {} fail (всего {})", self.ok, self.warn, self.fail, self.total());
        out
    }
}

/// Встроенный набор из 10 расширенных проверок (по одной на каждый `CheckId`).
pub fn builtin_checks() -> Vec<Check> {
    vec![
        Check { id: CheckId::DiskSpace, name: "диск (свободное место)", run: check_disk_space },
        Check { id: CheckId::GitRepo, name: "git-репозиторий", run: check_git_repo },
        Check { id: CheckId::ShellAvail, name: "shell (bash/sh)", run: check_shell_avail },
        Check { id: CheckId::PythonAvail, name: "python3", run: check_python_avail },
        Check { id: CheckId::RegexEngine, name: "regex-движок", run: check_regex_engine },
        Check { id: CheckId::SessionDir, name: "каталог сессий", run: check_session_dir },
        Check { id: CheckId::TranscriptQuota, name: "квота транскриптов", run: check_transcript_quota },
        Check { id: CheckId::ClockSkew, name: "сдвиг часов", run: check_clock_skew },
        Check { id: CheckId::EnvProxy, name: "прокси (env)", run: check_env_proxy },
        Check { id: CheckId::LocaleUtf8, name: "локаль UTF-8", run: check_locale_utf8 },
    ]
}

/// Прогнать все встроенные проверки и собрать отчёт со счётчиками.
pub fn run_all(ctx: &CheckCtx) -> Report {
    let mut entries = Vec::new();
    let mut ok = 0usize;
    let mut warn = 0usize;
    let mut fail = 0usize;
    for check in builtin_checks() {
        let outcome = (check.run)(ctx);
        match outcome.status {
            Status::Ok => ok += 1,
            Status::Warn => warn += 1,
            Status::Fail => fail += 1,
        }
        entries.push(ReportEntry { id: check.id, name: check.name, outcome });
    }
    Report { entries, ok, warn, fail }
}

// --- Пороги -----------------------------------------------------------------
const DISK_FAIL_BYTES: u64 = 100 * 1024 * 1024; // меньше — FAIL
const DISK_WARN_BYTES: u64 = 1024 * 1024 * 1024; // меньше — WARN
/// Квота на суммарный размер транскриптов; превышение — WARN.
const TRANSCRIPT_WARN_BYTES: u64 = 100 * 1024 * 1024;
/// Допустимый сдвиг часов системы относительно FS, секунд.
const CLOCK_SKEW_WARN_SECS: u64 = 120;
/// Переменные локали в порядке приоритета.
const LOCALE_VARS: [&str; 3] = ["LC_ALL", "LC_CTYPE", "LANG"];

// --- 1. DiskSpace: /proc/mounts + `df -Pk` (без libc) -----------------------
fn check_disk_space(ctx: &CheckCtx) -> Outcome {
    let ws = ctx.workspace.canonicalize().unwrap_or_else(|_| ctx.workspace.to_path_buf());
    // Точка монтирования workspace — самый длинный префикс из /proc/mounts.
    let mount = std::fs::read_to_string("/proc/mounts")
        .ok()
        .and_then(|content| {
            let mounts = parse_proc_mounts(&content);
            find_mount_point(&ws, &mounts).map(str::to_string)
        })
        .unwrap_or_else(|| "/".to_string());
    let df = std::process::Command::new("df").arg("-Pk").arg(&mount).output();
    match df {
        Ok(out) if out.status.success() => match parse_df_pk(&String::from_utf8_lossy(&out.stdout)) {
            Some(sample) => disk_outcome(sample.avail_kb.saturating_mul(1024), sample.capacity_pct),
            None => Outcome::warn(format!("не разобрать вывод `df -Pk {mount}`"), "проверьте вывод df вручную"),
        },
        _ => Outcome::warn("утилита df недоступна — свободное место не проверено", "установите coreutils (df)"),
    }
}

/// Разбор `/proc/mounts`: возвращает точки монтирования (с раскодировкой `\040` и т.п.).
fn parse_proc_mounts(content: &str) -> Vec<String> {
    content.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        let _device = fields.next()?;
        Some(unescape_mount(fields.next()?))
    }).collect()
}

/// В /proc/mounts пробел/таб/перевод строки/слэш кодируются восьмерично: \040 \011 \012 \134.
fn unescape_mount(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let digits: String = chars.by_ref().take(3).collect();
            if digits.len() == 3 {
                if let Ok(code) = u8::from_str_radix(&digits, 8) {
                    out.push(char::from(code));
                    continue;
                }
            }
            out.push('\\');
            out.push_str(&digits);
        } else {
            out.push(c);
        }
    }
    out
}

/// Точка монтирования для пути — самый длинный совпавший префикс.
fn find_mount_point<'m>(path: &Path, mounts: &'m [String]) -> Option<&'m str> {
    mounts.iter().filter(|mp| path.starts_with(Path::new(mp.as_str()))).max_by_key(|mp| mp.len()).map(String::as_str)
}

/// Выборка из вывода `df -Pk`: доступно (в 1K-блоках) и заполненность (%).
struct DfSample {
    avail_kb: u64,
    capacity_pct: u16,
}

/// Разбор вывода `df -Pk <mount>`: данные — последняя непустая строка.
/// Колонку Capacity находим по маске «N%», Available — сразу перед ней:
/// так устойчиво к пробелам и в имени устройства, и в точке монтирования.
fn parse_df_pk(output: &str) -> Option<DfSample> {
    let line = output.lines().rfind(|l| !l.trim().is_empty())?;
    if line.starts_with("Filesystem") {
        return None;
    }
    let cols: Vec<&str> = line.split_whitespace().collect();
    let cap_idx = cols.iter().position(|c| {
        c.strip_suffix('%').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })?;
    if cap_idx == 0 {
        return None;
    }
    let capacity_pct = cols[cap_idx].trim_end_matches('%').parse().ok()?;
    let avail_kb = cols[cap_idx - 1].parse().ok()?;
    Some(DfSample { avail_kb, capacity_pct })
}

/// Оценка свободного места по порогам DISK_FAIL/DISK_WARN.
fn disk_outcome(avail_bytes: u64, capacity_pct: u16) -> Outcome {
    let avail = fmt_bytes(avail_bytes);
    if avail_bytes < DISK_FAIL_BYTES {
        Outcome::fail(format!("свободно {avail} (занято {capacity_pct}%) — критически мало"), "освободите место: сессии и транскрипты требуют диск")
    } else if avail_bytes < DISK_WARN_BYTES {
        Outcome::warn(format!("свободно {avail} (занято {capacity_pct}%) — на грани"), "освободите место до следующего длинного прогона")
    } else {
        Outcome::ok(format!("свободно {avail} (занято {capacity_pct}%)"))
    }
}

/// Человекочитаемый размер: Б / КиБ / МиБ / ГиБ (целочисленно).
fn fmt_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB { format!("{} ГиБ", n / GIB) }
    else if n >= MIB { format!("{} МиБ", n / MIB) }
    else if n >= KIB { format!("{} КиБ", n / KIB) }
    else { format!("{n} Б") }
}

// --- 2. GitRepo -------------------------------------------------------------
fn check_git_repo(ctx: &CheckCtx) -> Outcome {
    let dotgit = ctx.workspace.join(".git");
    if dotgit.is_dir() {
        return Outcome::ok("git-репозиторий (.git обнаружен)");
    }
    if dotgit.is_file() {
        return Outcome::ok("git-worktree/submodule (.git-файл)");
    }
    // workspace может быть подкаталогом репозитория — спросим у git.
    let inside = std::process::Command::new("git")
        .arg("-C")
        .arg(ctx.workspace)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match inside {
        Ok(status) if status.success() => Outcome::ok("подкаталог git-репозитория (rev-parse)"),
        _ => Outcome::warn("не git-репозиторий — история изменений недоступна", "git init"),
    }
}

// --- 3/4. ShellAvail, PythonAvail: поиск исполняемых файлов в PATH ----------
/// Найти исполняемый файл `name` в каталогах из строки `path_var` (формат PATH).
fn find_in_path(name: &str, path_var: &str) -> Option<PathBuf> {
    path_var.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| is_executable(candidate))
}

/// Файл существует, регулярный и с хотя бы одним битом исполнения.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

fn check_shell_avail(_ctx: &CheckCtx) -> Outcome {
    let path_var = std::env::var("PATH").unwrap_or_default();
    let bash = find_in_path("bash", &path_var);
    let sh = find_in_path("sh", &path_var);
    // cargo — не требование shell, но харнесс зовёт его для Rust-задач: докладываем.
    let extra = match find_in_path("cargo", &path_var) {
        Some(c) => format!("; cargo: {}", c.display()),
        None => "; cargo не найден".to_string(),
    };
    match (bash, sh) {
        (Some(b), _) => Outcome::ok(format!("bash: {}{extra}", b.display())),
        (None, Some(s)) => Outcome::warn(format!("bash не найден; fallback на sh: {}{extra}", s.display()), "установите bash — shell-инструмент рассчитан на него"),
        (None, None) => Outcome::fail(format!("ни bash, ни sh в PATH{extra}"), "установите bash — shell-инструмент не сможет работать"),
    }
}

fn check_python_avail(_ctx: &CheckCtx) -> Outcome {
    let path_var = std::env::var("PATH").unwrap_or_default();
    match (find_in_path("python3", &path_var), find_in_path("python", &path_var)) {
        (Some(p), _) => Outcome::ok(format!("python3: {}", p.display())),
        (None, Some(p)) => Outcome::warn(format!("python3 не найден; есть только python: {}", p.display()), "установите python3 — скрипты и скиллы пишутся под него"),
        (None, None) => Outcome::warn("python3 не найден в PATH — Python-скрипты недоступны", "apt install python3"),
    }
}

// --- 5. RegexEngine ---------------------------------------------------------
/// Проба regex-движка: паттерн, образец, имя группы и ожидаемое значение.
struct RegexProbe {
    pattern: &'static str,
    sample: &'static str,
    group: &'static str,
    expect: &'static str,
}

/// Сложные паттерны уровня «permissions/парсинг логов»: именованные группы,
/// юникод-классы, квантификаторы, альтернации, незахватывающие группы.
const REGEX_PROBES: &[RegexProbe] = &[
    RegexProbe {
        pattern: r"(?P<ts>\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,6})?(?:Z|[+-]\d{2}:\d{2}))\s+\[(?P<level>TRACE|DEBUG|INFO|WARN|ERROR)\]\s+(?P<msg>.+)",
        sample: "2026-07-18T12:00:01Z [WARN] диск почти заполнен",
        group: "level",
        expect: "WARN",
    },
    RegexProbe {
        pattern: r"(?P<user>[\p{L}\p{N}._%+-]+)@(?P<domain>[\p{L}\p{N}.-]+\.[\p{L}]{2,})",
        sample: "пишите на dev.ops-team@bank-example.ru завтра",
        group: "domain",
        expect: "bank-example.ru",
    },
];

/// Прогон проб: компиляция + captures + сверка именованной группы.
fn run_regex_probes(probes: &[RegexProbe]) -> Result<usize, String> {
    for p in probes {
        let re = regex::Regex::new(p.pattern).map_err(|e| format!("шаблон «{}»: {e}", p.pattern))?;
        let caps = re.captures(p.sample).ok_or_else(|| format!("шаблон «{}» не совпал с образцом «{}»", p.pattern, p.sample))?;
        let got = caps.name(p.group).map(|m| m.as_str()).ok_or_else(|| format!("в captures нет группы «{}»", p.group))?;
        if got != p.expect {
            return Err(format!("группа «{}»: ожидали «{}», получили «{got}»", p.group, p.expect));
        }
    }
    Ok(probes.len())
}

fn check_regex_engine(_ctx: &CheckCtx) -> Outcome {
    match run_regex_probes(REGEX_PROBES) {
        Ok(n) => Outcome::ok(format!("{n} сложных паттерна: компиляция и named captures ok")),
        Err(e) => Outcome::fail(format!("regex-движок сломан: {e}"), "regex критичен для правил permissions — проверьте тулчейн/сборку"),
    }
}

// --- 6. SessionDir ----------------------------------------------------------
/// Каталог сессий харнесса внутри workspace.
fn sessions_dir(ctx: &CheckCtx) -> PathBuf {
    ctx.workspace.join(".theseus").join("sessions")
}

fn check_session_dir(ctx: &CheckCtx) -> Outcome {
    if !ctx.workspace.is_dir() {
        return Outcome::fail(format!("workspace не каталог: {}", ctx.workspace.display()), "укажите существующий каталог проекта");
    }
    let dir = sessions_dir(ctx);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Outcome::fail(format!("не создать {}: {e}", dir.display()), "проверьте права на .theseus");
    }
    let probe = dir.join(format!(".probe-{}", std::process::id()));
    match std::fs::write(&probe, b"ok").and_then(|_| std::fs::remove_file(&probe)) {
        Ok(()) => Outcome::ok(format!("{} — создание/запись ok", dir.display())),
        Err(e) => Outcome::fail(format!("запись в {}: {e}", dir.display()), "проверьте права/квоту каталога сессий"),
    }
}

// --- 7. TranscriptQuota -----------------------------------------------------
/// Каталог транскриптов харнесса внутри workspace.
fn transcripts_dir(ctx: &CheckCtx) -> PathBuf {
    ctx.workspace.join(".theseus").join("transcripts")
}

fn check_transcript_quota(ctx: &CheckCtx) -> Outcome {
    let dir = transcripts_dir(ctx);
    if !dir.exists() {
        return Outcome::ok(format!("{} пока нет — расход 0 Б", dir.display()));
    }
    quota_outcome(dir_size_bytes(&dir), TRANSCRIPT_WARN_BYTES)
}

/// Оценка расхода против квоты (строгое превышение — WARN).
fn quota_outcome(total: u64, limit: u64) -> Outcome {
    let total_h = fmt_bytes(total);
    let limit_h = fmt_bytes(limit);
    if total > limit {
        Outcome::warn(format!("транскрипты занимают {total_h} при квоте {limit_h}"), "запустите `theseus session prune` — старые транскрипты будут удалены")
    } else {
        Outcome::ok(format!("транскрипты: {total_h} из {limit_h}"))
    }
}

/// Суммарный размер файлов в каталоге (рекурсивно, без ухода по симлинкам).
fn dir_size_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                Ok(t) if t.is_file() => {
                    if let Ok(meta) = entry.metadata() { total += meta.len(); }
                }
                _ => {}
            }
        }
    }
    total
}

// --- 8. ClockSkew -----------------------------------------------------------
fn check_clock_skew(ctx: &CheckCtx) -> Outcome {
    let probe = ctx.workspace.join(format!(".clock-probe-{}", std::process::id()));
    let created = std::fs::write(&probe, b"t").and_then(|_| std::fs::metadata(&probe));
    let result = match created {
        Ok(meta) => match meta.modified() {
            Ok(mtime) => assess_skew(skew_seconds(mtime, SystemTime::now())),
            Err(e) => Outcome::warn(format!("mtime probe-файла недоступен: {e}"), "проверьте поддержку mtime файловой системой"),
        },
        Err(e) => Outcome::warn(format!("не создать probe-файл в workspace: {e}"), "проверка сдвига часов пропущена"),
    };
    let _ = std::fs::remove_file(&probe);
    result
}

/// Сдвиг в секундах: >0 — mtime в будущем (часы системы отстают), <0 — в прошлом.
fn skew_seconds(mtime: SystemTime, now: SystemTime) -> i64 {
    match now.duration_since(mtime) {
        Ok(back) => -clamp_secs(back),
        Err(future) => clamp_secs(future.duration()),
    }
}

/// Duration → i64 секунд с насыщением по i64::MAX.
fn clamp_secs(d: Duration) -> i64 {
    let secs = d.as_secs().min(i64::MAX as u64);
    i64::try_from(secs).unwrap_or(i64::MAX)
}

/// Оценка сдвига часов против допуска CLOCK_SKEW_WARN_SECS.
fn assess_skew(delta_secs: i64) -> Outcome {
    let abs = delta_secs.unsigned_abs();
    if abs <= CLOCK_SKEW_WARN_SECS {
        Outcome::ok(format!("сдвиг часов относительно FS: {delta_secs} с (в норме)"))
    } else if delta_secs > 0 {
        Outcome::warn(format!("mtime свежего файла в будущем на {abs} с — системные часы отстают"), "синхронизируйте время: timedatectl set-ntp true")
    } else {
        Outcome::warn(format!("mtime свежего файла в прошлом на {abs} с — системные часы спешат"), "синхронизируйте время: timedatectl set-ntp true")
    }
}

// --- 9. EnvProxy ------------------------------------------------------------
fn check_env_proxy(_ctx: &CheckCtx) -> Outcome {
    let nonempty = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    let http = nonempty("http_proxy").or_else(|| nonempty("HTTP_PROXY"));
    let https = nonempty("https_proxy").or_else(|| nonempty("HTTPS_PROXY"));
    proxy_outcome(http.as_deref(), https.as_deref())
}

/// Оценка пары прокси-переменных: не заданы — ок, заданы — должны быть со схемой.
fn proxy_outcome(http: Option<&str>, https: Option<&str>) -> Outcome {
    let mut valid: Vec<String> = Vec::new();
    let mut invalid: Vec<String> = Vec::new();
    for (label, value) in [("http_proxy", http), ("https_proxy", https)] {
        match value {
            Some(v) if proxy_url_valid(v) => valid.push(format!("{label}={v}")),
            Some(v) => invalid.push(format!("{label}={v}")),
            None => {}
        }
    }
    if invalid.is_empty() {
        if valid.is_empty() {
            Outcome::ok("прокси не задан — прямые соединения с API")
        } else {
            Outcome::ok(format!("прокси: {}", valid.join(", ")))
        }
    } else {
        Outcome::warn(format!("прокси без схемы: {}", invalid.join(", ")), "задавайте прокси полностью, например http://proxy.local:3128")
    }
}

/// Прокси-URL должен начинаться с явной схемы.
fn proxy_url_valid(v: &str) -> bool {
    v.starts_with("http://") || v.starts_with("https://") || v.starts_with("socks5://") || v.starts_with("socks5h://")
}

// --- 10. LocaleUtf8 ---------------------------------------------------------
fn check_locale_utf8(_ctx: &CheckCtx) -> Outcome {
    let found = LOCALE_VARS.iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (*k, v)))
        .find(|(_, v)| locale_is_utf8(v));
    if let Some((key, val)) = found {
        return Outcome::ok(format!("{key}={val}"));
    }
    let current = LOCALE_VARS.iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| format!("{k}={v}")))
        .collect::<Vec<_>>().join(" ");
    let shown = if current.is_empty() { "LC_ALL/LC_CTYPE/LANG не заданы".to_string() } else { current };
    Outcome::warn(format!("локаль без UTF-8 ({shown}) — юникод/иконки могут отображаться бито"), "export LANG=C.UTF-8")
}

/// Строка локали указывает на UTF-8 (`UTF-8`/`utf8`, регистр не важен).
fn locale_is_utf8(v: &str) -> bool {
    let upper = v.to_uppercase();
    upper.contains("UTF-8") || upper.contains("UTF8")
}

// --- Тесты ------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use std::sync::Mutex;

    /// Сериализация тестов, мутирующих переменные окружения (env — глобальный).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Временный каталог с автоудалением.
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("theseus-doc-ext-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Снимок и установка env-переменных с восстановлением при выходе из скоупа.
    struct EnvGuard(Vec<(&'static str, Option<String>)>);

    impl EnvGuard {
        fn apply(vars: &[(&'static str, Option<&str>)]) -> Self {
            let saved = vars.iter().map(|(k, _)| (*k, std::env::var(k).ok())).collect();
            for (k, v) in vars {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
            Self(saved)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.0 {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    fn ctx_for(dir: &TmpDir) -> CheckCtx<'_> {
        CheckCtx::new(dir.path())
    }

    #[test]
    fn proc_mounts_parse_and_longest_prefix() {
        let content = "proc /proc proc rw,nosuid,nodev,noexec 0 0\n\
                       /dev/sda1 / ext4 rw,relatime 0 0\n\
                       /dev/sda2 /home ext4 rw 0 0\n\
                       //server/share /mnt/my\\040share cifs rw 0 0\n";
        let mounts = parse_proc_mounts(content);
        assert_eq!(mounts.len(), 4);
        assert_eq!(mounts[1], "/");
        assert_eq!(mounts[3], "/mnt/my share"); // \040 раскодирован в пробел
        assert_eq!(find_mount_point(Path::new("/home/user/proj"), &mounts), Some("/home"));
        assert_eq!(find_mount_point(Path::new("/var/log"), &mounts), Some("/"));
        // относительный путь не матчится с абсолютными точками монтирования
        assert_eq!(find_mount_point(Path::new("relative/dir"), &mounts), None);
    }

    #[test]
    fn df_pk_standard_and_edge_cases() {
        let out = "Filesystem     1024-blocks      Used Available Capacity Mounted on\n\
                   /dev/sda1        490233412 390233412  75000000      84% /\n";
        let s = parse_df_pk(out).unwrap();
        assert_eq!(s.avail_kb, 75_000_000);
        assert_eq!(s.capacity_pct, 84);
        // длинное имя устройства и точка монтирования с пробелом: колонка «N%» всё равно находится
        let spaced = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                      very-long-device-name 1000 500 400 50% /mnt/my disk\n";
        let s = parse_df_pk(spaced).unwrap();
        assert_eq!(s.avail_kb, 400);
        assert_eq!(s.capacity_pct, 50);
        assert!(parse_df_pk("Filesystem 1024-blocks Used Available Capacity Mounted on\n").is_none());
        assert!(parse_df_pk("").is_none());
        assert!(parse_df_pk("garbage line\n").is_none());
    }

    #[test]
    fn disk_outcome_thresholds_boundaries_and_fmt() {
        assert_eq!(disk_outcome(DISK_WARN_BYTES + 1, 40).status, Status::Ok);
        assert_eq!(disk_outcome(DISK_WARN_BYTES, 40).status, Status::Ok);
        assert_eq!(disk_outcome(DISK_WARN_BYTES - 1, 90).status, Status::Warn);
        assert_eq!(disk_outcome(DISK_FAIL_BYTES, 95).status, Status::Warn);
        let fail = disk_outcome(DISK_FAIL_BYTES - 1, 99);
        assert_eq!(fail.status, Status::Fail);
        assert!(fail.fix_hint.is_some());
        assert!(fail.detail.contains("свободно"));
        assert_eq!(fmt_bytes(0), "0 Б");
        assert_eq!(fmt_bytes(512), "512 Б");
        assert_eq!(fmt_bytes(1024), "1 КиБ");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5 МиБ");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3 ГиБ");
    }

    #[test]
    fn disk_space_smoke_on_real_fs() {
        let tmp = TmpDir::new("disk");
        let ctx = ctx_for(&tmp);
        let out = check_disk_space(&ctx);
        // df доступен → деталь про свободное место; иначе — честный warn про df
        assert!(out.detail.contains("свободно") || out.detail.contains("df"), "detail: {}", out.detail);
    }

    #[test]
    fn find_in_path_requires_executable() {
        let tmp = TmpDir::new("path");
        let exe = tmp.path().join("fakesh");
        fs::write(&exe, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let path_var = format!("{}:/nonexistent", tmp.path().display());
        assert_eq!(find_in_path("fakesh", &path_var).as_deref(), Some(exe.as_path()));
        assert!(find_in_path("nosuch", &path_var).is_none());
        // регулярный файл без бита исполнения — не находится
        let plain = tmp.path().join("plain");
        fs::write(&plain, "x").unwrap();
        assert!(find_in_path("plain", &path_var).is_none());
    }

    #[test]
    fn shell_and_python_checks_ok_on_this_host() {
        // на машине разработки bash и python3 точно есть — иначе упадёт многое другое
        let tmp = TmpDir::new("shellpy");
        let ctx = ctx_for(&tmp);
        assert_eq!(check_shell_avail(&ctx).status, Status::Ok);
        assert_eq!(check_python_avail(&ctx).status, Status::Ok);
    }

    #[test]
    fn git_repo_detects_dotgit() {
        let tmp = TmpDir::new("git");
        let ctx = ctx_for(&tmp);
        assert_eq!(check_git_repo(&ctx).status, Status::Warn);
        fs::create_dir(tmp.path().join(".git")).unwrap();
        assert_eq!(check_git_repo(&ctx).status, Status::Ok);
    }

    #[test]
    fn session_dir_ok_and_fail() {
        let tmp = TmpDir::new("sess");
        let ctx = ctx_for(&tmp);
        let out = check_session_dir(&ctx);
        assert_eq!(out.status, Status::Ok);
        assert!(sessions_dir(&ctx).is_dir());
        // probe-файл удалён за собой
        assert_eq!(fs::read_dir(sessions_dir(&ctx)).unwrap().count(), 0);
        // workspace — регулярный файл: провал с подсказкой
        let file = tmp.path().join("not-a-dir");
        fs::write(&file, "x").unwrap();
        let bad = check_session_dir(&CheckCtx::new(&file));
        assert_eq!(bad.status, Status::Fail);
        assert!(bad.fix_hint.is_some());
    }

    #[test]
    fn transcript_quota_nested_files_and_boundary() {
        let tmp = TmpDir::new("quota");
        let nested = tmp.path().join(".theseus").join("transcripts").join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("a.jsonl"), vec![0u8; 1000]).unwrap();
        fs::write(nested.join("b.jsonl"), vec![0u8; 2000]).unwrap();
        assert_eq!(dir_size_bytes(&tmp.path().join(".theseus")), 3000);
        let ctx = ctx_for(&tmp);
        let out = check_transcript_quota(&ctx);
        assert_eq!(out.status, Status::Ok);
        assert!(out.detail.contains("2 КиБ"), "detail: {}", out.detail);
        // квота строгая: ровно лимит — ещё ок, лимит+1 — уже warn
        assert_eq!(quota_outcome(TRANSCRIPT_WARN_BYTES, TRANSCRIPT_WARN_BYTES).status, Status::Ok);
        let over = quota_outcome(TRANSCRIPT_WARN_BYTES + 1, TRANSCRIPT_WARN_BYTES);
        assert_eq!(over.status, Status::Warn);
        assert!(over.fix_hint.is_some());
    }

    #[test]
    fn clock_skew_assessment_and_real_fs() {
        assert_eq!(assess_skew(0).status, Status::Ok);
        assert_eq!(assess_skew(120).status, Status::Ok);
        assert_eq!(assess_skew(-120).status, Status::Ok);
        assert_eq!(assess_skew(121).status, Status::Warn);
        assert_eq!(assess_skew(-121).status, Status::Warn);
        assert!(assess_skew(300).detail.contains("отстают"));
        assert!(assess_skew(-300).detail.contains("спешат"));
        // на здоровой FS сдвига нет; probe-файл убран за собой
        let tmp = TmpDir::new("clock");
        let ctx = ctx_for(&tmp);
        let out = check_clock_skew(&ctx);
        assert_eq!(out.status, Status::Ok, "detail: {}", out.detail);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn regex_probes_pass_and_detect_errors() {
        assert_eq!(run_regex_probes(REGEX_PROBES), Ok(2));
        let bad = [RegexProbe { pattern: "(unclosed", sample: "x", group: "g", expect: "y" }];
        assert!(run_regex_probes(&bad).is_err());
        let no_match = [RegexProbe { pattern: r"(?P<w>\d+)", sample: "abc", group: "w", expect: "1" }];
        assert!(run_regex_probes(&no_match).is_err());
        let wrong = [RegexProbe { pattern: r"(?P<w>\d+)", sample: "a12", group: "w", expect: "99" }];
        assert!(run_regex_probes(&wrong).unwrap_err().contains("ожидали"));
    }

    #[test]
    fn proxy_outcome_pure_and_env_serialized() {
        let _lock = ENV_LOCK.lock().unwrap();
        assert_eq!(proxy_outcome(None, None).status, Status::Ok);
        assert_eq!(proxy_outcome(Some("http://proxy.local:3128"), None).status, Status::Ok);
        assert_eq!(proxy_outcome(Some("http://ok:1"), Some("socks5://s:2")).status, Status::Ok);
        let bad = proxy_outcome(Some("proxy.local:3128"), None);
        assert_eq!(bad.status, Status::Warn);
        assert!(bad.detail.contains("proxy.local:3128"));
        assert!(bad.fix_hint.is_some());
        assert_eq!(proxy_outcome(None, Some("no-scheme")).status, Status::Warn);
        // чтение env: только http_proxy задан корректно
        let _guard = EnvGuard::apply(&[
            ("http_proxy", Some("http://proxy.local:3128")),
            ("HTTP_PROXY", None), ("https_proxy", None), ("HTTPS_PROXY", None),
        ]);
        let tmp = TmpDir::new("proxy");
        let out = check_env_proxy(&ctx_for(&tmp));
        assert_eq!(out.status, Status::Ok);
        assert!(out.detail.contains("http://proxy.local:3128"), "detail: {}", out.detail);
    }

    #[test]
    fn locale_utf8_pure_and_env_serialized() {
        let _lock = ENV_LOCK.lock().unwrap();
        assert!(locale_is_utf8("en_US.UTF-8"));
        assert!(locale_is_utf8("ru_RU.utf8"));
        assert!(locale_is_utf8("C.UTF-8"));
        assert!(!locale_is_utf8("C"));
        assert!(!locale_is_utf8("POSIX"));
        assert!(!locale_is_utf8("ru_RU.KOI8-R"));
        let tmp = TmpDir::new("locale");
        let ctx = ctx_for(&tmp);
        {
            let _g = EnvGuard::apply(&[("LC_ALL", None), ("LC_CTYPE", None), ("LANG", Some("C"))]);
            assert_eq!(check_locale_utf8(&ctx).status, Status::Warn);
        }
        {
            let _g = EnvGuard::apply(&[("LC_ALL", None), ("LC_CTYPE", None), ("LANG", Some("ru_RU.UTF-8"))]);
            assert_eq!(check_locale_utf8(&ctx).status, Status::Ok);
        }
    }

    #[test]
    fn builtin_checks_ten_unique_ids_and_slugs() {
        let checks = builtin_checks();
        assert_eq!(checks.len(), 10);
        let ids: HashSet<CheckId> = checks.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), 10);
        let slugs: HashSet<&str> = checks.iter().map(|c| c.id.slug()).collect();
        assert_eq!(slugs.len(), 10);
        assert!(checks.iter().all(|c| !c.name.is_empty()));
    }

    #[test]
    fn report_render_exact_format() {
        let entries = vec![
            ReportEntry { id: CheckId::GitRepo, name: "git-репозиторий", outcome: Outcome::ok("всё хорошо") },
            ReportEntry { id: CheckId::DiskSpace, name: "диск", outcome: Outcome::fail("мало места", "очистите диск") },
        ];
        let report = Report { entries, ok: 1, warn: 0, fail: 1 };
        let text = report.render();
        assert!(text.contains("✅"));
        assert!(text.contains("❌"));
        assert!(text.contains("git-репозиторий"));
        assert!(text.contains("всё хорошо"));
        assert!(text.contains("💡 очистите диск"));
        assert!(text.contains("Итог: 1 ok / 0 warn / 1 fail (всего 2)"));
        assert!(!report.is_healthy());
        assert_eq!(report.exit_code(), 1);
        assert_eq!(report.total(), 2);
    }

    #[test]
    fn run_all_counts_and_consistency() {
        let _lock = ENV_LOCK.lock().unwrap();
        // фиксируем env-зависимые проверки: прокси не задан, локаль — UTF-8
        let _guard = EnvGuard::apply(&[
            ("http_proxy", None), ("HTTP_PROXY", None),
            ("https_proxy", None), ("HTTPS_PROXY", None),
            ("LC_ALL", None), ("LC_CTYPE", None), ("LANG", Some("C.UTF-8")),
        ]);
        let tmp = TmpDir::new("runall");
        let ctx = ctx_for(&tmp);
        let report = run_all(&ctx);
        assert_eq!(report.entries.len(), 10);
        assert_eq!(report.ok + report.warn + report.fail, 10);
        assert_eq!(report.total(), 10);
        assert_eq!(report.is_healthy(), report.fail == 0);
        assert_eq!(report.exit_code(), i32::from(!report.is_healthy()));
        // счётчики согласованы с фактическими статусами записей
        let actual_fail = report.entries.iter().filter(|e| e.outcome.status == Status::Fail).count();
        assert_eq!(report.fail, actual_fail);
        let text = report.render();
        assert!(text.contains("Итог:"));
        assert!(text.contains("regex-движок"));
    }
}
