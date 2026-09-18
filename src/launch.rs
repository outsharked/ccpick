//! Starting the agent for a LaunchPlan: `exec` on Unix, spawn-and-wait on Windows, or opening a
//! new terminal window (the web portal can't hand over its own terminal).
use crate::env::{Env, HostContext};
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

/// The command line that opens `plan` in a *new* terminal window, or `None` when this platform
/// has no terminal ccpick is willing to guess at — the caller then offers the paste-ready
/// command instead of opening something the user didn't ask for.
///
/// `terminal_env` is `$TERMINAL`; `on_path` reports whether a program is runnable, so every
/// platform's decision can be tested on every platform.
pub fn terminal_command(
    plan: &LaunchPlan,
    target: &Env,
    host: &HostContext,
    terminal_env: Option<&str>,
    on_path: &dyn Fn(&str) -> bool,
) -> Option<Vec<String>> {
    match (&host.env, target) {
        (Env::Windows, Env::Windows) => {
            let mut argv = windows_terminal_prefix(on_path);
            // Windows Terminal's own inherited-cwd behaviour isn't reliable when it's spawned
            // by another process rather than typed at a prompt, so pin it explicitly.
            if argv[0] == "wt.exe" && !plan.cwd.as_os_str().is_empty() {
                argv.push("-d".to_string());
                argv.push(plan.cwd.display().to_string());
            }
            argv.extend(plan.argv.iter().cloned());
            Some(argv)
        }
        (Env::Windows, Env::Wsl { distro }) => {
            let mut argv = windows_terminal_prefix(on_path);
            argv.push("wsl.exe".to_string());
            argv.push("-d".to_string());
            argv.push(distro.clone());
            // wsl.exe defaults to the distro's home directory, not our cwd, unless told
            // otherwise; --cd takes a Linux path directly, which is what `plan.cwd` already is
            // for a WSL target.
            if !plan.cwd.as_os_str().is_empty() {
                argv.push("--cd".to_string());
                argv.push(plan.cwd.display().to_string());
            }
            argv.push("--".to_string());
            // `wsl.exe -- <argv>` execs the command directly with no shell in between, so a
            // shell-style `VAR=value` prefix wouldn't be interpreted; `env` applies the same
            // adjustments the native launch path gets from `Command::env`/`env_remove`.
            if !plan.env_set.is_empty() || !plan.env_remove.is_empty() {
                argv.push("env".to_string());
                for key in &plan.env_remove {
                    argv.push("-u".to_string());
                    argv.push(key.clone());
                }
                for (key, value) in &plan.env_set {
                    argv.push(format!("{key}={value}"));
                }
            }
            argv.extend(plan.argv.iter().cloned());
            Some(argv)
        }
        (Env::MacOs, Env::MacOs) => Some(vec![
            "osascript".to_string(),
            "-e".to_string(),
            format!(
                "tell application \"Terminal\" to do script {}",
                // `do script` hands the text to a fresh login shell that Terminal.app spawns
                // itself; it does not inherit osascript's process environment, so cwd and env
                // vars have to be baked into the script text like a pasted command would be.
                applescript_string(&crate::shell::resume_command(plan, target))
            ),
        ]),
        (Env::Linux, Env::Linux) => {
            let chosen = terminal_env
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    [
                        "x-terminal-emulator",
                        "gnome-terminal",
                        "konsole",
                        "alacritty",
                        "kitty",
                        "xterm",
                    ]
                    .into_iter()
                    .find(|name| on_path(name))
                    .map(str::to_string)
                })?;
            let mut argv = vec![chosen, "-e".to_string()];
            argv.extend(plan.argv.iter().cloned());
            Some(argv)
        }
        // Every other combination (a WSL host reaching Windows/macOS, a Linux host reaching
        // WSL, a macOS host reaching anything else, ...) has no terminal ccpick can open
        // reliably; the caller falls back to the paste command.
        _ => None,
    }
}

fn windows_terminal_prefix(on_path: &dyn Fn(&str) -> bool) -> Vec<String> {
    if on_path("wt.exe") {
        vec!["wt.exe".to_string()]
    } else {
        vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            "start".to_string(),
            String::new(),
        ]
    }
}

/// An AppleScript string literal.
fn applescript_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Opens `plan` in a new terminal window. `Err` carries a message for the user.
pub fn spawn_in_new_terminal(
    plan: &LaunchPlan,
    target: &Env,
    host: &HostContext,
) -> Result<(), String> {
    let terminal_env = std::env::var("TERMINAL").ok();
    let argv = terminal_command(plan, target, host, terminal_env.as_deref(), &|name| {
        find_in_path(name).is_some()
    })
    .ok_or("no terminal to open on this system")?;
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).current_dir(&plan.cwd);
    for key in &plan.env_remove {
        command.env_remove(key);
    }
    for (key, value) in &plan.env_set {
        command.env(key, value);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open a terminal: {e}"))
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

    fn plan() -> LaunchPlan {
        LaunchPlan {
            cwd: PathBuf::from("/home/me/proj"),
            argv: vec!["ccs".into(), "c2".into(), "--resume".into(), "abc".into()],
            env_set: vec![],
            env_remove: vec![],
        }
    }
    fn wsl_host() -> HostContext {
        HostContext {
            env: Env::Wsl {
                distro: "Ubuntu".into(),
            },
            wsl_mount_root: PathBuf::from("/mnt/"),
        }
    }
    fn nothing_on_path(_: &str) -> bool {
        false
    }

    #[test]
    fn a_windows_session_opens_in_windows_terminal() {
        let host = HostContext {
            env: Env::Windows,
            ..Default::default()
        };
        let argv = terminal_command(&plan(), &Env::Windows, &host, None, &|name| {
            name == "wt.exe"
        })
        .unwrap();
        assert_eq!(argv[0], "wt.exe");
        assert!(argv.iter().any(|a| a == "--resume"));
    }

    #[test]
    fn windows_target_pins_the_working_directory_via_wt_d() {
        let host = HostContext {
            env: Env::Windows,
            ..Default::default()
        };
        let argv = terminal_command(&plan(), &Env::Windows, &host, None, &|name| {
            name == "wt.exe"
        })
        .unwrap();
        assert!(argv.windows(2).any(|w| w == ["-d", "/home/me/proj"]));
    }

    #[test]
    fn without_windows_terminal_it_falls_back_to_cmd_start() {
        let host = HostContext {
            env: Env::Windows,
            ..Default::default()
        };
        let argv = terminal_command(&plan(), &Env::Windows, &host, None, &nothing_on_path).unwrap();
        assert_eq!(argv[0], "cmd.exe");
        assert_eq!(argv[1], "/c");
        assert_eq!(argv[2], "start");
    }

    #[test]
    fn a_wsl_session_from_windows_goes_through_wsl_exe() {
        let host = HostContext {
            env: Env::Windows,
            ..Default::default()
        };
        let target = Env::Wsl {
            distro: "Ubuntu".into(),
        };
        let argv =
            terminal_command(&plan(), &target, &host, None, &|name| name == "wt.exe").unwrap();
        assert_eq!(argv[0], "wt.exe");
        assert!(argv.windows(3).any(|w| w == ["wsl.exe", "-d", "Ubuntu"]));
    }

    #[test]
    fn wsl_target_sets_cwd_and_env_via_wsl_exe_flags() {
        let host = HostContext {
            env: Env::Windows,
            ..Default::default()
        };
        let target = Env::Wsl {
            distro: "Ubuntu".into(),
        };
        let mut plan = plan();
        plan.env_set
            .push(("CLAUDE_CONFIG_DIR".into(), "/alt".into()));
        plan.env_remove.push("OTHER".into());
        let argv = terminal_command(&plan, &target, &host, None, &|name| name == "wt.exe").unwrap();
        assert!(argv.windows(2).any(|w| w == ["--cd", "/home/me/proj"]));
        assert!(argv.iter().any(|a| a == "OTHER"));
        assert!(argv.iter().any(|a| a == "CLAUDE_CONFIG_DIR=/alt"));
        // The `--` separator (exec, no shell) must come before the env adjustments and the argv.
        let sep = argv.iter().position(|a| a == "--").unwrap();
        let env_pos = argv.iter().position(|a| a == "env").unwrap();
        assert!(sep < env_pos);
    }

    #[test]
    fn the_terminal_env_var_wins_on_linux() {
        let host = HostContext::default();
        let argv =
            terminal_command(&plan(), &Env::Linux, &host, Some("kitty"), &nothing_on_path).unwrap();
        assert_eq!(argv[0], "kitty");
    }

    #[test]
    fn linux_falls_back_to_x_terminal_emulator_then_gives_up() {
        let host = HostContext::default();
        let argv = terminal_command(&plan(), &Env::Linux, &host, None, &|name| {
            name == "x-terminal-emulator"
        })
        .unwrap();
        assert_eq!(argv[0], "x-terminal-emulator");
        // Nothing installed means no command: the caller shows the paste-ready line instead.
        assert_eq!(
            terminal_command(&plan(), &Env::Linux, &host, None, &nothing_on_path),
            None
        );
    }

    #[test]
    fn a_wsl_session_from_wsl_has_no_terminal_to_open() {
        // ccpick can't open a Windows terminal window from inside the distro reliably;
        // the caller falls back to the paste command.
        assert_eq!(
            terminal_command(
                &plan(),
                &Env::Wsl {
                    distro: "Ubuntu".into()
                },
                &wsl_host(),
                None,
                &nothing_on_path
            ),
            None
        );
    }

    #[test]
    fn macos_opens_terminal_with_a_script() {
        let host = HostContext {
            env: Env::MacOs,
            ..Default::default()
        };
        let argv = terminal_command(&plan(), &Env::MacOs, &host, None, &nothing_on_path).unwrap();
        assert_eq!(argv[0], "osascript");
        assert!(argv.last().unwrap().contains("--resume"));
    }

    #[test]
    fn macos_script_carries_cwd_and_env_since_terminal_app_does_not_inherit_them() {
        let host = HostContext {
            env: Env::MacOs,
            ..Default::default()
        };
        let mut plan = plan();
        plan.env_set
            .push(("CLAUDE_CONFIG_DIR".into(), "/alt".into()));
        let argv = terminal_command(&plan, &Env::MacOs, &host, None, &nothing_on_path).unwrap();
        let script = argv.last().unwrap();
        assert!(script.contains("CLAUDE_CONFIG_DIR="));
        assert!(script.contains("/alt"));
        assert!(script.contains("/home/me/proj"));
    }

    #[test]
    fn a_macos_host_has_no_terminal_for_a_non_macos_target() {
        // Cross-machine targets (Windows/WSL) never actually reach a macOS host, but this keeps
        // the match honest rather than shipping an AppleScript built from PowerShell syntax.
        let host = HostContext {
            env: Env::MacOs,
            ..Default::default()
        };
        assert_eq!(
            terminal_command(&plan(), &Env::Windows, &host, None, &nothing_on_path),
            None
        );
    }
}
