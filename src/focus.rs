//! Focusing the terminal window that runs a session. Agent-neutral.
//!
//! A session is identified only by a pid, so its terminal has to be found from that.
//!
//! In WSL the reliable route is the session's own interop socket (`WSL_INTEROP`): a Windows
//! process started through it is parented to that session's terminal, so it can walk up from
//! itself to the first ancestor with a window. (Walking `/proc` for `NtTgid` only works when the
//! shell descends from a Windows interop launch, which isn't the case with systemd enabled, so
//! it is kept only as a fallback.) On Windows the ancestor chain is walked directly.
//!
//! On Linux, GNOME Terminal is supported: it puts the tab's identity in the environment of every
//! process it starts (`GNOME_TERMINAL_SCREEN`), and its GNOME Shell search provider raises a tab
//! by that id, the same way picking a terminal from the desktop search does. Wayland doesn't let
//! one program raise another's window, so going through the terminal itself is what makes this
//! work there. Other Linux terminals expose nothing comparable.
//!
//! That variable is inherited, though, so carrying it does not mean a process is *in* that tab:
//! an app launched from a tab passes the id on to terminals of its own (VS Code's integrated
//! terminal is the case that bites). The ancestor chain is therefore walked to the terminal
//! server before the id is trusted.
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
    environ_var(environ, "WSL_INTEROP").map(str::to_string)
}

/// A non-empty variable from the contents of a `/proc/<pid>/environ` file.
fn environ_var<'a>(environ: &'a [u8], name: &str) -> Option<&'a str> {
    environ
        .split(|b| *b == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix(name)?.strip_prefix('='))
        .filter(|value| !value.is_empty())
}

/// The GNOME Terminal tab a process runs in.
#[derive(Debug, PartialEq, Eq)]
pub struct GnomeTerminalTab {
    /// Bus name of the terminal server that owns the tab (`GNOME_TERMINAL_SERVICE`), when set.
    pub service: Option<String>,
    /// The tab's id as GNOME Terminal's search provider knows it.
    pub id: String,
}

const GNOME_TERMINAL_SCREEN_PREFIX: &str = "/org/gnome/Terminal/screen/";

/// The session's GNOME Terminal tab from its `/proc/<pid>/environ`. The tab's D-Bus object path
/// ends in its UUID with `_` for `-` (D-Bus paths can't hold dashes); the search provider uses
/// the plain UUID.
pub fn parse_gnome_terminal_tab(environ: &[u8]) -> Option<GnomeTerminalTab> {
    let uuid = environ_var(environ, "GNOME_TERMINAL_SCREEN")?
        .strip_prefix(GNOME_TERMINAL_SCREEN_PREFIX)
        .filter(|uuid| !uuid.is_empty() && !uuid.contains('/'))?;
    Some(GnomeTerminalTab {
        service: environ_var(environ, "GNOME_TERMINAL_SERVICE").map(str::to_string),
        id: uuid.replace('_', "-"),
    })
}

/// Names `/proc/<pid>/status` can give GNOME Terminal's server. The kernel truncates `Name` to
/// 15 bytes, which is just where `gnome-terminal-server` loses its `server`.
const GNOME_TERMINAL_SERVER_NAMES: [&str; 2] = ["gnome-terminal-", "gnome-terminal-server"];

/// `Name` from a `/proc/<pid>/status` file.
fn parse_proc_name(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|line| line.strip_prefix("Name:"))
        .map(str::trim)
}

/// Process names that mean the walk has reached the top without passing a terminal.
const INIT_NAMES: [&str; 2] = ["systemd", "init"];

/// The message for a session whose terminal offers no way to be raised.
pub const LINUX_FOCUS_UNSUPPORTED: &str =
    "focusing a terminal on Linux works only with GNOME Terminal";

/// What a session's ancestry says about focusing it.
#[derive(Debug, PartialEq, Eq)]
pub enum TerminalHost {
    /// A GNOME Terminal tab, so `GNOME_TERMINAL_SCREEN` is this session's own tab and can be
    /// acted on.
    GnomeTerminal,
    /// Another app: the pid it was found at, and the name `/proc` gives it (truncated to 15
    /// bytes by the kernel). Any tab id the session carries was inherited from it.
    Other { pid: u32, name: String },
    /// Nothing readable to go on.
    Unknown,
}

/// What runs `pid`, by walking its parents to the first terminal server.
/// `GNOME_TERMINAL_SCREEN` is inherited by every descendant, so carrying one does not mean a
/// process is *in* that tab: VS Code launched from a tab hands its id to every integrated
/// terminal it opens, and focusing such a session would raise that unrelated tab -- or, once it
/// has been closed, whatever the search provider falls back to, which looks like a tab picked at
/// random. Finding the server in the chain is what distinguishes a session's own tab from one an
/// ancestor was started in, and it rejects a session under a daemonized multiplexer too, where
/// no one tab can be meant.
pub fn terminal_host(pid: u32, read_status: impl Fn(u32) -> Option<String>) -> TerminalHost {
    let mut current = pid;
    let mut host = None;
    for hop in 0..MAX_HOPS {
        let Some(status) = read_status(current) else {
            break;
        };
        let name = parse_proc_name(&status).unwrap_or_default();
        if GNOME_TERMINAL_SERVER_NAMES.contains(&name) {
            return TerminalHost::GnomeTerminal;
        }
        // The session itself names nothing the user doesn't already know, and every chain ends
        // at init, so the useful name is the outermost one between them -- the app, not the
        // shell it happens to run.
        if hop > 0 && !name.is_empty() && !INIT_NAMES.contains(&name) {
            host = Some((current, name.to_string()));
        }
        match parse_proc_status(&status).0 {
            Some(parent) if parent != 0 && parent != current => current = parent,
            _ => break,
        }
    }
    host.map_or(TerminalHost::Unknown, |(pid, name)| TerminalHost::Other {
        pid,
        name,
    })
}

/// The basename of `argv[0]` from a `/proc/<pid>/cmdline`. Unlike `/proc`'s `Name` this isn't
/// truncated, and it is what a desktop entry's `Exec` can be matched against.
pub fn exec_basename(cmdline: &[u8]) -> Option<&str> {
    let argv0 = cmdline.split(|b| *b == 0).next()?;
    let argv0 = std::str::from_utf8(argv0).ok()?;
    basename(argv0).filter(|name| !name.is_empty())
}

fn basename(path: &str) -> Option<&str> {
    path.rsplit('/').next()
}

/// The `Name` and `Exec` basename of a desktop entry, or `None` when it isn't an application the
/// user would recognise: `NoDisplay`/`Hidden` entries exist for things like VS Code's URL
/// handler, whose name ("Visual Studio Code - URL Handler") describes no window anyone sees.
/// Only the `[Desktop Entry]` section counts -- the `[Desktop Action ...]` sections that follow
/// carry names of their own ("New Empty Window") for items in an app's right-click menu.
pub fn parse_desktop_entry(text: &str) -> Option<(&str, &str)> {
    let mut name = None;
    let mut exec = None;
    let mut is_application = true;
    let mut lines = text.lines().skip_while(|l| l.trim() != "[Desktop Entry]");
    lines.next()?;
    for line in lines.take_while(|l| !l.trim_start().starts_with('[')) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Name" => name = name.or(Some(value)),
            "Exec" => exec = exec.or(Some(value)),
            "Type" => is_application = value == "Application",
            "NoDisplay" | "Hidden" if value == "true" => return None,
            _ => {}
        }
    }
    if !is_application {
        return None;
    }
    Some((name?, basename(exec_program(exec?)?)?))
}

/// The program an `Exec=` line runs. Its first token is not always it: the line can be prefixed
/// with `env VAR=value`, and a path with spaces in it is quoted, so splitting on whitespace
/// would cut `"/opt/My App/bin/app"` in half.
fn exec_program(exec: &str) -> Option<&str> {
    let mut rest = exec.trim_start();
    while !rest.is_empty() {
        let (token, remainder) = match rest.strip_prefix('"') {
            Some(quoted) => {
                let end = quoted.find('"')?;
                (&quoted[..end], &quoted[end + 1..])
            }
            None => match rest.find(char::is_whitespace) {
                Some(end) => (&rest[..end], &rest[end..]),
                None => (rest, ""),
            },
        };
        if !token.is_empty() && token != "env" && !token.contains('=') {
            return Some(token);
        }
        rest = remainder.trim_start();
    }
    None
}

/// The name a desktop entry gives the program `exec`, so a refusal can say "Visual Studio Code"
/// where `/proc` only offers `code`.
pub fn desktop_app_name<'a>(
    exec: &str,
    entries: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    entries.into_iter().find_map(|text| {
        let (name, program) = parse_desktop_entry(text)?;
        (program == exec).then(|| name.to_string())
    })
}

/// The GNOME Terminal tab `pid` runs in: its own tab id, never one it inherited. `read_environ`
/// and `read_status` give `/proc/<pid>/environ` and `/proc/<pid>/status`, and `app_name` turns
/// the app in the way into something to call it by, so the refusal can name it.
pub fn gnome_terminal_tab(
    pid: u32,
    read_environ: impl Fn(u32) -> Option<Vec<u8>>,
    read_status: impl Fn(u32) -> Option<String>,
    app_name: impl Fn(u32, &str) -> String,
) -> Result<GnomeTerminalTab, String> {
    match terminal_host(pid, read_status) {
        TerminalHost::GnomeTerminal => read_environ(pid)
            .as_deref()
            .and_then(parse_gnome_terminal_tab)
            .ok_or_else(|| LINUX_FOCUS_UNSUPPORTED.to_string()),
        TerminalHost::Other { pid, name } => Err(format!(
            "its terminal ({}) can't be focused; only GNOME Terminal is supported",
            app_name(pid, &name)
        )),
        TerminalHost::Unknown => Err(LINUX_FOCUS_UNSUPPORTED.to_string()),
    }
}

/// Desktop-entry directories, in XDG precedence order: the user's own first.
fn desktop_entry_dirs() -> Vec<std::path::PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        });
    let system =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(system.split(':').filter(|d| !d.is_empty()).map(Into::into))
        .map(|dir| dir.join("applications"))
        .collect()
}

/// Every installed desktop entry's contents. Only read when focusing has already failed, so the
/// cost lands on the path that is about to stop and explain itself anyway.
fn read_desktop_entries() -> Vec<String> {
    /// More than any desktop has, and enough that a directory of junk can't stall the UI.
    const MAX_ENTRIES: usize = 2000;
    let mut out = Vec::new();
    for dir in desktop_entry_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if out.len() >= MAX_ENTRIES {
                return out;
            }
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "desktop")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                out.push(text);
            }
        }
    }
    out
}

/// Names to try for the program at `pid`, best first. `argv[0]`'s basename is the obvious one,
/// but a process that has rewritten its title has its arguments in there too -- Electron apps do
/// this, so VS Code's `argv[0]` is the single string `/usr/share/code/code .` -- which is why
/// what precedes the first space is tried as well. The `/proc` name comes last: the kernel
/// truncates it to 15 bytes, so it is the least likely of the three to match an entry.
pub fn program_candidates<'a>(cmdline: &'a [u8], proc_name: &'a str) -> Vec<&'a str> {
    let mut out = Vec::new();
    if let Some(argv0) = exec_basename(cmdline) {
        out.push(argv0);
        if let Some((before_space, _)) = argv0.split_once(char::is_whitespace)
            && !before_space.is_empty()
        {
            out.push(before_space);
        }
    }
    if !proc_name.is_empty() {
        out.push(proc_name);
    }
    out.dedup();
    out
}

/// What to call the app running at `pid`: the name its desktop entry gives it, else the program
/// itself. `proc_name` is what `/proc` called it, used when the command line says nothing.
fn linux_app_name(pid: u32, proc_name: &str) -> String {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let candidates = program_candidates(&cmdline, proc_name);
    let entries = read_desktop_entries();
    if let Some(name) = candidates
        .iter()
        .find_map(|program| desktop_app_name(program, entries.iter().map(String::as_str)))
    {
        return name;
    }
    // Nothing installed matches, so show the program itself -- but never a title with arguments
    // trailing off it.
    candidates
        .iter()
        .find(|program| !program.contains(char::is_whitespace))
        .unwrap_or(&proc_name)
        .to_string()
}

/// `gdbus` arguments that bring `tab` to the front: the call GNOME Shell makes when a terminal
/// is picked from the desktop search. The timestamp is left at 0 (no user-event time to hand on).
pub fn gnome_terminal_focus_args(tab: &GnomeTerminalTab) -> Vec<String> {
    let service = tab.service.as_deref().unwrap_or("org.gnome.Terminal");
    [
        "call",
        "--session",
        "--dest",
        service,
        "--object-path",
        "/org/gnome/Terminal/SearchProvider",
        "--method",
        "org.gnome.Shell.SearchProvider2.ActivateResult",
        &tab.id,
        "[]",
        "0",
    ]
    .map(str::to_string)
    .to_vec()
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
///
/// That fallback can't demand the tab name equal `tab_title`, because agents decorate the
/// terminal title they set and each does it differently: Claude prefixes a status symbol
/// (`✳ Fix the parser`), Codex appends the user (`Fix the parser | alice`). Leading decoration is
/// stripped, and a trailing one is allowed as long as a separator (`|`, `:`, `-`, `–`, `—`, `·`)
/// follows the title — so the match stays anchored at the start rather than becoming a substring
/// search that any longer title would satisfy. Ambiguity is still rejected by the caller's
/// one-hit rule below, which does nothing rather than raise the wrong tab.
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
$n = ($_.Current.Name -replace '^[^\\p{{L}}\\p{{N}}~/\\\\]+', '').Trim(); \
($n -ieq $want) -or \
($n -imatch ('^' + [regex]::Escape($want) + '\\s*[|:\\u00B7\\u2013\\u2014-]')) }} }}); \
if ($hits.Count -eq 1) {{ \
$hits[0].GetCurrentPattern(\
[System.Windows.Automation.SelectionItemPattern]::Pattern).Select(); \
return $window.Current.NativeWindowHandle }} }} \
return 0 }}; \
$id={start}; $hwnd=0; $last=$null; \
{attach}\
{find_tab}\
if ($hwnd -eq 0) {{ \
for ($i = 0; $i -lt {MAX_HOPS} -and $id; $i++) {{ \
$p = Get-Process -Id $id; \
if ($p -and $id -ne $PID) {{ $last = $p.ProcessName }} \
if ($p -and $p.MainWindowHandle -ne 0) {{ $hwnd = $p.MainWindowHandle; break }} \
$id = (Get-CimInstance Win32_Process -Filter \"ProcessId=$id\").ParentProcessId }} }}; \
if ($hwnd -eq 0 -and $last) {{ \
$cands = @(Get-Process -Name $last | Where-Object {{ $_.MainWindowHandle -ne 0 }}); \
if ($cands.Count -eq 1) {{ $hwnd = $cands[0].MainWindowHandle }} }}; \
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

/// Reads the session's own interop socket and launches the helper through it, from inside the
/// distro. `$1` is the session's pid and `$2` the PowerShell script, both passed as arguments so
/// nothing has to be quoted into the script text.
const WSL_FOCUS_SCRIPT: &str = "socket=$(tr '\\000' '\\n' < /proc/$1/environ \
| sed -n 's/^WSL_INTEROP=//p' | head -n1); \
[ -n \"$socket\" ] || exit 3; \
ps=powershell.exe; \
command -v powershell.exe >/dev/null 2>&1 || \
ps=/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe; \
WSL_INTEROP=$socket exec \"$ps\" -NoProfile -NonInteractive -WindowStyle Hidden -Command \"$2\"";

/// `wsl.exe` arguments that run the focus helper inside `distro` for `pid`.
pub fn wsl_focus_args(distro: &str, pid: u32, script: &str) -> Vec<String> {
    vec![
        "-d".into(),
        distro.into(),
        "--exec".into(),
        "sh".into(),
        "-c".into(),
        WSL_FOCUS_SCRIPT.into(),
        "sh".into(),
        pid.to_string(),
        script.into(),
    ]
}

/// Whether a Linux-domain pid from `env` is in this host's own process namespace, so its
/// `/proc` can be read directly rather than through `wsl.exe`.
fn is_own_namespace(host: &Env, env: &Env) -> bool {
    match (host, env) {
        (Env::Wsl { distro: h }, Env::Wsl { distro }) => {
            h == crate::env::UNKNOWN_DISTRO || h.eq_ignore_ascii_case(distro)
        }
        (Env::Wsl { .. }, _) => true,
        _ => false,
    }
}

/// Focuses the terminal running a session. `env` is the environment the session belongs to and
/// `domain` its process namespace. Ok holds a message for the status line.
pub fn focus_session(
    pid: u32,
    domain: PidDomain,
    env: &Env,
    host: &HostContext,
    tab_title: Option<&str>,
) -> Result<String, String> {
    match (&host.env, domain) {
        (Env::Wsl { .. }, PidDomain::Linux) if is_own_namespace(&host.env, env) => {
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
        // A session in another distro: the same interop-socket route, run from inside it.
        (Env::Wsl { .. } | Env::Windows, PidDomain::Linux) => match env {
            Env::Wsl { distro } => run_focus_script_in_distro(
                distro,
                pid,
                &focus_script(Target::Inherited, &tab_marker(pid), tab_title),
                host,
            ),
            _ => Err("could not find the terminal window for this session".into()),
        },
        (Env::Wsl { .. } | Env::Windows, PidDomain::Windows) => run_focus_script(
            &focus_script(Target::Console(pid), &tab_marker(pid), tab_title),
            None,
            host,
        ),
        (Env::Linux, PidDomain::Linux) => {
            let tab = gnome_terminal_tab(
                pid,
                |p| std::fs::read(format!("/proc/{p}/environ")).ok(),
                |p| std::fs::read_to_string(format!("/proc/{p}/status")).ok(),
                linux_app_name,
            )?;
            run_gnome_terminal_focus(&tab)
        }
        (host_env, _) => Err(format!(
            "focusing a terminal isn't supported on {}",
            host_env.display_name()
        )),
    }
}

/// Asks GNOME Terminal to raise `tab`. Fire-and-forget, like the other helpers.
fn run_gnome_terminal_focus(tab: &GnomeTerminalTab) -> Result<String, String> {
    Command::new("gdbus")
        .args(gnome_terminal_focus_args(tab))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| String::new())
        .map_err(|e| format!("could not run gdbus to focus the terminal: {e}"))
}

/// Spawns the focus helper inside a WSL distro. Fire-and-forget, like `run_focus_script`.
fn run_focus_script_in_distro(
    distro: &str,
    pid: u32,
    script: &str,
    host: &HostContext,
) -> Result<String, String> {
    let args = wsl_focus_args(distro, pid, script);
    for tool in crate::env::windows_tool_candidates("wsl.exe", &host.wsl_mount_root) {
        let spawned = Command::new(&tool)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if spawned.is_ok() {
            return Ok(String::new());
        }
    }
    Err(format!(
        "could not run wsl.exe to focus the terminal in {distro}"
    ))
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
    fn reads_the_sessions_gnome_terminal_tab_from_its_environ() {
        let environ = b"SHELL=/bin/zsh\0GNOME_TERMINAL_SCREEN=/org/gnome/Terminal/screen/4887d8bd_0d54_4c28_b108_761eae95b56c\0GNOME_TERMINAL_SERVICE=:1.102\0TERM=xterm-256color\0";
        // The search provider names tabs by the screen's UUID with dashes, not D-Bus underscores.
        assert_eq!(
            parse_gnome_terminal_tab(environ),
            Some(GnomeTerminalTab {
                service: Some(":1.102".into()),
                id: "4887d8bd-0d54-4c28-b108-761eae95b56c".into(),
            })
        );
        // Without the service name, the well-known one is used later on.
        assert_eq!(
            parse_gnome_terminal_tab(b"GNOME_TERMINAL_SCREEN=/org/gnome/Terminal/screen/ab_cd\0"),
            Some(GnomeTerminalTab {
                service: None,
                id: "ab-cd".into(),
            })
        );
        assert_eq!(parse_gnome_terminal_tab(b"GNOME_TERMINAL_SCREEN=\0"), None);
        assert_eq!(
            parse_gnome_terminal_tab(b"GNOME_TERMINAL_SCREEN=/org/gnome/Terminal/screen/\0"),
            None
        );
        assert_eq!(
            parse_gnome_terminal_tab(b"GNOME_TERMINAL_SCREEN=/some/other/path\0"),
            None
        );
        assert_eq!(parse_gnome_terminal_tab(b"TERM=xterm\0"), None);
        assert_eq!(parse_gnome_terminal_tab(b""), None);
    }

    /// A `/proc/<pid>/status` reader over a `(pid, name, ppid)` process tree.
    fn status_reader(rows: Vec<(u32, &'static str, u32)>) -> impl Fn(u32) -> Option<String> {
        move |pid| {
            rows.iter()
                .find(|(p, _, _)| *p == pid)
                .map(|(_, name, ppid)| {
                    format!("Name:\t{name}\nState:\tS (sleeping)\nPPid:\t{ppid}\n")
                })
        }
    }

    fn stub_app_name(_pid: u32, name: &str) -> String {
        name.to_string()
    }

    const TAB_ENVIRON: &[u8] =
        b"GNOME_TERMINAL_SCREEN=/org/gnome/Terminal/screen/ab_cd\0GNOME_TERMINAL_SERVICE=:1.102\0";

    #[test]
    fn a_session_running_in_a_tab_is_focused_by_its_own_id() {
        // claude <- zsh <- the terminal server, whose /proc `Name` the kernel truncates at 15.
        let proc = status_reader(vec![
            (100, "claude", 90),
            (90, "zsh", 3306),
            (3306, "gnome-terminal-", 1),
            (1, "systemd", 0),
        ]);
        assert_eq!(terminal_host(100, &proc), TerminalHost::GnomeTerminal);
        assert_eq!(
            gnome_terminal_tab(100, |_| Some(TAB_ENVIRON.to_vec()), &proc, stub_app_name),
            Ok(GnomeTerminalTab {
                service: Some(":1.102".into()),
                id: "ab-cd".into(),
            })
        );
        // An untruncated name counts too, in case a kernel ever hands one over.
        let untruncated = status_reader(vec![(100, "gnome-terminal-server", 1)]);
        assert_eq!(
            terminal_host(100, &untruncated),
            TerminalHost::GnomeTerminal
        );
    }

    #[test]
    fn a_tab_id_inherited_by_another_apps_terminal_is_not_trusted() {
        // VS Code launched from a tab passes GNOME_TERMINAL_SCREEN to its integrated terminals,
        // so the id is real but names a tab this session isn't in -- focusing it would raise that
        // unrelated tab. No terminal server in the chain, so the id is refused.
        // Named after the app, not the shell between them and not init at the top: VS Code's
        // own chain puts its extension host in the middle, which is no more use to say than
        // `bash` would be.
        let proc = status_reader(vec![
            (200, "claude", 190),
            (190, "node-MainThread", 185),
            (185, "zsh", 180),
            (180, "code", 170),
            (170, "code", 1),
            (1, "systemd", 0),
        ]);
        assert_eq!(
            terminal_host(200, &proc),
            // The outermost `code`, which is VS Code itself rather than one of its helpers.
            TerminalHost::Other {
                pid: 170,
                name: "code".into()
            }
        );
        // The name comes from the app found in the chain, not from the session's own process.
        assert_eq!(
            gnome_terminal_tab(
                200,
                |_| Some(TAB_ENVIRON.to_vec()),
                &proc,
                |pid, name| format!("{name}@{pid}")
            ),
            Err(
                "its terminal (code@170) can't be focused; only GNOME Terminal is supported".into()
            )
        );
    }

    #[test]
    fn an_unreadable_or_cyclic_process_chain_is_refused_without_looping() {
        // Nothing to read: the process is gone, and nothing can be said about its terminal.
        assert_eq!(terminal_host(1, |_| None), TerminalHost::Unknown);
        assert_eq!(
            gnome_terminal_tab(1, |_| Some(TAB_ENVIRON.to_vec()), |_| None, stub_app_name),
            Err(LINUX_FOCUS_UNSUPPORTED.into())
        );
        // Its own parent, and a two-process cycle: both end rather than spin, and neither is
        // mistaken for a tab.
        let itself = status_reader(vec![(5, "zsh", 5)]);
        assert_eq!(terminal_host(5, &itself), TerminalHost::Unknown);
        let cycle = status_reader(vec![(5, "zsh", 6), (6, "zsh", 5)]);
        assert_ne!(terminal_host(5, &cycle), TerminalHost::GnomeTerminal);
        // In a tab, but with no tab id to act on.
        let in_tab = status_reader(vec![(100, "gnome-terminal-", 1)]);
        assert_eq!(
            gnome_terminal_tab(
                100,
                |_| Some(b"TERM=xterm\0".to_vec()),
                &in_tab,
                stub_app_name
            ),
            Err(LINUX_FOCUS_UNSUPPORTED.into())
        );
        assert_eq!(
            gnome_terminal_tab(100, |_| None, &in_tab, stub_app_name),
            Err(LINUX_FOCUS_UNSUPPORTED.into())
        );
    }

    #[test]
    fn an_app_is_named_as_its_desktop_entry_names_it() {
        // VS Code's entry, as installed: the program is an absolute path, and a second entry for
        // its URL handler carries a name that describes no window anyone sees.
        let vscode = concat!(
            "[Desktop Entry]\n",
            "Name=Visual Studio Code\n",
            "Exec=/usr/share/code/code %F\n",
            "Type=Application\n",
            "StartupWMClass=com.microsoft.VSCode\n",
            "\n",
            "[Desktop Action new-empty-window]\n",
            "Name=New Empty Window\n",
            "Exec=/usr/share/code/code --new-window %F\n",
        );
        let url_handler = concat!(
            "[Desktop Entry]\n",
            "Name=Visual Studio Code - URL Handler\n",
            "Exec=/usr/share/code/code --open-url %U\n",
            "Type=Application\n",
            "NoDisplay=true\n",
        );
        // The action section's own Name is not the app's.
        assert_eq!(
            parse_desktop_entry(vscode),
            Some(("Visual Studio Code", "code"))
        );
        assert_eq!(parse_desktop_entry(url_handler), None);
        // The hidden handler is listed first and still doesn't win.
        assert_eq!(
            desktop_app_name("code", [url_handler, vscode]),
            Some("Visual Studio Code".into())
        );
        assert_eq!(desktop_app_name("ghostty", [vscode]), None);
    }

    #[test]
    fn a_desktop_entry_that_names_no_usable_app_is_skipped() {
        // A link isn't an app, and `Hidden` means uninstalled but left behind.
        assert_eq!(
            parse_desktop_entry("[Desktop Entry]\nName=Docs\nExec=x\nType=Link\n"),
            None
        );
        assert_eq!(
            parse_desktop_entry("[Desktop Entry]\nName=Old\nExec=old\nHidden=true\n"),
            None
        );
        // No `[Desktop Entry]` section at all, and an entry with nothing to run.
        assert_eq!(parse_desktop_entry("Name=Stray\nExec=stray\n"), None);
        assert_eq!(parse_desktop_entry("[Desktop Entry]\nName=Nothing\n"), None);
        // An `env` prefix and quoting are not the program being run.
        assert_eq!(
            parse_desktop_entry(
                "[Desktop Entry]\nName=Wrapped\nExec=env G=1 \"/opt/My App/bin/app\" %U\n"
            ),
            Some(("Wrapped", "app"))
        );
    }

    #[test]
    fn a_process_that_rewrote_its_title_is_still_matched_to_its_app() {
        // Electron rewrites argv[0] into one string, arguments and all, so the basename alone is
        // `code .` and matches nothing. What precedes the space is the program.
        let electron = b"/usr/share/code/code .\0";
        assert_eq!(program_candidates(electron, "code"), vec!["code .", "code"]);
        let vscode = concat!(
            "[Desktop Entry]\n",
            "Name=Visual Studio Code\n",
            "Exec=/usr/share/code/code %F\n",
            "Type=Application\n",
        );
        assert_eq!(desktop_app_name("code .", [vscode]), None);
        assert_eq!(
            program_candidates(electron, "code")
                .iter()
                .find_map(|p| desktop_app_name(p, [vscode])),
            Some("Visual Studio Code".into())
        );
        // An ordinary process offers its basename once, with the truncated /proc name after it.
        assert_eq!(
            program_candidates(b"/usr/bin/ghostty\0--title\0x\0", "ghostty"),
            vec!["ghostty"]
        );
        // Nothing readable: only what /proc called it.
        assert_eq!(
            program_candidates(b"", "gnome-text-edit"),
            vec!["gnome-text-edit"]
        );
    }

    #[test]
    fn the_program_name_comes_from_the_command_line_untruncated() {
        // `/proc` truncates `Name` at 15 bytes; argv[0] doesn't, and it is what a desktop
        // entry's `Exec` can be matched against.
        assert_eq!(exec_basename(b"/usr/share/code/code\0.\0"), Some("code"));
        assert_eq!(
            exec_basename(b"gnome-text-editor\0"),
            Some("gnome-text-editor")
        );
        assert_eq!(exec_basename(b""), None);
        assert_eq!(exec_basename(b"\0"), None);
    }

    #[test]
    fn a_gnome_terminal_tab_is_raised_through_its_search_provider() {
        let tab = GnomeTerminalTab {
            service: Some(":1.102".into()),
            id: "ab-cd".into(),
        };
        assert_eq!(
            gnome_terminal_focus_args(&tab),
            [
                "call",
                "--session",
                "--dest",
                ":1.102",
                "--object-path",
                "/org/gnome/Terminal/SearchProvider",
                "--method",
                "org.gnome.Shell.SearchProvider2.ActivateResult",
                "ab-cd",
                "[]",
                "0",
            ]
        );
        let anonymous = GnomeTerminalTab {
            service: None,
            id: "ab-cd".into(),
        };
        assert_eq!(
            gnome_terminal_focus_args(&anonymous)[3],
            "org.gnome.Terminal"
        );
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
    fn a_session_in_another_distro_is_focused_through_wsl_exe() {
        let args = wsl_focus_args("Ubuntu-24.04", 1234, "$hwnd=0");
        assert_eq!(&args[..5], &["-d", "Ubuntu-24.04", "--exec", "sh", "-c"]);
        // The pid and the script are arguments, never interpolated into the shell text.
        assert_eq!(&args[6..], &["sh", "1234", "$hwnd=0"]);
        let script = &args[5];
        assert!(script.contains("/proc/$1/environ"));
        assert!(script.contains("WSL_INTEROP=//p"));
        assert!(script.contains("WSL_INTEROP=$socket exec"));
        // The helper runs in the session's console, so it matches the tab by title.
        assert!(script.contains("-Command \"$2\""));
    }

    #[test]
    fn a_wsl_session_focused_from_windows_uses_the_title_matching_script() {
        // What run_focus_script_in_distro passes as $2: an Inherited-target script, because the
        // helper is launched inside the session's own console.
        let script = focus_script(Target::Inherited, "unused", Some("hello"));
        assert!(script.contains("$id=$PID"));
        assert!(script.contains("Find-Tab 'hello' $false"));
    }

    #[test]
    fn the_tab_match_allows_decoration_on_either_side_of_the_title() {
        // Real tab names seen in Windows Terminal: Claude renders "\u{2733} <title>", Codex
        // renders "<title> | <user>". Matching the title exactly finds neither reliably, which
        // is why the script normalizes the front and tolerates a separated suffix.
        let script = focus_script(Target::Inherited, "marker", Some("Test"));
        assert!(
            script.contains("-replace '^[^\\p{L}\\p{N}~/\\\\]+', ''"),
            "leading decoration must still be stripped"
        );
        assert!(
            script.contains("[regex]::Escape($want)"),
            "the title must be regex-escaped, not interpolated raw"
        );
        assert!(
            script.contains("'\\s*[|:\\u00B7\\u2013\\u2014-]'"),
            "a trailing decoration must be separator-anchored, not a bare prefix match"
        );
        assert!(
            script.contains("$hits.Count -eq 1"),
            "two candidate tabs must still select neither"
        );
    }

    #[test]
    fn a_broken_parent_chain_falls_back_to_a_sibling_window() {
        // Climbing parents assumes an unbroken chain to a windowed process. VS Code does not
        // provide one: the helper lands in a windowless Electron subprocess whose parent has
        // already exited, so the walk dies with no handle and focusing silently does nothing.
        // Observed on a real machine:
        //   powershell -> wsl -> wsl -> Code (mainwnd=0) -> <gone>
        // When that happens, look sideways for a window owned by a process of the same name.
        let script = focus_script(Target::Inherited, "marker", Some("t"));
        assert!(
            script.contains("$id -ne $PID") && script.contains("$last = $p.ProcessName"),
            "remember the deepest living ancestor's name, never the helper's own: a chain that \
             dies at hop 0 would otherwise raise some unrelated PowerShell window"
        );
        assert!(
            script.contains("Get-Process -Name $last"),
            "the fallback must search by that name"
        );
        assert!(
            script.contains("$cands.Count -eq 1"),
            "more than one candidate window must select neither, as with tabs"
        );
    }

    #[test]
    fn only_wsl_sessions_can_be_focused_in_a_distro() {
        let windows = HostContext {
            env: Env::Windows,
            wsl_mount_root: "/mnt/".into(),
        };
        // A Linux-domain pid that belongs to no distro can't be reached from Windows.
        let error = focus_session(1, PidDomain::Linux, &Env::Linux, &windows, None).unwrap_err();
        assert_eq!(error, "could not find the terminal window for this session");
    }

    #[test]
    fn a_hosts_own_distro_is_not_routed_through_wsl_exe() {
        let ubuntu = Env::Wsl {
            distro: "Ubuntu".into(),
        };
        let host = HostContext {
            env: ubuntu.clone(),
            wsl_mount_root: "/mnt/".into(),
        };
        assert!(is_own_namespace(&host.env, &ubuntu));
        assert!(is_own_namespace(
            &host.env,
            &Env::Wsl {
                distro: "UBUNTU".into()
            }
        ));
        assert!(!is_own_namespace(
            &host.env,
            &Env::Wsl {
                distro: "Debian".into()
            }
        ));
        // A host that doesn't know its own distro name assumes every distro is its own.
        let unknown = Env::Wsl {
            distro: crate::env::UNKNOWN_DISTRO.into(),
        };
        assert!(is_own_namespace(
            &unknown,
            &Env::Wsl {
                distro: "Debian".into()
            }
        ));
        assert!(!is_own_namespace(&Env::Windows, &ubuntu));
    }

    #[test]
    fn unsupported_hosts_explain_themselves() {
        let mac = HostContext {
            env: Env::MacOs,
            wsl_mount_root: "/mnt/".into(),
        };
        assert!(
            focus_session(1, PidDomain::Windows, &Env::MacOs, &mac, None)
                .unwrap_err()
                .contains("macOS")
        );
    }
}
