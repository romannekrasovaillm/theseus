//! Реализации `--fix` для `doctor` — дополнение к `crate::doctor` и
//! `crate::doctor_ext` (они не меняются). Модули-диагносты возвращают строки
//! проблем; этот модуль превращает их в план действий и применяет исправления:
//!
//! - [`plan_fixes`] — маппинг строк проблем (RU/EN, без учёта регистра) в
//!   уникальные [`FixAction`] в каноническом порядке; уже удовлетворённые
//!   цели (по состоянию ФС) из плана исключаются — план идемпотентен;
//! - [`apply_fix`] — применяет одно действие и отчитывается [`FixResult`]
//!   (`did_something = false`, когда менять нечего — повторный запуск честный);
//! - [`dry_run`] — человекочитаемое описание плана без изменений на диске;
//! - [`summary`] — итоговая сводка по результатам в стиле `doctor`;
//! - [`fix_all`] — удобная связка «план + применить» для `main`.
//!
//! Модуль самодостаточный: только `std`. Unix-специфика (chmod 0755) собрана
//! через `#[cfg(unix)]`, на остальных платформах действие честно пропускается.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

/// Квота на суммарный размер транскриптов (events-файлов), байт.
/// Совпадает с порогом WARN в `doctor_ext` (100 МиБ): превышение — повод чистки.
pub const TRANSCRIPT_QUOTA_BYTES: u64 = 100 * 1024 * 1024;

/// Действие исправления, доступное `doctor --fix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FixAction {
    /// Создать пример глобального конфига `~/.theseus/config.toml`
    /// (существующий файл никогда не перезаписывается).
    CreateConfig,
    /// Выставить права 0755 на каталог workspace (только unix).
    FixWorkspacePerms,
    /// Создать каталог сессий `<workspace>/.theseus/sessions`.
    CreateSessionDir,
    /// Создать каталог скиллов `<workspace>/.theseus/skills`.
    CreateSkillsDir,
    /// Удалить старейшие `events-*.jsonl` сверх квоты [`TRANSCRIPT_QUOTA_BYTES`].
    TrimTranscripts,
}

impl FixAction {
    /// Все действия в каноническом порядке плана (порядок применения).
    pub const ALL: [FixAction; 5] = [
        FixAction::CreateConfig,
        FixAction::FixWorkspacePerms,
        FixAction::CreateSessionDir,
        FixAction::CreateSkillsDir,
        FixAction::TrimTranscripts,
    ];

    /// Стабильный slug для машинного вывода и сводок.
    pub fn slug(&self) -> &'static str {
        match self {
            FixAction::CreateConfig => "create_config",
            FixAction::FixWorkspacePerms => "fix_workspace_perms",
            FixAction::CreateSessionDir => "create_session_dir",
            FixAction::CreateSkillsDir => "create_skills_dir",
            FixAction::TrimTranscripts => "trim_transcripts",
        }
    }

    /// Короткое русское название для отчётов.
    pub fn title(&self) -> &'static str {
        match self {
            FixAction::CreateConfig => "создание конфига",
            FixAction::FixWorkspacePerms => "права на workspace",
            FixAction::CreateSessionDir => "каталог сессий",
            FixAction::CreateSkillsDir => "каталог скиллов",
            FixAction::TrimTranscripts => "чистка транскриптов",
        }
    }

    /// Что именно будет сделано (с конкретными путями) — для dry-run.
    pub fn describe(&self, home: &Path, workspace: &Path) -> String {
        match self {
            FixAction::CreateConfig => {
                format!("создать пример конфига {}", global_config_path(home).display())
            }
            FixAction::FixWorkspacePerms => {
                format!("выставить права 0755 на каталог {}", workspace.display())
            }
            FixAction::CreateSessionDir => {
                format!("создать каталог сессий {}", sessions_dir(workspace).display())
            }
            FixAction::CreateSkillsDir => {
                format!("создать каталог скиллов {}", skills_dir(workspace).display())
            }
            FixAction::TrimTranscripts => format!(
                "удалить старейшие events-*.jsonl в {} сверх квоты {}",
                transcripts_root(workspace).display(),
                fmt_bytes(TRANSCRIPT_QUOTA_BYTES)
            ),
        }
    }
}

/// Результат применения одного действия.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixResult {
    /// Какое действие применялось.
    pub action: FixAction,
    /// true, если на диске что-то изменилось; false — уже в порядке или ошибка.
    pub did_something: bool,
    /// Человекочитаемая деталь (путь, числа, либо текст ошибки с «ошибка: »).
    pub detail: String,
}

impl FixResult {
    /// Действие реально изменило состояние.
    fn done(action: FixAction, detail: String) -> Self {
        Self { action, did_something: true, detail }
    }

    /// Менять не потребовалось (идемпотентный повторный запуск).
    fn skipped(action: FixAction, detail: String) -> Self {
        Self { action, did_something: false, detail }
    }

    /// Применение не удалось: did_something = false, деталь с префиксом «ошибка: ».
    fn failed(action: FixAction, detail: String) -> Self {
        Self { action, did_something: false, detail: format!("ошибка: {detail}") }
    }
}

/// Ключевые подстроки (нижний регистр) строк проблем doctor → действие.
/// Покрываем формулировки `doctor`/`doctor_ext` и типовые тексты io-ошибок.
const ISSUE_KEYWORDS: &[(FixAction, &[&str])] = &[
    (FixAction::CreateConfig, &["конфиг", "config.toml", "config"]),
    (FixAction::FixWorkspacePerms, &["прав", "запис", "permission", "denied"]),
    (FixAction::CreateSessionDir, &["сесси", "session"]),
    (FixAction::CreateSkillsDir, &["скилл", "skill"]),
    (FixAction::TrimTranscripts, &["транскрипт", "квот", "transcript"]),
];

/// Строка проблемы (уже в нижнем регистре) относится к действию?
fn issue_matches(action: FixAction, issue_lower: &str) -> bool {
    ISSUE_KEYWORDS.iter().any(|(a, keywords)| {
        *a == action && keywords.iter().any(|kw| issue_lower.contains(kw))
    })
}

/// Построить план исправлений по строкам проблем из `doctor`.
///
/// - регистр и язык строк не важны (RU/EN, матч по подстрокам);
/// - действия уникальны и идут в каноническом порядке [`FixAction::ALL`]
///   независимо от порядка и повторов входных строк;
/// - действия, чья цель уже удовлетворена на ФС (конфиг существует, каталог
///   есть, права уже 0755, квота не превышена), в план не попадают —
///   повторный `plan_fixes` после успешного `fix_all` вернёт пустой вектор.
pub fn plan_fixes(home: &Path, workspace: &Path, issues: &[String]) -> Vec<FixAction> {
    let lowered: Vec<String> = issues.iter().map(|i| i.to_lowercase()).collect();
    FixAction::ALL
        .into_iter()
        .filter(|a| lowered.iter().any(|issue| issue_matches(*a, issue)))
        .filter(|a| !already_satisfied(*a, home, workspace))
        .collect()
}

/// Применить одно действие. Всегда возвращает [`FixResult`]: ошибки ФС
/// складываются в `detail` с префиксом «ошибка: », не паникуем и не падаем.
pub fn apply_fix(action: FixAction, home: &Path, workspace: &Path) -> FixResult {
    match action {
        FixAction::CreateConfig => apply_create_config(home),
        FixAction::FixWorkspacePerms => apply_workspace_perms(workspace),
        FixAction::CreateSessionDir => {
            apply_create_dir(action, &sessions_dir(workspace), "каталог сессий")
        }
        FixAction::CreateSkillsDir => {
            apply_create_dir(action, &skills_dir(workspace), "каталог скиллов")
        }
        FixAction::TrimTranscripts => apply_trim_transcripts(workspace),
    }
}

/// Спланировать и сразу применить все исправления по строкам проблем.
pub fn fix_all(home: &Path, workspace: &Path, issues: &[String]) -> Vec<FixResult> {
    plan_fixes(home, workspace, issues)
        .into_iter()
        .map(|a| apply_fix(a, home, workspace))
        .collect()
}

/// Dry-run: человекочитаемое описание плана, ничего не меняя на диске.
pub fn dry_run(actions: &[FixAction], home: &Path, workspace: &Path) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("theseus doctor --fix (dry-run)\n");
    if actions.is_empty() {
        let _ = writeln!(out, "  действий не требуется");
        return out;
    }
    for (i, a) in actions.iter().enumerate() {
        let _ = writeln!(out, "  {}. {} — {}", i + 1, a.title(), a.describe(home, workspace));
    }
    let _ = writeln!(out, "всего действий: {}", actions.len());
    out
}

/// Итоговая сводка по применённым исправлениям (стиль отчёта `doctor`).
pub fn summary(results: &[FixResult]) -> String {
    use std::fmt::Write as _;
    if results.is_empty() {
        return "Итог --fix: действий не было — всё уже в порядке".to_string();
    }
    let fixed = results.iter().filter(|r| r.did_something).count();
    let mut out = String::new();
    let _ = writeln!(out, "Итог --fix: исправлено {fixed} из {}", results.len());
    for r in results {
        let icon = if r.did_something { "✅" } else { "⏭️" };
        let _ = writeln!(out, "  {icon} {}: {}", r.action.slug(), r.detail);
    }
    out
}

// --- Пути -------------------------------------------------------------------

/// Глобальный конфиг харнесса (слой `global` из `config_layers`).
fn global_config_path(home: &Path) -> PathBuf {
    home.join(".theseus").join("config.toml")
}

/// Каталог сессий внутри workspace (как в `doctor_ext::sessions_dir`).
fn sessions_dir(workspace: &Path) -> PathBuf {
    workspace.join(".theseus").join("sessions")
}

/// Каталог скиллов внутри workspace (как в `doctor::skill_dirs`).
fn skills_dir(workspace: &Path) -> PathBuf {
    workspace.join(".theseus").join("skills")
}

/// Корень транскриптов: агент пишет `events-<ts>.jsonl` прямо в `.theseus`,
/// `doctor_ext` квотит подкаталог `.theseus/transcripts` — сканируем рекурсивно
/// весь `.theseus`, чтобы покрыть оба расположения.
fn transcripts_root(workspace: &Path) -> PathBuf {
    workspace.join(".theseus")
}

/// Цель действия уже удовлетворена — планировать/чинить нечего.
fn already_satisfied(action: FixAction, home: &Path, workspace: &Path) -> bool {
    match action {
        FixAction::CreateConfig => global_config_path(home).exists(),
        // несуществующий workspace chmod'ом не починить — не планируем
        FixAction::FixWorkspacePerms => !workspace.is_dir() || workspace_has_755(workspace),
        FixAction::CreateSessionDir => sessions_dir(workspace).is_dir(),
        FixAction::CreateSkillsDir => skills_dir(workspace).is_dir(),
        FixAction::TrimTranscripts => {
            events_total_bytes(&transcripts_root(workspace)) <= TRANSCRIPT_QUOTA_BYTES
        }
    }
}

// --- CreateConfig -----------------------------------------------------------

/// Пример конфига из defaults `crate::config` (актуальные значения по умолчанию).
const EXAMPLE_CONFIG: &str = "\
# Конфигурация theseus — глобальный слой (~/.theseus/config.toml).
# Создано `theseus doctor --fix`. Приоритет слоёв:
# defaults < ~/.theseus/config.toml < ./.theseus/config.toml < CLI-оверрайды.

# Модель провайдера.
model = \"deepseek-v4-pro\"

# Базовый URL OpenAI-совместимого API; ключ лучше держать в env DEEPSEEK_API_KEY.
# base_url = \"https://api.deepseek.com/v1\"
# api_key = \"...\"

# Оценочный лимит контекста в токенах (chars/4-эвристика).
context_limit_tokens = 120000

# Потолок max_tokens на один ответ.
max_output_tokens = 8192

# Таймаут одного API-вызова, секунд.
api_timeout_secs = 600

# Ядерный sandbox (landlock) для bash-команд.
sandbox = true

# Пороги трёхуровневой компактификации, % окна (строго по возрастанию).
compact_mask_pct = 70
compact_prune_pct = 80
compact_summary_pct = 95
";

fn apply_create_config(home: &Path) -> FixResult {
    let path = global_config_path(home);
    let Some(parent) = path.parent() else {
        return FixResult::failed(FixAction::CreateConfig, format!("нет родителя у {}", path.display()));
    };
    if let Err(e) = std::fs::create_dir_all(parent) {
        return FixResult::failed(FixAction::CreateConfig, format!("не создать {}: {e}", parent.display()));
    }
    // create_new: существующий конфиг никогда не затираем (гонка безопасна).
    let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return FixResult::skipped(FixAction::CreateConfig, format!("конфиг уже существует: {}", path.display()));
        }
        Err(e) => return FixResult::failed(FixAction::CreateConfig, format!("не создать {}: {e}", path.display())),
    };
    use std::io::Write as _;
    match file.write_all(EXAMPLE_CONFIG.as_bytes()) {
        Ok(()) => FixResult::done(FixAction::CreateConfig, format!("создан пример конфига: {}", path.display())),
        Err(e) => FixResult::failed(FixAction::CreateConfig, format!("запись в {}: {e}", path.display())),
    }
}

// --- FixWorkspacePerms ------------------------------------------------------

/// Права на workspace уже ровно 0755?
#[cfg(unix)]
fn workspace_has_755(workspace: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(workspace)
        .map(|m| m.permissions().mode() & 0o777 == 0o755)
        .unwrap_or(false)
}

/// На не-unix chmod не поддерживается — считаем цель удовлетворённой.
#[cfg(not(unix))]
fn workspace_has_755(_workspace: &Path) -> bool {
    true
}

#[cfg(unix)]
fn apply_workspace_perms(workspace: &Path) -> FixResult {
    use std::os::unix::fs::PermissionsExt;
    let meta = match std::fs::metadata(workspace) {
        Ok(m) => m,
        Err(e) => {
            return FixResult::failed(FixAction::FixWorkspacePerms, format!("metadata {}: {e}", workspace.display()));
        }
    };
    if !meta.is_dir() {
        return FixResult::failed(FixAction::FixWorkspacePerms, format!("не каталог: {}", workspace.display()));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode == 0o755 {
        return FixResult::skipped(FixAction::FixWorkspacePerms, format!("права уже 0755: {}", workspace.display()));
    }
    match std::fs::set_permissions(workspace, std::fs::Permissions::from_mode(0o755)) {
        Ok(()) => FixResult::done(FixAction::FixWorkspacePerms, format!("права {}: {mode:04o} → 0755", workspace.display())),
        Err(e) => FixResult::failed(FixAction::FixWorkspacePerms, format!("chmod {}: {e}", workspace.display())),
    }
}

#[cfg(not(unix))]
fn apply_workspace_perms(workspace: &Path) -> FixResult {
    FixResult::skipped(
        FixAction::FixWorkspacePerms,
        format!("chmod поддерживается только на unix: {}", workspace.display()),
    )
}

// --- CreateSessionDir / CreateSkillsDir --------------------------------------

fn apply_create_dir(action: FixAction, dir: &Path, what: &str) -> FixResult {
    if dir.is_dir() {
        return FixResult::skipped(action, format!("{what} уже есть: {}", dir.display()));
    }
    match std::fs::create_dir_all(dir) {
        Ok(()) => FixResult::done(action, format!("создан {what}: {}", dir.display())),
        Err(e) => FixResult::failed(action, format!("не создать {}: {e}", dir.display())),
    }
}

// --- TrimTranscripts ---------------------------------------------------------

/// Найденный events-файл транскрипта.
struct EventsFile {
    path: PathBuf,
    size: u64,
    /// Метка «старости»: ts из имени `events-<ts>.jsonl`; имя без числового ts
    /// — это мусор/чужой файл в каталоге транскриптов, считаем его древнейшим
    /// (ts = 0) и чистим первым.
    ts: u64,
}

/// Разобрать ts из имени вида `events-<unix-секунды>.jsonl`.
fn parse_events_ts(name: &str) -> Option<u64> {
    name.strip_prefix("events-")?.strip_suffix(".jsonl")?.parse().ok()
}

/// Собрать все `events-*.jsonl` под каталогом (рекурсивно, без ухода по
/// симлинкам), отсортировав от старых к новым.
fn collect_events(dir: &Path) -> Vec<EventsFile> {
    let mut out: Vec<EventsFile> = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                Ok(t) if t.is_file() => {
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    if !(name.starts_with("events-") && name.ends_with(".jsonl")) {
                        continue;
                    }
                    let Ok(meta) = entry.metadata() else { continue };
                    let ts = parse_events_ts(name).unwrap_or(0);
                    out.push(EventsFile { path: entry.path(), size: meta.len(), ts });
                }
                _ => {}
            }
        }
    }
    out.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.path.cmp(&b.path)));
    out
}

/// Суммарный размер всех events-файлов под каталогом.
fn events_total_bytes(dir: &Path) -> u64 {
    collect_events(dir).iter().map(|f| f.size).sum()
}

/// Удалить старейшие events-файлы, пока сумма не уложится в квоту.
/// Возвращает (число удалённых, освобождено байт, осталось байт).
fn trim_events(dir: &Path, quota: u64) -> (usize, u64, u64) {
    let files = collect_events(dir);
    let mut total: u64 = files.iter().map(|f| f.size).sum();
    let mut deleted = 0usize;
    let mut freed = 0u64;
    for f in &files {
        if total <= quota {
            break;
        }
        // не удалившийся файл пропускаем, но из кандидатов он уже выбыл —
        // бесконечного цикла нет, честно движемся к следующему старейшему
        if std::fs::remove_file(&f.path).is_ok() {
            deleted += 1;
            freed += f.size;
            total = total.saturating_sub(f.size);
        }
    }
    (deleted, freed, total)
}

fn apply_trim_transcripts(workspace: &Path) -> FixResult {
    let dir = transcripts_root(workspace);
    let (deleted, freed, left) = trim_events(&dir, TRANSCRIPT_QUOTA_BYTES);
    if deleted == 0 {
        FixResult::skipped(
            FixAction::TrimTranscripts,
            format!("квота не превышена: {} из {}", fmt_bytes(left), fmt_bytes(TRANSCRIPT_QUOTA_BYTES)),
        )
    } else {
        FixResult::done(
            FixAction::TrimTranscripts,
            format!(
                "удалено {deleted} events-файлов, освобождено {}, осталось {} из {}",
                fmt_bytes(freed),
                fmt_bytes(left),
                fmt_bytes(TRANSCRIPT_QUOTA_BYTES)
            ),
        )
    }
}

// --- Утилиты -----------------------------------------------------------------

/// Человекочитаемый размер: Б / КиБ / МиБ / ГиБ (целочисленно).
/// Дублирует приватный хелпер `doctor_ext` осознанно — модули независимы.
fn fmt_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB {
        format!("{} ГиБ", n / GIB)
    } else if n >= MIB {
        format!("{} МиБ", n / MIB)
    } else if n >= KIB {
        format!("{} КиБ", n / KIB)
    } else {
        format!("{n} Б")
    }
}

// --- Тесты -------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Временный каталог с автоудалением.
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("theseus-doc-fix-{}-{tag}", std::process::id()));
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

    /// Записать файл заданного размера (создавая родителей).
    fn write_sized(path: &Path, bytes: usize) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, vec![7u8; bytes]).unwrap();
    }

    /// Выставить unix-права на каталог.
    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Текущие unix-права (младшие 9 бит) на путь.
    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Пять характерных строк проблем (RU/EN, с дублем, не в каноническом порядке).
    fn all_issues() -> Vec<String> {
        vec![
            "транскрипты занимают 150 МиБ при квоте 100 МиБ".to_string(),
            "нет config.toml — конфиг не найден".to_string(),
            "каталог сессий недоступен".to_string(),
            "workspace: Permission denied (os error 13)".to_string(),
            "скиллы не найдены: нет каталога skills".to_string(),
            "каталог сессий недоступен".to_string(), // дубль — не должен плодить действия
        ]
    }

    #[test]
    fn plan_maps_all_issue_kinds_in_canonical_order() {
        let tmp = TmpDir::new("plan-all");
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        set_mode(&ws, 0o700); // чтобы FixWorkspacePerms был нужен
        // превышение квоты, чтобы планировался TrimTranscripts
        write_sized(&ws.join(".theseus/events-1.jsonl"), (TRANSCRIPT_QUOTA_BYTES + 1) as usize);

        let plan = plan_fixes(&home, &ws, &all_issues());
        assert_eq!(plan, FixAction::ALL.to_vec(), "все 5 действий в каноническом порядке, без дублей");
    }

    #[test]
    fn plan_ignores_unknown_and_empty_issues() {
        let tmp = TmpDir::new("plan-unknown");
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        assert!(plan_fixes(&home, &ws, &[]).is_empty(), "пустой вход — пустой план");
        let unknown = vec![
            "landlock недоступен — bash без ядерной изоляции".to_string(),
            "mtime свежего файла в будущем на 300 с".to_string(),
            "локаль без UTF-8".to_string(),
        ];
        assert!(plan_fixes(&home, &ws, &unknown).is_empty(), "неизвестные проблемы не мапятся");
    }

    #[test]
    fn plan_skips_already_satisfied_targets() {
        let tmp = TmpDir::new("plan-satisfied");
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        set_mode(&ws, 0o755);
        write_sized(&global_config_path(&home), 10);
        fs::create_dir_all(sessions_dir(&ws)).unwrap();
        fs::create_dir_all(skills_dir(&ws)).unwrap();
        write_sized(&ws.join(".theseus/events-1.jsonl"), 100); // под квотой

        assert!(plan_fixes(&home, &ws, &all_issues()).is_empty(), "всё удовлетворено — план пуст");

        // уберём каталог скиллов — в плане должно остаться ровно одно действие
        fs::remove_dir_all(skills_dir(&ws)).unwrap();
        assert_eq!(plan_fixes(&home, &ws, &all_issues()), vec![FixAction::CreateSkillsDir]);
    }

    #[test]
    fn keyword_matching_is_case_insensitive_and_multilingual() {
        let tmp = TmpDir::new("plan-case");
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        set_mode(&ws, 0o700);
        let issues = vec![
            "CONFIG.TOML MISSING".to_string(),
            "Проверьте ПРАВА на каталог".to_string(),
            "Session Directory Broken".to_string(),
            "SKILLS dir not found".to_string(),
            "ПРЕВЫШЕНА КВОТА транскриптов".to_string(),
        ];
        // квота не превышена на диске → TrimTranscripts отфильтруется ФС-проверкой,
        // остальные четыре распознаются в любом регистре и языке
        let plan = plan_fixes(&home, &ws, &issues);
        assert_eq!(
            plan,
            vec![
                FixAction::CreateConfig,
                FixAction::FixWorkspacePerms,
                FixAction::CreateSessionDir,
                FixAction::CreateSkillsDir,
            ]
        );
    }

    #[test]
    fn apply_create_config_writes_parseable_example_with_defaults() {
        let tmp = TmpDir::new("cfg-create");
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");

        let res = apply_fix(FixAction::CreateConfig, &home, &ws);
        assert!(res.did_something, "detail: {}", res.detail);
        assert_eq!(res.action, FixAction::CreateConfig);

        let path = global_config_path(&home);
        let text = fs::read_to_string(&path).unwrap();
        assert!(res.detail.contains(&path.display().to_string()));
        // пример обязан быть валидным TOML и нести defaults из crate::config
        let doc: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(doc["model"].as_str().unwrap(), "deepseek-v4-pro");
        assert_eq!(doc["context_limit_tokens"].as_integer().unwrap(), 120_000);
        assert_eq!(doc["max_output_tokens"].as_integer().unwrap(), 8_192);
        assert_eq!(doc["api_timeout_secs"].as_integer().unwrap(), 600);
        assert!(doc["sandbox"].as_bool().unwrap());
        // пороги компактификации — строго по возрастанию (проверка doctor #11)
        let mask = doc["compact_mask_pct"].as_integer().unwrap();
        let prune = doc["compact_prune_pct"].as_integer().unwrap();
        let summ = doc["compact_summary_pct"].as_integer().unwrap();
        assert!(mask < prune && prune < summ, "{mask} < {prune} < {summ}");
    }

    #[test]
    fn apply_create_config_idempotent_and_never_overwrites() {
        let tmp = TmpDir::new("cfg-idem");
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");

        let first = apply_fix(FixAction::CreateConfig, &home, &ws);
        assert!(first.did_something);
        let original = fs::read_to_string(global_config_path(&home)).unwrap();

        let second = apply_fix(FixAction::CreateConfig, &home, &ws);
        assert!(!second.did_something, "повтор — без изменений");
        assert!(second.detail.contains("уже существует"));
        assert_eq!(fs::read_to_string(global_config_path(&home)).unwrap(), original);

        // чужой конфиг не затирается ни байтом
        let custom = "model = \"custom-model\"\n";
        fs::write(global_config_path(&home), custom).unwrap();
        let third = apply_fix(FixAction::CreateConfig, &home, &ws);
        assert!(!third.did_something);
        assert_eq!(fs::read_to_string(global_config_path(&home)).unwrap(), custom);
    }

    #[cfg(unix)]
    #[test]
    fn apply_workspace_perms_chmods_to_755_and_is_idempotent() {
        let tmp = TmpDir::new("perms");
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        set_mode(&ws, 0o700);

        let res = apply_fix(FixAction::FixWorkspacePerms, &home, &ws);
        assert!(res.did_something, "detail: {}", res.detail);
        assert_eq!(mode_of(&ws), 0o755);
        assert!(res.detail.contains("0700 → 0755"), "detail: {}", res.detail);

        let second = apply_fix(FixAction::FixWorkspacePerms, &home, &ws);
        assert!(!second.did_something);
        assert!(second.detail.contains("уже 0755"));

        // после исправления действие выпадает и из плана
        let issues = vec!["проверьте права на workspace".to_string()];
        assert!(!plan_fixes(&home, &ws, &issues).contains(&FixAction::FixWorkspacePerms));
    }

    #[cfg(unix)]
    #[test]
    fn apply_workspace_perms_reports_missing_and_non_dir() {
        let tmp = TmpDir::new("perms-err");
        let home = tmp.path().join("home");
        let missing = tmp.path().join("no-such-dir");
        let res = apply_fix(FixAction::FixWorkspacePerms, &home, &missing);
        assert!(!res.did_something);
        assert!(res.detail.contains("ошибка"), "detail: {}", res.detail);

        let file = tmp.path().join("a-file");
        fs::write(&file, "x").unwrap();
        let res = apply_fix(FixAction::FixWorkspacePerms, &home, &file);
        assert!(!res.did_something);
        assert!(res.detail.contains("не каталог"), "detail: {}", res.detail);
    }

    #[test]
    fn apply_session_and_skills_dirs_create_then_idempotent() {
        let tmp = TmpDir::new("dirs");
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();

        for action in [FixAction::CreateSessionDir, FixAction::CreateSkillsDir] {
            let res = apply_fix(action, &home, &ws);
            assert!(res.did_something, "{action:?}: {}", res.detail);
            let again = apply_fix(action, &home, &ws);
            assert!(!again.did_something, "{action:?} повторно: {}", again.detail);
            assert!(again.detail.contains("уже есть"));
        }
        assert!(sessions_dir(&ws).is_dir());
        assert!(skills_dir(&ws).is_dir());
        // и план больше их не предлагает
        let issues = vec!["нет каталога сессий и скиллов".to_string()];
        assert!(plan_fixes(&home, &ws, &issues).is_empty());
    }

    #[test]
    fn apply_create_dir_reports_error_when_blocked_by_file() {
        let tmp = TmpDir::new("dirs-err");
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        // путь каталога сессий занят регулярным файлом — create_dir_all упадёт
        write_sized(&sessions_dir(&ws), 1);
        let res = apply_fix(FixAction::CreateSessionDir, &home, &ws);
        assert!(!res.did_something);
        assert!(res.detail.contains("ошибка"), "detail: {}", res.detail);
    }

    #[test]
    fn trim_events_deletes_oldest_first_until_quota() {
        let tmp = TmpDir::new("trim-order");
        let dir = tmp.path().join("transcripts");
        for ts in 1..=5 {
            write_sized(&dir.join(format!("events-{ts}.jsonl")), 100);
        }
        // 500 Б при квоте 250 Б → удалятся ровно 3 старейших (ts 1,2,3)
        let (deleted, freed, left) = trim_events(&dir, 250);
        assert_eq!(deleted, 3);
        assert_eq!(freed, 300);
        assert_eq!(left, 200);
        assert!(!dir.join("events-1.jsonl").exists());
        assert!(!dir.join("events-3.jsonl").exists());
        assert!(dir.join("events-4.jsonl").exists(), "свежие остаются");
        assert!(dir.join("events-5.jsonl").exists());
    }

    #[test]
    fn trim_events_exact_quota_is_not_exceeded() {
        let tmp = TmpDir::new("trim-boundary");
        let dir = tmp.path().join("t");
        write_sized(&dir.join("events-1.jsonl"), 150);
        write_sized(&dir.join("events-2.jsonl"), 100);
        // ровно квота — строгое превышение не наступило, ничего не удаляем
        let (deleted, freed, left) = trim_events(&dir, 250);
        assert_eq!((deleted, freed, left), (0, 0, 250));
        assert!(dir.join("events-1.jsonl").exists());
    }

    #[test]
    fn trim_ignores_foreign_files_and_unparsable_names_go_first() {
        let tmp = TmpDir::new("trim-foreign");
        let dir = tmp.path().join("t");
        // чужие файлы: сессии, трейсы, events без .jsonl — не трогаем
        write_sized(&dir.join("session-1.json"), 10_000);
        write_sized(&dir.join("trace-1.jsonl"), 10_000);
        write_sized(&dir.join("events-99.txt"), 10_000);
        // events-файлы: имя без ts считается древнейшим (ts=0)
        write_sized(&dir.join("events-broken.jsonl"), 100);
        write_sized(&dir.join("events-10.jsonl"), 100);
        write_sized(&dir.join("events-20.jsonl"), 100);

        let (deleted, _freed, left) = trim_events(&dir, 200);
        assert_eq!(deleted, 1, "только events-broken.jsonl — он «старейший»");
        assert!(!dir.join("events-broken.jsonl").exists());
        assert_eq!(left, 200);
        for kept in ["session-1.json", "trace-1.jsonl", "events-99.txt", "events-10.jsonl", "events-20.jsonl"] {
            assert!(dir.join(kept).exists(), "{kept} должен остаться");
        }
    }

    #[test]
    fn apply_trim_transcripts_real_quota_end_to_end() {
        let tmp = TmpDir::new("trim-real");
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        let mib = 1024 * 1024usize;
        // 60 МиБ (старый) + 50 МиБ (новый) = 110 МиБ > квоты 100 МиБ
        write_sized(&ws.join(".theseus/events-1000.jsonl"), 60 * mib);
        write_sized(&ws.join(".theseus/events-2000.jsonl"), 50 * mib);

        let res = apply_fix(FixAction::TrimTranscripts, &home, &ws);
        assert!(res.did_something, "detail: {}", res.detail);
        assert!(res.detail.contains("удалено 1 events-файлов"), "detail: {}", res.detail);
        assert!(!ws.join(".theseus/events-1000.jsonl").exists(), "старый удалён");
        assert!(ws.join(".theseus/events-2000.jsonl").exists(), "новый остался");

        // повтор — квота уже не превышена
        let second = apply_fix(FixAction::TrimTranscripts, &home, &ws);
        assert!(!second.did_something);
        assert!(second.detail.contains("квота не превышена"), "detail: {}", second.detail);
    }

    #[test]
    fn dry_run_renders_plan_and_empty_case() {
        let tmp = TmpDir::new("dry");
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        let text = dry_run(&[FixAction::CreateConfig, FixAction::CreateSkillsDir], &home, &ws);
        assert!(text.contains("dry-run"));
        assert!(text.contains("1. создание конфига"));
        assert!(text.contains("2. каталог скиллов"));
        assert!(text.contains(&global_config_path(&home).display().to_string()));
        assert!(text.contains(&skills_dir(&ws).display().to_string()));
        assert!(text.contains("всего действий: 2"));

        let empty = dry_run(&[], &home, &ws);
        assert!(empty.contains("действий не требуется"));
    }

    #[test]
    fn summary_renders_exact_format() {
        let results = vec![
            FixResult::done(FixAction::CreateSessionDir, "создан каталог сессий: /tmp/x".to_string()),
            FixResult::skipped(FixAction::TrimTranscripts, "квота не превышена: 1 КиБ из 100 МиБ".to_string()),
        ];
        let text = summary(&results);
        assert!(text.contains("Итог --fix: исправлено 1 из 2"), "text: {text}");
        assert!(text.contains("✅ create_session_dir: создан каталог сессий: /tmp/x"));
        assert!(text.contains("⏭️ trim_transcripts: квота не превышена"));
        assert!(text.lines().all(|l| !l.contains("⏭️") || l.contains("trim_transcripts")));

        assert_eq!(summary(&[]), "Итог --fix: действий не было — всё уже в порядке");
    }

    #[test]
    fn fix_all_end_to_end_and_second_run_is_empty() {
        let tmp = TmpDir::new("fix-all");
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        set_mode(&ws, 0o700);
        let issues = vec![
            "конфиг не найден".to_string(),
            "permission denied на workspace".to_string(),
            "нет каталога сессий".to_string(),
            "skills missing".to_string(),
        ];

        let results = fix_all(&home, &ws, &issues);
        assert_eq!(results.len(), 4, "TrimTranscripts не нужен — квота не превышена");
        assert!(results.iter().all(|r| r.did_something), "первый прогон всё чинит");
        let text = summary(&results);
        assert!(text.contains("Итог --fix: исправлено 4 из 4"));

        // идемпотентность end-to-end: повторный fix_all ничего не планирует
        let again = fix_all(&home, &ws, &issues);
        assert!(again.is_empty(), "повторный прогон — действий нет");
        assert_eq!(summary(&again), "Итог --fix: действий не было — всё уже в порядке");
        // состояние на месте
        assert!(global_config_path(&home).is_file());
        assert_eq!(mode_of(&ws), 0o755);
        assert!(sessions_dir(&ws).is_dir());
        assert!(skills_dir(&ws).is_dir());
    }
}
