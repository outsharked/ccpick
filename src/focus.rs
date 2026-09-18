//! Focusing the terminal window that runs a session. Agent-neutral.
//!
//! A session is identified only by a pid, so its terminal has to be found from that.
//!
//! In WSL the reliable route is the session's own interop socket (`WSL_INTEROP`): a Windows
//! process started through it is parented to that session's terminal, so it can walk up from
//! itself to the first ancestor with a window. (Walking `/proc` for `NtTgid` only works when the
//! shell descends from a Windows interop launch, which isn't the case with systemd enabled, so
//! it is kept only as a fallback.) On Windows the ancestor chain is walked directly.
use crate::env::{Env, HostContext};
use crate::process::PidDomain;
use std::process::{Command, Stdio};

/// Give up rather than loop forever on a malformed or cyclic process chain.
const MAX_HOPS: usize = 32;

/// `PPid` and `NtTgid` from a `/proc/<pid>/status` file. `NtTgid` only exists under WSL, on
/// processes that came from Windows interop, and holds the Windows pid of the terminal host.
pub fn parse_proc_status(text: &str) -> (Option<u32>, Option<u32>) {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            line.strip_prefix(name)?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
    };
    (field("PPid:"), field("NtTgid:"))
}

/// Windows pid of the terminal hosting `pid`, by walking parents until one has `NtTgid`.
/// `read_status` returns the contents of `/proc/<pid>/status`.
pub fn windows_host_pid(pid: u32, read_status: impl Fn(u32) -> Option<String>) -> Option<u32> {
    let mut current = pid;
    for _ in 0..MAX_HOPS {
        let (ppid, nt_tgid) = parse_proc_status(&read_status(current)?);
        if let Some(win_pid) = nt_tgid.filter(|p| *p != 0) {
            return Some(win_pid);
        }
        let parent = ppid?;
        if parent == 0 || parent == current {
            return None;
        }
        current = parent;
    }
    None
}

/// The session's interop socket path from its `/proc/<pid>/environ`, used to launch a Windows
/// process attached to that session's terminal.
pub fn parse_interop_socket(environ: &[u8]) -> Option<String> {
    environ
        .split(|b| *b == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix("WSL_INTEROP="))
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// A PowerShell script that focuses a session's terminal.
///
/// `target` says how the script finds the session's console: `Console(pid)` attaches to that
/// pid's console (its own ancestors may be gone, but the console still belongs to the terminal),
/// while `Inherited` is for a helper launched through the session's interop socket, which is
/// already inside it.
///
/// Identifying the *tab* uses `marker` when the script owns the console title (the tab title
/// follows the console title, so setting a unique value makes the tab findable and the original
/// is put back), and otherwise falls back to matching `tab_title`.
pub fn focus_script(target: Target, marker: &str, tab_title: Option<&str>) -> String {
    let start = match target {
        Target::Console(pid) => pid.to_string(),
        Target::Inherited => "$PID".to_string(),
    };
    let attach = match target {
        Target::Console(_) => concat!(
            "[CcPick.Win]::FreeConsole() | Out-Null; ",
            "$attached = [CcPick.Win]::AttachConsole($id); ",
            "if ($attached) { $console = [CcPick.Win]::GetConsoleWindow(); ",
            "if ($console -ne 0) { $owner = 0; ",
            "[CcPick.Win]::GetWindowThreadProcessId($console, [ref]$owner) | Out-Null; ",
            "if ($owner -ne 0) { $id = $owner } } }; "
        ),
        // A helper started through the interop socket already shares the session's console, but
        // PowerShell has overwritten its title, so the original can't be restored: match on the
        // title ccpick knows instead of planting a marker.
        Target::Inherited => "$attached = $false; ",
    };
    let find_tab = match (target, tab_title) {
        (Target::Console(_), _) => format!(
            "if ($attached) {{ \
$sb = New-Object System.Text.StringBuilder 1024; \
[CcPick.Win]::GetConsoleTitleW($sb, 1024) | Out-Null; $old = $sb.ToString(); \
if ([CcPick.Win]::SetConsoleTitleW({marker})) {{ \
Start-Sleep -Milliseconds 350; \
$hit = Find-Tab {marker} $true; \
if ($hit) {{ $hwnd = $hit }} \
[CcPick.Win]::SetConsoleTitleW($old) | Out-Null }} }}; ",
            marker = powershell_literal(marker)
        ),
        (Target::Inherited, Some(title)) if !title.trim().is_empty() => format!(
            "$hit = Find-Tab {want} $false; if ($hit) {{ $hwnd = $hit }}; ",
            want = powershell_literal(title.trim())
        ),
        (Target::Inherited, _) => String::new(),
    };
    format!(
        "$ErrorActionPreference='SilentlyContinue'; \
Add-Type -Namespace CcPick -Name Win -MemberDefinition '\
[DllImport(\"user32.dll\")] public static extern bool SetForegroundWindow(IntPtr h); \
[DllImport(\"user32.dll\")] public static extern bool ShowWindow(IntPtr h, int c); \
[DllImport(\"user32.dll\")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid); \
[DllImport(\"kernel32.dll\")] public static extern bool AttachConsole(uint p); \
[DllImport(\"kernel32.dll\")] public static extern bool FreeConsole(); \
[DllImport(\"kernel32.dll\", CharSet=CharSet.Unicode)] public static extern bool SetConsoleTitleW(string t); \
[DllImport(\"kernel32.dll\", CharSet=CharSet.Unicode)] public static extern int GetConsoleTitleW(System.Text.StringBuilder b, int n);'; \
function Find-Tab($want, $exact) {{ \
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes; \
$root = [System.Windows.Automation.AutomationElement]::RootElement; \
$isWindow = New-Object System.Windows.Automation.PropertyCondition(\
[System.Windows.Automation.AutomationElement]::ControlTypeProperty, \
[System.Windows.Automation.ControlType]::Window); \
$isTab = New-Object System.Windows.Automation.PropertyCondition(\
[System.Windows.Automation.AutomationElement]::ControlTypeProperty, \
[System.Windows.Automation.ControlType]::TabItem); \
foreach ($window in $root.FindAll([System.Windows.Automation.TreeScope]::Children, $isWindow)) {{ \
$tabs = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, $isTab); \
$hits = @($tabs | Where-Object {{ if ($exact) {{ $_.Current.Name -like \"*$want*\" }} else {{ \
($_.Current.Name -replace '^[^\\p{{L}}\\p{{N}}~/\\\\]+', '').Trim() -ieq $want }} }}); \
if ($hits.Count -eq 1) {{ \
$hits[0].GetCurrentPattern(\
[System.Windows.Automation.SelectionItemPattern]::Pattern).Select(); \
return $window.Current.NativeWindowHandle }} }} \
return 0 }}; \
$id={start}; $hwnd=0; \
{attach}\
{find_tab}\
if ($hwnd -eq 0) {{ \
for ($i = 0; $i -lt {MAX_HOPS} -and $id; $i++) {{ \
$p = Get-Process -Id $id; \
if ($p -and $p.MainWindowHandle -ne 0) {{ $hwnd = $p.MainWindowHandle; break }} \
$id = (Get-CimInstance Win32_Process -Filter \"ProcessId=$id\").ParentProcessId }} }}; \
if ($hwnd -ne 0) {{ [CcPick.Win]::ShowWindow($hwnd, 9); [CcPick.Win]::SetForegroundWindow($hwnd) }}"
    )
}

/// How the focus script reaches the session's console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Attach to this pid's console (a Windows session).
    Console(u32),
    /// Already inside the session's console (launched through its interop socket).
    Inherited,
}

/// Single-quoted PowerShell literal.
fn powershell_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// A value unlikely to collide with a real tab title.
fn tab_marker(pid: u32) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("ccpick-{pid}-{nanos:08x}")
}

/// Focuses the terminal running a session. Ok holds a message for the status line.
pub fn focus_session(
    pid: u32,
    domain: PidDomain,
    host: &HostContext,
    tab_title: Option<&str>,
) -> Result<String, String> {
    match (&host.env, domain) {
        (Env::Wsl { .. }, PidDomain::Linux) => {
            // The session's own interop socket puts the helper inside its terminal.
            let socket = std::fs::read(format!("/proc/{pid}/environ"))
                .ok()
                .as_deref()
                .and_then(parse_interop_socket)
                .ok_or("could not find the terminal window for this session")?;
            run_focus_script(
                &focus_script(Target::Inherited, &tab_marker(pid), tab_title),
                Some(&socket),
                host,
            )
        }
        (Env::Wsl { .. } | Env::Windows, PidDomain::Windows) => run_focus_script(
            &focus_script(Target::Console(pid), &tab_marker(pid), tab_title),
            None,
            host,
        ),
        (Env::Windows, PidDomain::Linux) => {
            Err("focusing a WSL session's terminal from Windows isn't supported".into())
        }
        (host_env, _) => Err(format!(
            "focusing a terminal isn't supported on {}",
            host_env.display_name()
        )),
    }
}

/// Spawns PowerShell with `script`, optionally through a session's interop socket. Fire-and-forget:
/// PowerShell start-up takes a second or two and the UI must not block on it.
fn run_focus_script(
    script: &str,
    interop_socket: Option<&str>,
    host: &HostContext,
) -> Result<String, String> {
    let args = [
        "-NoProfile",
        "-NonInteractive",
        "-WindowStyle",
        "Hidden",
        "-Command",
        script,
    ];
    for tool in crate::env::windows_tool_candidates("powershell.exe", &host.wsl_mount_root) {
        let mut command = Command::new(&tool);
        if let Some(socket) = interop_socket {
            command.env("WSL_INTEROP", socket);
        }
        let spawned = command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if spawned.is_ok() {
            return Ok(String::new());
        }
    }
    Err("could not run powershell.exe to focus the window".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const STATUS: &str =
        "Name:\tclaude\nState:\tS (sleeping)\nTgid:\t100\nNtTgid:\t4242\nPPid:\t90\n";

    #[test]
    fn parses_ppid_and_nt_tgid() {
        assert_eq!(parse_proc_status(STATUS), (Some(90), Some(4242)));
        assert_eq!(
            parse_proc_status("Name:\tbash\nPPid:\t7\n"),
            (Some(7), None)
        );
        assert_eq!(parse_proc_status(""), (None, None));
    }

    fn chain(entries: &[(u32, &str)]) -> impl Fn(u32) -> Option<String> + use<> {
        let map: HashMap<u32, String> = entries
            .iter()
            .map(|(pid, text)| (*pid, (*text).to_string()))
            .collect();
        move |pid| map.get(&pid).cloned()
    }

    #[test]
    fn walks_parents_to_the_terminal_host() {
        let read = chain(&[
            (100, "PPid:\t90\n"),
            (90, "PPid:\t80\n"),
            (80, "PPid:\t1\nNtTgid:\t4242\n"),
        ]);
        assert_eq!(windows_host_pid(100, read), Some(4242));
    }

    #[test]
    fn gives_up_without_a_terminal_host() {
        // No NtTgid anywhere (a native Linux process tree).
        let plain = chain(&[(5, "PPid:\t1\n"), (1, "PPid:\t0\n")]);
        assert_eq!(windows_host_pid(5, plain), None);
        // A cycle, and an unreadable process.
        let cyclic = chain(&[(5, "PPid:\t6\n"), (6, "PPid:\t5\n")]);
        assert_eq!(windows_host_pid(5, cyclic), None);
        assert_eq!(windows_host_pid(9, chain(&[])), None);
        // NtTgid 0 means no interop parent.
        assert_eq!(
            windows_host_pid(5, chain(&[(5, "NtTgid:\t0\nPPid:\t0\n")])),
            None
        );
    }

    #[test]
    fn reads_the_sessions_interop_socket_from_its_environ() {
        let environ = b"SHELL=/bin/zsh\0WSL_INTEROP=/run/WSL/1138643_interop\0TERM=xterm\0";
        assert_eq!(
            parse_interop_socket(environ).as_deref(),
            Some("/run/WSL/1138643_interop")
        );
        assert_eq!(parse_interop_socket(b"WSL_INTEROP=\0"), None);
        assert_eq!(parse_interop_socket(b"TERM=xterm\0"), None);
        assert_eq!(parse_interop_socket(b""), None);
    }

    #[test]
    fn a_windows_session_is_found_through_its_console_and_a_marker_title() {
        let script = focus_script(Target::Console(58892), "ccpick-58892-0001", None);
        assert!(script.contains("$id=58892"));
        assert!(script.contains("AttachConsole($id)"));
        // The tab is identified by planting a unique title, then the original is restored.
        assert!(script.contains("SetConsoleTitleW('ccpick-58892-0001')"));
        assert!(script.contains("$old = $sb.ToString()"));
        assert!(script.contains("SetConsoleTitleW($old)"));
        assert!(script.contains("SelectionItemPattern"));
        // And the window is still raised.
        assert!(script.contains("SetForegroundWindow"));
    }

    #[test]
    fn an_interop_launched_helper_matches_on_the_title_it_was_given() {
        let script = focus_script(Target::Inherited, "unused", Some("  PBS Main backup  "));
        assert!(script.contains("$id=$PID"));
        // It shares the session's console but can't restore the title, so no marker is planted.
        assert!(!script.contains("AttachConsole($id)"));
        assert!(!script.contains("SetConsoleTitleW('unused')"));
        assert!(script.contains("Find-Tab 'PBS Main backup' $false"));
        // Apostrophes are doubled so a title can't break out of the literal.
        let quoted = focus_script(Target::Inherited, "m", Some("it's mine"));
        assert!(quoted.contains("'it''s mine'"));
        // Without a title it just finds the window (the helper is defined but never called).
        let bare = focus_script(Target::Inherited, "m", Some("   "));
        assert!(!bare.contains("$hit = Find-Tab"));
        assert!(bare.contains("SetForegroundWindow"));
    }

    #[test]
    fn markers_differ_between_calls() {
        assert_ne!(tab_marker(7), tab_marker(7));
        assert!(tab_marker(7).starts_with("ccpick-7-"));
    }

    #[test]
    fn unsupported_hosts_explain_themselves() {
        let linux = HostContext {
            env: Env::Linux,
            wsl_mount_root: "/mnt/".into(),
        };
        let error = focus_session(1, PidDomain::Linux, &linux, None).unwrap_err();
        assert_eq!(error, "focusing a terminal isn't supported on Linux");
        let mac = HostContext {
            env: Env::MacOs,
            wsl_mount_root: "/mnt/".into(),
        };
        assert!(
            focus_session(1, PidDomain::Windows, &mac, None)
                .unwrap_err()
                .contains("macOS")
        );
    }
}
