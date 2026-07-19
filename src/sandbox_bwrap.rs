//! Интеграция bubblewrap (bwrap) — второй уровень sandbox поверх Landlock.
//!
//! Образец — `codex-rs/linux-sandbox` (`bwrap.rs`) и `codex-rs/bwrap`:
//! - файловая система по умолчанию read-only (`--ro-bind / /`), поверх неё —
//!   минимальные `/dev`, `/proc` и свежий tmpfs на `/tmp`;
//! - каталоги на запись монтируются явно (`--bind`), а read-only вырезы внутри
//!   них переприменяются ПОСЛЕ: у bwrap порядок аргументов = порядок
//!   монтирования, и последний бинд побеждает (так у Codex защищён `.git`
//!   внутри записываемого workspace);
//! - свежий user namespace запрашивается всегда (`--unshare-user`) — урок
//!   Codex: без него root внутри контейнера не сможет unshare остальные ns;
//! - `--unshare-all` намеренно НЕ используется: он безусловно отрезает сеть,
//!   а харнессу нужен доступ к API. Сеть отключается только по явному запросу
//!   (`--unshare-net`, поле [`BwrapSpec::unshare_net`]).
//!
//! Fallback-матрица с Landlock (модуль самодостаточен и не зависит от
//! `crate::sandbox`): bwrap отсутствует или непригоден → только Landlock, см.
//! [`fallback_plan`] и [`SandboxPlan`].
//!
//! Кэширование: [`probe`] и [`landlock_available`] считаются один раз за
//! процесс (`OnceLock`, как `status()` в `crate::sandbox`), а [`detect`] и
//! [`probe_detail`] каждый раз идут в систему — используйте их для диагностики.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::OnceLock;

/// Статус доступности bubblewrap на этой машине.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BwrapStatus {
    /// bwrap найден в PATH и пробный запуск изолированной команды успешен.
    Available,
    /// Бинарник bwrap не найден в PATH (или не запускается).
    Missing,
    /// Бинарник есть, но создать sandbox не удалось — почти всегда это
    /// запрещённые user namespaces (`kernel.unprivileged_userns_clone=0`,
    /// `user.max_user_namespaces=0`, секкомп контейнера). Детали — в stderr
    /// [`BwrapProbe`].
    NoUserNs,
}

impl std::fmt::Display for BwrapStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Available => write!(f, "bwrap доступен"),
            Self::Missing => write!(f, "bwrap не найден в PATH"),
            Self::NoUserNs => write!(f, "bwrap есть, но user namespaces недоступны"),
        }
    }
}

/// Подробности пробного запуска bwrap (см. [`probe_detail`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BwrapProbe {
    /// Итоговый статус.
    pub status: BwrapStatus,
    /// Распарсенная версия bwrap, если бинарник найден и ответил на `--version`.
    pub version: Option<(u64, u64, u64)>,
    /// Код выхода пробного запуска (`0` при успехе).
    pub exit_code: Option<i32>,
    /// stderr пробного запуска (обрезан) — первичный диагностический материал
    /// при [`BwrapStatus::NoUserNs`].
    pub stderr: String,
}

/// Ошибка построения или валидации [`BwrapSpec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BwrapError {
    /// Путь бинда не абсолютный — bwrap требует абсолютные пути.
    NotAbsolute(PathBuf),
    /// rw-бинд совпадает с ro-биндом или лежит под ним: ro применяется позже
    /// и скроет запись, такой бинд бессмыслен (а скорее всего — ошибка конфига).
    RoRwOverlap {
        /// Read-only бинд, который перекрывает.
        ro: PathBuf,
        /// Перекрытый rw-бинд.
        rw: PathBuf,
    },
    /// Пустая команда: bwrap требует хотя бы один элемент argv.
    EmptyCommand,
}

impl std::fmt::Display for BwrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAbsolute(path) => {
                write!(f, "путь бинда не абсолютный: {}", path.display())
            }
            Self::RoRwOverlap { ro, rw } => write!(
                f,
                "rw-бинд {} перекрыт ro-биндом {} (ro применяется позже и скроет запись)",
                rw.display(),
                ro.display()
            ),
            Self::EmptyCommand => write!(f, "пустая команда: bwrap требует argv"),
        }
    }
}

impl std::error::Error for BwrapError {}

/// Спецификация изоляции bubblewrap для одного запуска команды.
///
/// Базовое дерево (`--ro-bind / /` + `/dev`, `/proc`, `/tmp`) добавляется
/// всегда; поля описывают только отличия от умолчаний Codex-подобного
/// запуска: `--new-session --die-with-parent --unshare-user --unshare-pid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BwrapSpec {
    /// Read-only бинды поверх rw (`--ro-bind`), применяются ПОСЛЕ [`Self::rw_binds`]
    /// — так делаются ro-вырезы внутри записываемых каталогов (например `.git`).
    pub ro_binds: Vec<PathBuf>,
    /// Каталоги, доступные на запись (`--bind`), — обычно workspace.
    pub rw_binds: Vec<PathBuf>,
    /// Отрезать сеть (`--unshare-net`). По умолчанию `false`: агенту нужен API.
    pub unshare_net: bool,
    /// Свежий PID namespace (`--unshare-pid`).
    pub unshare_pid: bool,
    /// Завершить sandbox при смерти родителя (`--die-with-parent`).
    pub die_with_parent: bool,
    /// Новая сессия без управляющего терминала (`--new-session`).
    pub new_session: bool,
}

impl Default for BwrapSpec {
    /// Умолчания Codex-подобного запуска: сеть сохранена, остальная изоляция
    /// включена, биндов нет.
    fn default() -> Self {
        Self {
            ro_binds: Vec::new(),
            rw_binds: Vec::new(),
            unshare_net: false,
            unshare_pid: true,
            die_with_parent: true,
            new_session: true,
        }
    }
}

impl BwrapSpec {
    /// Начать построение спецификации через билдер (с валидацией в `build()`).
    pub fn builder() -> BwrapSpecBuilder {
        BwrapSpecBuilder::default()
    }

    /// Типовая спецификация «запись только в workspace»: умолчания + rw-бинд.
    ///
    /// # Ошибки
    /// [`BwrapError::NotAbsolute`], если `workspace` не абсолютный.
    pub fn for_workspace(workspace: &Path) -> Result<Self, BwrapError> {
        Self::builder().rw_bind(workspace).build()
    }

    /// Проверить спецификацию без построения argv.
    ///
    /// # Ошибки
    /// - [`BwrapError::NotAbsolute`] — любой неабсолютный путь;
    /// - [`BwrapError::RoRwOverlap`] — rw-бинд, совпадающий с ro-биндом или
    ///   лежащий под ним (ro применяется позже и скроет запись). Обратное
    ///   вложение (ro-подпуть внутри rw-корня) — легальный вырез и разрешён.
    pub fn validate(&self) -> Result<(), BwrapError> {
        for path in self.ro_binds.iter().chain(&self.rw_binds) {
            if !path.is_absolute() {
                return Err(BwrapError::NotAbsolute(path.clone()));
            }
        }
        for rw in &self.rw_binds {
            if let Some(ro) = self.ro_binds.iter().find(|ro| rw.starts_with(ro)) {
                return Err(BwrapError::RoRwOverlap {
                    ro: ro.clone(),
                    rw: rw.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Билдер [`BwrapSpec`] с дедупликацией биндов и валидацией в [`Self::build`].
#[derive(Debug, Clone, Default)]
pub struct BwrapSpecBuilder {
    spec: BwrapSpec,
}

impl BwrapSpecBuilder {
    /// Пустой билдер с умолчаниями [`BwrapSpec::default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Добавить read-only бинд (применится ПОСЛЕ rw-биндов). Дубликаты молча
    /// игнорируются; абсолютность и пересечения проверяются в [`Self::build`].
    pub fn ro_bind(mut self, path: impl Into<PathBuf>) -> Self {
        push_unique(&mut self.spec.ro_binds, path.into());
        self
    }

    /// Добавить бинд на запись. Дубликаты молча игнорируются.
    pub fn rw_bind(mut self, path: impl Into<PathBuf>) -> Self {
        push_unique(&mut self.spec.rw_binds, path.into());
        self
    }

    /// Отрезать ли сеть (`--unshare-net`). Умолчание — `false` (нужен API).
    pub fn unshare_net(mut self, yes: bool) -> Self {
        self.spec.unshare_net = yes;
        self
    }

    /// Свежий ли PID namespace (`--unshare-pid`). Умолчание — `true`.
    pub fn unshare_pid(mut self, yes: bool) -> Self {
        self.spec.unshare_pid = yes;
        self
    }

    /// Умирать ли вместе с родителем (`--die-with-parent`). Умолчание — `true`.
    pub fn die_with_parent(mut self, yes: bool) -> Self {
        self.spec.die_with_parent = yes;
        self
    }

    /// Новая ли сессия (`--new-session`). Умолчание — `true`.
    pub fn new_session(mut self, yes: bool) -> Self {
        self.spec.new_session = yes;
        self
    }

    /// Завершить построение с валидацией (см. [`BwrapSpec::validate`]).
    ///
    /// # Ошибки
    /// [`BwrapError::NotAbsolute`] и [`BwrapError::RoRwOverlap`].
    pub fn build(self) -> Result<BwrapSpec, BwrapError> {
        self.spec.validate()?;
        Ok(self.spec)
    }
}

/// Добавить путь в список, если его там ещё нет (сохраняя порядок добавления).
fn push_unique(list: &mut Vec<PathBuf>, path: PathBuf) {
    if !list.contains(&path) {
        list.push(path);
    }
}

/// Найти работающий bwrap в PATH.
///
/// Возвращает путь к бинарнику, только если он исполняем И отвечает на
/// `--version` кодом 0 (отсекает битые обёртки). Не кэшируется.
#[must_use]
pub fn detect() -> Option<PathBuf> {
    let candidate = find_in_path("bwrap")?;
    let runs = Command::new(&candidate)
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    runs.then_some(candidate)
}

/// Найти исполняемый файл `name` в каталогах PATH.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// Файл существует, является обычным файлом и имеет хотя бы один бит exec.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
}

/// Распарсить вывод `bwrap --version` вида `bubblewrap 0.9.0`.
///
/// Берётся первый токен, начинающийся с цифры; недостающие компоненты
/// (`bubblewrap 1.2`) считаются нулями, суффиксы (`0.10.1-rc1`) отбрасываются.
/// Возвращает `None`, если цифрового токена нет вовсе.
#[must_use]
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let token = text
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let mut parts = token.split('.');
    let major = parse_leading_digits(parts.next()?)?;
    let minor = parts.next().and_then(parse_leading_digits).unwrap_or(0);
    let patch = parts.next().and_then(parse_leading_digits).unwrap_or(0);
    Some((major, minor, patch))
}

/// Ведущие ASCII-цифры токена: «10-rc1» → `Some(10)`, «rc1»/«» → `None`.
fn parse_leading_digits(s: &str) -> Option<u64> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Версия конкретного бинарника bwrap (`--version` + [`parse_version`]).
#[must_use]
pub fn bwrap_version(bwrap: &Path) -> Option<(u64, u64, u64)> {
    let out = Command::new(bwrap).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

/// Однократная проверка пригодности bwrap (кэшируется на весь процесс).
///
/// Для свежей диагностики (stderr, код выхода, версия) зовите [`probe_detail`].
#[must_use]
pub fn probe() -> BwrapStatus {
    static STATUS: OnceLock<BwrapStatus> = OnceLock::new();
    *STATUS.get_or_init(|| probe_detail().status)
}

/// Пробный запуск bwrap с полной диагностикой, без кэша.
///
/// Запускается `bwrap --ro-bind / / --unshare-user -- true` — с тем же ключом
/// user namespace, который всегда ставит [`wrap_command`], иначе probe мог бы
/// быть «зелёным» там, где реальный запуск упадёт. Коды выхода и stderr
/// классифицируются так: успех → [`BwrapStatus::Available`]; бинарник пропал
/// между [`detect`] и запуском → [`BwrapStatus::Missing`]; любой другой сбой
/// (в подавляющем большинстве случаев — запрет user namespaces, bwrap пишет
/// тогда `No permissions to create new namespace` в stderr) →
/// [`BwrapStatus::NoUserNs`].
#[must_use]
pub fn probe_detail() -> BwrapProbe {
    let Some(path) = detect() else {
        return BwrapProbe {
            status: BwrapStatus::Missing,
            version: None,
            exit_code: None,
            stderr: String::new(),
        };
    };
    let version = bwrap_version(&path);
    let probe_args = ["--ro-bind", "/", "/", "--unshare-user", "--", "true"];
    match Command::new(&path).args(probe_args).output() {
        Ok(out) => {
            let status = if out.status.success() {
                BwrapStatus::Available
            } else {
                BwrapStatus::NoUserNs
            };
            BwrapProbe {
                status,
                version,
                exit_code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BwrapProbe {
            status: BwrapStatus::Missing,
            version,
            exit_code: None,
            stderr: e.to_string(),
        },
        Err(e) => BwrapProbe {
            status: BwrapStatus::NoUserNs,
            version,
            exit_code: None,
            stderr: e.to_string(),
        },
    }
}

/// Построить полный argv запуска через bwrap (элемент 0 — `bwrap`).
///
/// Порядок аргументов (он же — порядок монтирования у bwrap):
/// 1. флаги сессии (`--new-session`, `--die-with-parent`) — по полям spec;
/// 2. базовое дерево: `--ro-bind / /`, минимальный `--dev /dev`, свежие
///    `--proc /proc` и `--tmpfs /tmp`;
/// 3. rw-бинды (`--bind путь путь`) в порядке добавления;
/// 4. ro-бинды (`--ro-bind путь путь`) — ПОСЛЕ rw, чтобы ro-вырезы побеждали;
/// 5. `--unshare-user` всегда; `--unshare-pid`/`--unshare-net` — по полям;
/// 6. `--` и сама команда.
///
/// Чистая функция: валидация — на совести [`BwrapSpec::validate`] (или
/// билдера), пустой `argv` здесь не проверяется (проверяется в [`run_wrapped`]).
#[must_use]
pub fn wrap_command(spec: &BwrapSpec, argv: &[String]) -> Vec<String> {
    let mut args = vec!["bwrap".to_string()];
    if spec.new_session {
        args.push("--new-session".to_string());
    }
    if spec.die_with_parent {
        args.push("--die-with-parent".to_string());
    }
    args.extend(
        [
            "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp",
        ]
        .map(str::to_string),
    );
    for path in &spec.rw_binds {
        let rendered = path.to_string_lossy();
        args.extend(["--bind".to_string(), rendered.to_string(), rendered.to_string()]);
    }
    for path in &spec.ro_binds {
        let rendered = path.to_string_lossy();
        args.extend(["--ro-bind".to_string(), rendered.to_string(), rendered.to_string()]);
    }
    // Урок Codex: свежий user namespace всегда, иначе root внутри контейнера
    // не сможет unshare остальные namespace без ambient CAP_SYS_ADMIN.
    args.push("--unshare-user".to_string());
    if spec.unshare_pid {
        args.push("--unshare-pid".to_string());
    }
    // `--unshare-all` не используем: он отрезал бы сеть безусловно.
    if spec.unshare_net {
        args.push("--unshare-net".to_string());
    }
    args.push("--".to_string());
    args.extend_from_slice(argv);
    args
}

/// Реально выполнить команду под bwrap (наследуя stdio вызывающего).
///
/// Бинарник берётся из [`detect`]; если тот `None` (гонка с удалением),
/// пробуется `bwrap` через PATH — ошибка запуска всплывёт как `Err`.
///
/// # Ошибки
/// - [`std::io::ErrorKind::InvalidInput`] — невалидная spec или пустой argv
///   (внутри — [`BwrapError`]);
/// - прочие `io::Error` — сбои запуска процесса (нет bwrap и т.п.).
pub fn run_wrapped(spec: &BwrapSpec, argv: &[String]) -> std::io::Result<ExitStatus> {
    spec.validate()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    if argv.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            BwrapError::EmptyCommand,
        ));
    }
    let full = wrap_command(spec, argv);
    let program = detect().unwrap_or_else(|| PathBuf::from("bwrap"));
    Command::new(program).args(&full[1..]).status()
}

/// Итоговый план изоляции: fallback-матрица bwrap × Landlock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxPlan {
    /// Двойной уровень: bwrap (вид ФС + namespaces) поверх Landlock
    /// (kernel-enforced ФС) — целевая конфигурация.
    BwrapPlusLandlock,
    /// Только bwrap: Landlock недоступен (ядро < 5.13).
    BwrapOnly,
    /// Только Landlock: bwrap не установлен или не смог создать namespace
    /// — ровно сегодняшний уровень `crate::sandbox`.
    LandlockOnly,
    /// Ни один механизм недоступен: команда идёт без kernel-enforced sandbox,
    /// пользователя нужно предупредить.
    Unprotected,
}

impl std::fmt::Display for SandboxPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BwrapPlusLandlock => write!(f, "bwrap + Landlock"),
            Self::BwrapOnly => write!(f, "только bwrap"),
            Self::LandlockOnly => write!(f, "только Landlock"),
            Self::Unprotected => write!(f, "без sandbox"),
        }
    }
}

/// Чистая fallback-матрица: bwrap отсутствует/непригоден → только Landlock.
#[must_use]
pub fn fallback_plan(bwrap: BwrapStatus, landlock_ok: bool) -> SandboxPlan {
    match (bwrap, landlock_ok) {
        (BwrapStatus::Available, true) => SandboxPlan::BwrapPlusLandlock,
        (BwrapStatus::Available, false) => SandboxPlan::BwrapOnly,
        (_, true) => SandboxPlan::LandlockOnly,
        (_, false) => SandboxPlan::Unprotected,
    }
}

/// Доступен ли Landlock на этом ядре (кэшируется на весь процесс).
///
/// Осознанная самодостаточная копия логики `crate::sandbox::status()` (модуль
/// не импортирует `crate::sandbox`): `create()` крейта landlock возвращает
/// «пустышку» даже на ядрах без поддержки, поэтому честный ответ даёт только
/// `restrict_self()` + проверка статуса в дочернем процессе.
#[must_use]
pub fn landlock_available() -> bool {
    static STATUS: OnceLock<bool> = OnceLock::new();
    *STATUS.get_or_init(landlock_probe_child)
}

/// Probe в ребёнке: `/bin/true` с pre_exec, применяющим минимальный ruleset.
fn landlock_probe_child() -> bool {
    use std::os::unix::process::CommandExt;
    // pre_exec unsafe по контракту std: между fork и exec разрешены только
    // async-signal-safe операции; landlock-сисвызовы им удовлетворяют.
    let result = unsafe {
        Command::new("/bin/true")
            .pre_exec(|| {
                enforce_landlock_minimal().map_err(std::io::Error::other)?;
                Ok(())
            })
            .status()
    };
    matches!(result, Ok(status) if status.success())
}

/// Минимальный ruleset + restrict_self; `Err`, если Landlock не применился
/// (`NotEnforced`) — это и есть «недоступен».
///
/// Правило — read-only на `/` (как у `crate::sandbox`), а не на `/tmp`:
/// после `restrict_self()` ребёнку ещё предстоит `execve("/bin/true")`, и без
/// права чтения/исполнения корня тот вернёт `EACCES` — probe ложно «упал» бы
/// на любой машине (проверено: `AccessFs::from_read` включает `Execute`).
fn enforce_landlock_minimal() -> Result<(), String> {
    use landlock::{
        ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus,
    };
    let abi = ABI::V1;
    let rw = AccessFs::from_all(abi);
    let ro = AccessFs::from_read(abi);
    let root_fd = PathFd::new("/").map_err(|e| format!("PathFd /: {e}"))?;
    let status = Ruleset::default()
        .handle_access(rw)
        .map_err(|e| format!("handle_access: {e}"))?
        .create()
        .map_err(|e| format!("create ruleset: {e}"))?
        .add_rule(PathBeneath::new(root_fd, ro))
        .map_err(|e| format!("rule /: {e}"))?
        .restrict_self()
        .map_err(|e| format!("restrict_self: {e}"))?;
    if matches!(status.ruleset, RulesetStatus::NotEnforced) {
        return Err("landlock не применён (NotEnforced)".to_string());
    }
    Ok(())
}

/// Текущий план изоляции этой машины: [`probe`] × [`landlock_available`].
#[must_use]
pub fn current_plan() -> SandboxPlan {
    fallback_plan(probe(), landlock_available())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// spec «workspace + ro-вырез .git» — типовая форма для тестов argv.
    fn ws_spec() -> BwrapSpec {
        BwrapSpec::builder()
            .rw_bind("/ws")
            .ro_bind("/ws/.git")
            .build()
            .unwrap()
    }

    /// bwrap есть и реально работает — врата для «живых» тестов.
    fn bwrap_usable() -> bool {
        detect().is_some() && probe() == BwrapStatus::Available
    }

    #[test]
    fn default_spec_values() {
        let spec = BwrapSpec::default();
        assert!(spec.ro_binds.is_empty());
        assert!(spec.rw_binds.is_empty());
        assert!(!spec.unshare_net, "сеть по умолчанию сохраняем (нужен API)");
        assert!(spec.unshare_pid);
        assert!(spec.die_with_parent);
        assert!(spec.new_session);
    }

    #[test]
    fn builder_dedups_binds_keeping_order() {
        let spec = BwrapSpec::builder()
            .rw_bind("/a")
            .rw_bind("/a")
            .rw_bind("/b")
            .ro_bind("/a/x")
            .ro_bind("/a/x")
            .build()
            .unwrap();
        assert_eq!(spec.rw_binds, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(spec.ro_binds, vec![PathBuf::from("/a/x")]);
    }

    #[test]
    fn builder_rejects_relative_paths() {
        let err = BwrapSpec::builder().ro_bind("relative/dir").build().unwrap_err();
        assert_eq!(err, BwrapError::NotAbsolute(PathBuf::from("relative/dir")));
        let err = BwrapSpec::builder().rw_bind("rel").build().unwrap_err();
        assert!(matches!(err, BwrapError::NotAbsolute(_)));
    }

    #[test]
    fn builder_rejects_exact_ro_rw_overlap() {
        let err = BwrapSpec::builder().ro_bind("/x").rw_bind("/x").build().unwrap_err();
        match err {
            BwrapError::RoRwOverlap { ro, rw } => {
                assert_eq!(ro, PathBuf::from("/x"));
                assert_eq!(rw, PathBuf::from("/x"));
            }
            other => panic!("ожидался RoRwOverlap, получен {other:?}"),
        }
    }

    #[test]
    fn builder_rejects_rw_under_ro() {
        // ro-бинд применится позже и скроет rw-подпуть — это ошибка конфига
        let err = BwrapSpec::builder().ro_bind("/a").rw_bind("/a/b").build().unwrap_err();
        assert!(matches!(err, BwrapError::RoRwOverlap { .. }));
    }

    #[test]
    fn builder_allows_ro_carveout_under_rw() {
        // обратное вложение — легальный ro-вырез (как .git внутри workspace)
        BwrapSpec::builder().rw_bind("/a").ro_bind("/a/b").build().unwrap();
        // и совсем непересекающиеся ветки
        BwrapSpec::builder().rw_bind("/x").ro_bind("/y").build().unwrap();
    }

    #[test]
    fn for_workspace_helper() {
        let spec = BwrapSpec::for_workspace(Path::new("/tmp/ws")).unwrap();
        assert_eq!(spec.rw_binds, vec![PathBuf::from("/tmp/ws")]);
        assert!(BwrapSpec::for_workspace(Path::new("rel")).is_err());
    }

    #[test]
    fn wrap_command_exact_layout() {
        let cmd = wrap_command(&ws_spec(), &["echo".to_string(), "hi".to_string()]);
        let expected = vec![
            "bwrap",
            "--new-session",
            "--die-with-parent",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--bind",
            "/ws",
            "/ws",
            "--ro-bind",
            "/ws/.git",
            "/ws/.git",
            "--unshare-user",
            "--unshare-pid",
            "--",
            "echo",
            "hi",
        ];
        assert_eq!(cmd, expected);
    }

    #[test]
    fn wrap_command_flag_toggles() {
        let spec = BwrapSpec {
            unshare_net: true,
            unshare_pid: false,
            die_with_parent: false,
            new_session: false,
            ..Default::default()
        };
        let cmd = wrap_command(&spec, &["echo".to_string()]);
        assert!(cmd.contains(&"--unshare-net".to_string()));
        assert!(!cmd.contains(&"--unshare-pid".to_string()));
        assert!(!cmd.contains(&"--new-session".to_string()));
        assert!(!cmd.contains(&"--die-with-parent".to_string()));
        // user namespace — всегда
        assert!(cmd.contains(&"--unshare-user".to_string()));
        // --unshare-all — никогда (он безусловно отрезает сеть)
        assert!(!cmd.contains(&"--unshare-all".to_string()));
    }

    #[test]
    fn wrap_command_ro_binds_come_after_rw() {
        let cmd = wrap_command(&ws_spec(), &["true".to_string()]);
        let pos = |needle: &str| cmd.iter().position(|a| a == needle).unwrap();
        // порядок аргументов = порядок монтирования: rw раньше ro-выреза
        assert!(pos("/ws") < pos("/ws/.git"), "rw-бинд должен идти раньше ro");
        // и всё это — после базового дерева
        assert!(pos("/tmp") < pos("/ws"));
    }

    #[test]
    fn wrap_command_empty_argv_keeps_separator() {
        // чистая функция: пустая команда — ответственность run_wrapped
        let cmd = wrap_command(&BwrapSpec::default(), &[]);
        assert_eq!(cmd.last().unwrap(), "--");
    }

    #[test]
    fn parse_version_variants() {
        assert_eq!(parse_version("bubblewrap 0.9.0\n"), Some((0, 9, 0)));
        assert_eq!(parse_version("bubblewrap 1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("1.0.0"), Some((1, 0, 0)));
        assert_eq!(parse_version("bubblewrap 0.10.1-rc1"), Some((0, 10, 1)));
        assert_eq!(parse_version("  bwrap версия 2.0.0 сборка 5"), Some((2, 0, 0)));
    }

    #[test]
    fn parse_version_garbage_is_none() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("bubblewrap"), None);
        assert_eq!(parse_version("нет цифр в начале токенов x.9"), None);
        assert_eq!(parse_version(".9.0"), None);
    }

    #[test]
    fn detect_agrees_with_probe() {
        // probe не должен падать ни на машине без bwrap, ни с ним
        let detected = detect();
        let status = probe();
        assert_eq!(
            detected.is_none(),
            status == BwrapStatus::Missing,
            "detect и probe обязаны согласовываться"
        );
        // повторный вызов — тот же (кэшированный) результат
        assert_eq!(probe(), status);
        if let Some(path) = detected {
            assert!(path.exists());
            assert_eq!(path.file_name().unwrap(), "bwrap");
            assert!(bwrap_version(&path).is_some());
        }
    }

    #[test]
    fn probe_detail_is_consistent() {
        let detail = probe_detail();
        assert_eq!(detail.status, probe());
        match detail.status {
            BwrapStatus::Available => {
                assert_eq!(detail.exit_code, Some(0));
                assert!(detail.version.is_some());
            }
            BwrapStatus::Missing => assert!(detail.exit_code.is_none()),
            BwrapStatus::NoUserNs => {
                // почти всегда bwrap жалуется в stderr на namespaces
                assert!(!detail.stderr.is_empty());
            }
        }
    }

    #[test]
    fn fallback_plan_matrix() {
        assert_eq!(
            fallback_plan(BwrapStatus::Available, true),
            SandboxPlan::BwrapPlusLandlock
        );
        assert_eq!(fallback_plan(BwrapStatus::Available, false), SandboxPlan::BwrapOnly);
        assert_eq!(fallback_plan(BwrapStatus::Missing, true), SandboxPlan::LandlockOnly);
        assert_eq!(fallback_plan(BwrapStatus::NoUserNs, true), SandboxPlan::LandlockOnly);
        assert_eq!(fallback_plan(BwrapStatus::Missing, false), SandboxPlan::Unprotected);
        assert_eq!(fallback_plan(BwrapStatus::NoUserNs, false), SandboxPlan::Unprotected);
    }

    #[test]
    fn landlock_probe_runs_and_is_stable() {
        // значение зависит от ядра; важно, что не паникует и детерминировано
        let first = landlock_available();
        assert_eq!(landlock_available(), first);
    }

    #[test]
    fn current_plan_matches_matrix() {
        assert_eq!(current_plan(), fallback_plan(probe(), landlock_available()));
    }

    #[test]
    fn run_wrapped_rejects_empty_argv() {
        let err = run_wrapped(&BwrapSpec::default(), &[]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let inner = err.into_inner().unwrap();
        assert_eq!(inner.to_string(), BwrapError::EmptyCommand.to_string());
    }

    #[test]
    fn run_wrapped_rejects_invalid_spec() {
        let spec = BwrapSpec {
            ro_binds: vec![PathBuf::from("relative")],
            ..Default::default()
        };
        let err = run_wrapped(&spec, &["true".to_string()]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn real_echo_when_bwrap_available() {
        if !bwrap_usable() {
            eprintln!("пропуск: bwrap недоступен на этой машине");
            return;
        }
        let status =
            run_wrapped(&BwrapSpec::default(), &["echo".to_string(), "sandbox-ok".to_string()])
                .unwrap();
        assert!(status.success(), "echo под bwrap должен отработать");
        // и с отрезанной сетью процесс тоже обязан запускаться
        let net_spec = BwrapSpec::builder().unshare_net(true).build().unwrap();
        let status = run_wrapped(&net_spec, &["echo".to_string(), "no-net".to_string()]).unwrap();
        assert!(status.success());
    }

    #[test]
    fn real_fs_isolation_when_bwrap_available() {
        if !bwrap_usable() {
            eprintln!("пропуск: bwrap недоступен на этой машине");
            return;
        }
        let dir = std::env::temp_dir().join(format!("theseus_bwrap_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spec = BwrapSpec::builder().rw_bind(&dir).build().unwrap();
        // запись в rw-бинд разрешена (и видна снаружи)
        let inside = dir.join("ok.txt");
        let status = run_wrapped(
            &spec,
            &["touch".to_string(), inside.to_string_lossy().to_string()],
        )
        .unwrap();
        assert!(status.success(), "запись в rw-бинд должна работать");
        assert!(inside.exists());
        // запись вне rw-биндов блокируется ro-корнем
        let status = run_wrapped(
            &spec,
            &["touch".to_string(), "/etc/theseus_bwrap_forbidden".to_string()],
        )
        .unwrap();
        assert!(!status.success(), "запись в ro-корень должна блокироваться");
        std::fs::remove_dir_all(&dir).ok();
    }
}
