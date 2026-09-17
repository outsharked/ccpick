//! Starting the agent for a LaunchPlan: `exec` on Unix, spawn-and-wait on Windows.
use crate::model::LaunchPlan;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn command(plan: &LaunchPlan) -> Command {
    let mut cmd = Command::new(&plan.argv[0]);
    cmd.args(&plan.argv[1..]).current_dir(&plan.cwd);
    for key in &plan.env_remove {
        cmd.env_remove(key);
    }
    for (key, value) in &plan.env_set {
        cmd.env(key, value);
    }
    cmd
}

/// Replaces the current process. Only returns on failure.
#[cfg(unix)]
pub fn exec(plan: &LaunchPlan) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    command(plan).exec()
}

/// Claims every Ctrl-C/Ctrl-Break event as handled (returns TRUE) without acting on it, so
/// ccpick itself ignores them while the agent runs.
#[cfg(windows)]
unsafe extern "system" fn ignore_ctrl_c(_event: u32) -> windows_sys::core::BOOL {
    1
}

/// Runs the agent in this console and returns its exit code. Ctrl-C goes to the agent, not
/// ccpick, while it runs.
#[cfg(windows)]
pub fn run_and_wait(plan: &LaunchPlan) -> std::io::Result<i32> {
    // SAFETY: `ignore_ctrl_c` matches the required PHANDLER_ROUTINE signature. Registering an
    // explicit handler function (rather than passing a null handler, which every child process
    // inherits and would make the launched agent ignore Ctrl-C too) is process-local: the child
    // still gets its own default Ctrl-C handling.
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(ignore_ctrl_c), 1);
    }
    let status = command(plan).status()?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(unix)]
fn is_usable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_usable(path: &Path) -> bool {
    path.is_file()
}

/// Extensions to try for `program` on Windows: none if it already has one, else PATHEXT's.
pub fn windows_exts(program: &str, pathext: Option<&str>) -> Vec<String> {
    if Path::new(program).extension().is_some() {
        return vec![String::new()];
    }
    pathext
        .unwrap_or(".COM;.EXE;.BAT;.CMD")
        .split(';')
        .filter(|e| !e.is_empty())
        .map(|e| e.to_ascii_lowercase())
        .collect()
}

/// First usable `<dir>/<program><ext>`, or `<program><ext>` itself when it contains a separator.
pub fn resolve(
    program: &str,
    dirs: impl IntoIterator<Item = PathBuf>,
    exts: &[String],
    usable: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let names: Vec<String> = exts.iter().map(|e| format!("{program}{e}")).collect();
    if program.contains('/') || program.contains('\\') {
        return names.into_iter().map(PathBuf::from).find(|p| usable(p));
    }
    dirs.into_iter()
        .find_map(|dir| names.iter().map(|n| dir.join(n)).find(|p| usable(p)))
}

pub fn find_in_path(program: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    let exts = if cfg!(windows) {
        windows_exts(program, std::env::var("PATHEXT").ok().as_deref())
    } else {
        vec![String::new()]
    };
    resolve(program, std::env::split_paths(&paths), &exts, is_usable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn builds_command_with_cwd_and_env() {
        let plan = LaunchPlan {
            cwd: PathBuf::from("/work"),
            argv: vec!["claude".into(), "--resume".into(), "abc".into()],
            env_set: vec![("CLAUDE_CONFIG_DIR".into(), "/alt".into())],
            env_remove: vec!["OTHER".into()],
        };
        let cmd = command(&plan);
        assert_eq!(cmd.get_program(), OsStr::new("claude"));
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            vec![OsStr::new("--resume"), OsStr::new("abc")]
        );
        assert_eq!(cmd.get_current_dir(), Some(std::path::Path::new("/work")));
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("OTHER"), None)));
        assert!(envs.contains(&(OsStr::new("CLAUDE_CONFIG_DIR"), Some(OsStr::new("/alt")))));
    }

    #[cfg(unix)]
    #[test]
    fn finds_programs_on_path() {
        assert!(find_in_path("sh").is_some());
        assert!(find_in_path("/bin/sh").is_some());
        assert!(find_in_path("definitely-not-a-real-binary-xyz").is_none());
    }

    use std::path::Path;

    #[test]
    fn windows_exts_use_pathext_unless_program_has_extension() {
        assert_eq!(
            windows_exts("ccs", Some(".COM;.EXE;.CMD;")),
            vec![".com", ".exe", ".cmd"]
        );
        assert_eq!(
            windows_exts("ccs", None),
            vec![".com", ".exe", ".bat", ".cmd"]
        );
        assert_eq!(windows_exts("ccs.cmd", Some(".EXE")), vec![""]);
    }

    #[test]
    fn resolves_first_usable_candidate_in_dir_order() {
        let usable = |p: &Path| {
            let s = p.to_string_lossy().replace('\\', "/");
            s == "/b/ccs.cmd" || s == "/c/ccs.exe" || s == "/a/ccs"
        };
        let dirs = || {
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c"),
            ]
        };
        let exts = vec![".exe".to_string(), ".cmd".to_string()];
        // The extensionless npm shim in /a is ignored when extensions are required.
        assert_eq!(
            resolve("ccs", dirs(), &exts, usable).map(|p| p.to_string_lossy().replace('\\', "/")),
            Some("/b/ccs.cmd".to_string())
        );
        assert_eq!(
            resolve("ccs", dirs(), &[String::new()], usable)
                .map(|p| p.to_string_lossy().replace('\\', "/")),
            Some("/a/ccs".to_string())
        );
        assert_eq!(
            resolve("/b/ccs.cmd", dirs(), &[String::new()], usable),
            Some(PathBuf::from("/b/ccs.cmd"))
        );
        assert_eq!(resolve("missing", dirs(), &exts, usable), None);
    }
}
