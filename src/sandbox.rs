//! Ядерный sandbox на Linux Landlock (урок Codex WorkspaceWrite: kernel-enforced).
//! Чтение — везде (нужны бинарники/библиотеки), запись — только workspace + /tmp
//! и /dev/null (bash-профили пишут туда при инициализации).
//! При недоступности Landlock — мягкая деградация с одноразовым предупреждением.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxStatus {
    Available,
    Unavailable,
}

static STATUS: std::sync::OnceLock<SandboxStatus> = std::sync::OnceLock::new();

/// Ограничить ТЕКУЩИЙ процесс (вызывать только в pre_exec ребёнка!)
pub fn enforce_workspace(workspace: &Path) -> Result<(), String> {
    use landlock::{ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset,
                   RulesetAttr, RulesetCreatedAttr, RulesetStatus};
    let abi = ABI::V1;
    let rw = AccessFs::from_all(abi);
    let ro = AccessFs::from_read(abi);
    let ws_fd = PathFd::new(workspace).map_err(|e| format!("PathFd workspace: {e}"))?;
    let root_fd = PathFd::new("/").map_err(|e| format!("PathFd /: {e}"))?;
    let tmp_fd = PathFd::new("/tmp").map_err(|e| format!("PathFd /tmp: {e}"))?;
    // /dev/null нужен bash-профилям (/etc/profile.d/*), которые пишут туда при старте.
    // Без него каждый bash-вызов загрязняет stderr мусором «Отказано в доступе».
    let devnull_fd = PathFd::new("/dev/null").map_err(|e| format!("PathFd /dev/null: {e}"))?;
    let status = Ruleset::default()
        .handle_access(rw).map_err(|e| format!("handle_access: {e}"))?
        .create().map_err(|e| format!("create ruleset: {e}"))?
        .add_rule(PathBeneath::new(root_fd, ro)).map_err(|e| format!("rule /: {e}"))?
        .add_rule(PathBeneath::new(tmp_fd, rw)).map_err(|e| format!("rule /tmp: {e}"))?
        .add_rule(PathBeneath::new(ws_fd, rw)).map_err(|e| format!("rule workspace: {e}"))?
        .add_rule(PathBeneath::new(devnull_fd, rw)).map_err(|e| format!("rule /dev/null: {e}"))?
        .restrict_self().map_err(|e| format!("restrict_self: {e}"))?;
    match status.ruleset {
        RulesetStatus::FullyEnforced => Ok(()),
        // PartiallyEnforced: какие-то правила ядро проигнорировало (обычно вспомогательные,
        // как /dev/null) — базовые правила защиты (ro /, rw tmp, rw workspace) действуют.
        RulesetStatus::PartiallyEnforced => Ok(()),
        other => Err(format!("landlock не применён: {other:?}")),
    }
}

/// Однократная проверка доступности (probe в дочернем процессе, кэшируется)
pub fn status() -> SandboxStatus {
    *STATUS.get_or_init(|| {
        use std::os::unix::process::CommandExt;
        let r = unsafe {
            std::process::Command::new("/bin/true")
                .pre_exec(|| {
                    enforce_workspace(Path::new("/tmp"))
                        .map_err(std::io::Error::other)?;
                    Ok(())
                })
                .status()
        };
        match r {
            Ok(s) if s.success() => SandboxStatus::Available,
            _ => SandboxStatus::Unavailable,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_runs() {
        // на современных ядрах (>=5.13) Landlock доступен; тест мягкий
        let _ = status();
    }

    #[test]
    fn sandbox_blocks_write_outside() {
        if status() != SandboxStatus::Available { return; }
        use std::os::unix::process::CommandExt;
        let ws = std::env::temp_dir().join("theseus_sbx_ws");
        std::fs::create_dir_all(&ws).unwrap();
        // запись в workspace — должна работать
        let ok = unsafe {
            std::process::Command::new("bash")
                .arg("-c").arg(format!("touch {}/ok.txt", ws.display()))
                .pre_exec({
                    let ws2 = ws.clone();
                    move || {
                        enforce_workspace(&ws2)
                            .map_err(std::io::Error::other)?;
                        Ok(())
                    }
                })
                .status().unwrap()
        };
        assert!(ok.success(), "запись в workspace должна быть разрешена");
        // запись вне workspace (в $HOME) — должна быть запрещена
        let home = std::env::var("HOME").unwrap();
        let fail = unsafe {
            std::process::Command::new("bash")
                .arg("-c").arg(format!("touch {home}/theseus_sbx_forbidden.txt"))
                .pre_exec({
                    move || {
                        enforce_workspace(&ws)
                            .map_err(std::io::Error::other)?;
                        Ok(())
                    }
                })
                .status().unwrap()
        };
        assert!(!fail.success(), "запись вне workspace должна блокироваться Landlock");
    }
}
