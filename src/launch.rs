//! Turning a LaunchPlan into a process that replaces ccpick.
use crate::model::LaunchPlan;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
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
pub fn exec(plan: &LaunchPlan) -> std::io::Error {
    command(plan).exec()
}

fn is_executable(path: &PathBuf) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

pub fn find_in_path(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return is_executable(&path).then_some(path);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|dir| dir.join(program)).find(is_executable)
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
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), vec![OsStr::new("--resume"), OsStr::new("abc")]);
        assert_eq!(cmd.get_current_dir(), Some(std::path::Path::new("/work")));
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("OTHER"), None)));
        assert!(envs.contains(&(OsStr::new("CLAUDE_CONFIG_DIR"), Some(OsStr::new("/alt")))));
    }

    #[test]
    fn finds_programs_on_path() {
        assert!(find_in_path("sh").is_some());
        assert!(find_in_path("/bin/sh").is_some());
        assert!(find_in_path("definitely-not-a-real-binary-xyz").is_none());
    }
}
