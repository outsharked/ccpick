//! "Is this process running?" for pids in Linux or Windows process namespaces. Agent-neutral.
use crate::env::{Env, HostContext, UNKNOWN_DISTRO};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidDomain {
    Linux,
    Windows,
}

impl PidDomain {
    /// From a domain string such as `linux:<machine-id>:pid:[…]` or `win32:<host>`.
    pub fn parse(s: &str) -> Option<PidDomain> {
        let lower = s.to_ascii_lowercase();
        if lower.starts_with("linux") {
            Some(PidDomain::Linux)
        } else if lower.starts_with("win32") || lower.starts_with("windows") {
            Some(PidDomain::Windows)
        } else {
            None
        }
    }

    /// The domain processes started in `env` live in.
    pub fn of(env: &Env) -> Option<PidDomain> {
        match env {
            Env::Linux | Env::Wsl { .. } => Some(PidDomain::Linux),
            Env::Windows => Some(PidDomain::Windows),
            Env::MacOs => None,
        }
    }
}

type Snapshot = Result<HashMap<u32, String>, String>;
/// Pids inside one WSL distro whose command line contains the probed name.
type ProcSnapshot = Result<HashSet<u32>, String>;
/// (pid, open file path) pairs for processes whose command line contains the probed name.
type OpenFiles = Vec<(u32, PathBuf)>;

/// Answers liveness questions for one ccpick run. The Windows snapshot (WSL hosts) is taken at
/// most once, lazily, the first time a Windows-domain pid is actually probed — filtered by the
/// image name that call supplies, so `tasklist.exe` doesn't have to enumerate every process.
pub struct ProcessProbe {
    host: Env,
    /// WSL drive mount root, for finding `tasklist.exe` when it isn't on `PATH`
    /// (`appendWindowsPath = false` in `wsl.conf`). Only meaningful on a WSL host.
    wsl_mount_root: PathBuf,
    snapshot: OnceLock<Snapshot>,
    /// How to take the Windows snapshot, filtered by image name; swappable in tests.
    snapshotter: fn(&str, &Path) -> Snapshot,
    /// One `/proc` snapshot per WSL distro that isn't this host, taken lazily.
    distro_snapshots: Mutex<HashMap<String, Arc<ProcSnapshot>>>,
    /// Running distros, so probing never starts a stopped one. Taken lazily, at most once.
    running: OnceLock<Result<Vec<String>, String>>,
    /// How to take a distro's snapshot and list running distros; swappable in tests.
    distro_snapshotter: fn(&str, &str, &Path) -> ProcSnapshot,
    distro_lister: fn(&Path) -> Result<Vec<String>, String>,
    /// This host's own open-file listing, filtered by process name; taken at most once, like
    /// the Windows snapshot above. Empty on a host with no `/proc` to walk.
    open_files: OnceLock<OpenFiles>,
    /// How to list (pid, open file) pairs on this host; swappable in tests.
    open_file_lister: fn(&str) -> OpenFiles,
}

impl ProcessProbe {
    pub fn new(host: Env) -> ProcessProbe {
        ProcessProbe {
            host,
            wsl_mount_root: PathBuf::from("/mnt/"),
            snapshot: OnceLock::new(),
            snapshotter: take_tasklist_snapshot,
            distro_snapshots: Mutex::new(HashMap::new()),
            running: OnceLock::new(),
            distro_snapshotter: take_wsl_proc_snapshot,
            distro_lister: take_running_distros,
            open_files: OnceLock::new(),
            open_file_lister: linux_open_files,
        }
    }

    /// A probe for the real host, whose WSL mount root (if any) comes from `host`.
    pub fn from_host(host: &HostContext) -> ProcessProbe {
        ProcessProbe {
            wsl_mount_root: host.wsl_mount_root.clone(),
            ..ProcessProbe::new(host.env.clone())
        }
    }

    #[cfg(test)]
    pub fn with_windows_snapshot(host: Env, snapshot: Snapshot) -> ProcessProbe {
        let probe = ProcessProbe::new(host);
        let _ = probe.snapshot.set(snapshot);
        probe
    }

    /// A probe whose Windows snapshot is taken by `snapshotter`, for testing the real
    /// spawn/join and panic-recovery paths without shelling out to `tasklist.exe`.
    #[cfg(test)]
    pub fn with_windows_snapshotter(
        host: Env,
        snapshotter: fn(&str, &Path) -> Snapshot,
    ) -> ProcessProbe {
        Self::with_windows_snapshotter_and_root(host, snapshotter, PathBuf::from("/mnt/"))
    }

    #[cfg(test)]
    pub fn with_windows_snapshotter_and_root(
        host: Env,
        snapshotter: fn(&str, &Path) -> Snapshot,
        wsl_mount_root: PathBuf,
    ) -> ProcessProbe {
        ProcessProbe {
            wsl_mount_root,
            snapshotter,
            ..ProcessProbe::new(host)
        }
    }

    /// A probe whose open-file listing comes from a test double, so no test walks the real
    /// `/proc`.
    #[cfg(test)]
    pub fn with_open_file_lister(host: Env, lister: fn(&str) -> OpenFiles) -> ProcessProbe {
        ProcessProbe {
            open_file_lister: lister,
            ..ProcessProbe::new(host)
        }
    }

    /// A probe whose WSL `/proc` snapshots and running-distro list come from test doubles.
    #[cfg(test)]
    pub fn with_distro_snapshotter(
        host: Env,
        distro_snapshotter: fn(&str, &str, &Path) -> ProcSnapshot,
        distro_lister: fn(&Path) -> Result<Vec<String>, String>,
    ) -> ProcessProbe {
        ProcessProbe {
            distro_snapshotter,
            distro_lister,
            ..ProcessProbe::new(host)
        }
    }

    /// Takes the snapshot (filtered by `name`) on first use; a panic in the snapshotter is
    /// caught and reported as a warning instead of taking down the whole probe.
    fn windows_snapshot(&self, name: &str) -> &Snapshot {
        self.snapshot.get_or_init(|| {
            let snapshotter = self.snapshotter;
            let name = name.to_string();
            let root = self.wsl_mount_root.clone();
            std::thread::spawn(move || snapshotter(&name, &root))
                .join()
                .unwrap_or_else(|_| Err("tasklist.exe snapshot panicked".into()))
        })
    }

    /// The distros that are running, listed at most once. A distro missing from this list is
    /// never probed, so a stopped one is not started just to answer a liveness question.
    fn running_distros(&self) -> &Result<Vec<String>, String> {
        self.running
            .get_or_init(|| (self.distro_lister)(&self.wsl_mount_root))
    }

    /// `/proc` snapshot for one distro, taken on first use and filtered by `name`.
    fn distro_snapshot(&self, distro: &str, name: &str) -> Arc<ProcSnapshot> {
        let key = distro.to_ascii_lowercase();
        if let Some(hit) = self.distro_snapshots.lock().unwrap().get(&key) {
            return hit.clone();
        }
        let snapshot = Arc::new(match self.running_distros() {
            Err(e) => Err(e.clone()),
            // Not running means nothing in it is running, which is an answer, not a failure.
            Ok(running) if !running.iter().any(|d| d.eq_ignore_ascii_case(distro)) => {
                Ok(HashSet::new())
            }
            Ok(_) => (self.distro_snapshotter)(distro, name, &self.wsl_mount_root),
        });
        self.distro_snapshots
            .lock()
            .unwrap()
            .insert(key, snapshot.clone());
        snapshot
    }

    fn distro_contains(&self, distro: &str, pid: u32, name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        match &*self.distro_snapshot(distro, name) {
            Ok(pids) => pids.contains(&pid),
            Err(_) => false,
        }
    }

    /// Whether a Linux-domain pid from `env` lives in this host's own `/proc`. A host whose
    /// distro name is unknown is assumed to be the one it is asked about.
    fn is_own_proc(&self, env: &Env) -> bool {
        match (&self.host, env) {
            (Env::Wsl { distro: host }, Env::Wsl { distro }) => {
                host == UNKNOWN_DISTRO || host.eq_ignore_ascii_case(distro)
            }
            (Env::Wsl { .. }, _) => true,
            (Env::Linux, Env::Linux) => true,
            _ => false,
        }
    }

    /// Whether the process `pid` in `env`'s process namespace is running. `domain` says which
    /// namespace that is: it comes from the session's own record when it has one, so a Windows
    /// session recorded under a WSL source is still probed on the Windows side.
    pub fn is_running(&self, domain: PidDomain, env: &Env, pid: u32, name: &str) -> bool {
        if pid == 0 {
            return false;
        }
        match (domain, &self.host) {
            (PidDomain::Linux, Env::Linux | Env::Wsl { .. }) if self.is_own_proc(env) => {
                linux_cmdline_contains(pid, name)
            }
            // Another distro, whether ccpick runs on Windows or in a sibling distro.
            (PidDomain::Linux, Env::Wsl { .. } | Env::Windows) => match env {
                Env::Wsl { distro } => self.distro_contains(distro, pid, name),
                _ => false,
            },
            (PidDomain::Windows, Env::Wsl { .. }) => match self.windows_snapshot(name) {
                Ok(map) => map
                    .get(&pid)
                    .is_some_and(|image| contains_ignore_case(image, name)),
                Err(_) => false,
            },
            (PidDomain::Windows, Env::Windows) => windows_image_contains(pid, name),
            _ => false,
        }
    }

    /// Every (pid, open file) pair for this host's own processes whose command line contains
    /// `process_name`. This is agent-neutral: it says nothing about what the files mean — a
    /// caller (e.g. the Codex provider) maps them back to whatever it recognises.
    ///
    /// Linux and WSL only, and only this host's own `/proc` — there is no cross-distro or
    /// cross-machine handle enumeration here, unlike the Windows-snapshot machinery above.
    /// Windows-native has no `/proc`; enumerating its handles is out of scope, so the answer
    /// there is always empty, the way macOS already has no liveness signal for Claude. Taken at
    /// most once per probe, so a caller re-checking many sessions doesn't walk `/proc` per
    /// session.
    pub fn open_files(&self, process_name: &str) -> Vec<(u32, PathBuf)> {
        if !matches!(self.host, Env::Linux | Env::Wsl { .. }) {
            return Vec::new();
        }
        self.open_files
            .get_or_init(|| (self.open_file_lister)(process_name))
            .clone()
    }

    /// Problems hit while probing (e.g. `tasklist.exe` unavailable).
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Some(Err(e)) = self.snapshot.get() {
            warnings.push(e.clone());
        }
        let mut distro_errors: Vec<String> = self
            .distro_snapshots
            .lock()
            .unwrap()
            .values()
            .filter_map(|s| match &**s {
                Err(e) => Some(e.clone()),
                Ok(_) => None,
            })
            .collect();
        distro_errors.sort();
        distro_errors.dedup();
        warnings.extend(distro_errors);
        warnings
    }
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn linux_cmdline_contains(pid: u32, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| bytes.windows(name.len()).any(|w| w == name.as_bytes()))
        .unwrap_or(false)
}

/// This host's own `/proc`: every (pid, open file target) pair for a pid whose command line
/// contains `name`, established empirically against a live `codex` process — it holds its
/// rollout file open for as long as it runs, and `/proc/<pid>/fd/*` symlinks resolve to it.
/// A pid or fd that can't be read (permission denied, exited mid-walk) is skipped rather than
/// failing the whole scan: another user's processes are not ours to inspect, and that is not an
/// error, just nothing to report for that pid.
#[cfg(target_os = "linux")]
fn linux_open_files(name: &str) -> Vec<(u32, PathBuf)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if !linux_cmdline_contains(pid, name) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(target) = std::fs::read_link(fd.path()) {
                found.push((pid, target));
            }
        }
    }
    found
}

#[cfg(not(target_os = "linux"))]
fn linux_open_files(_name: &str) -> Vec<(u32, PathBuf)> {
    Vec::new()
}

#[cfg(windows)]
fn windows_image_contains(pid: u32, name: &str) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    // SAFETY: plain Win32 calls; the handle is closed before returning and the buffer length is
    // passed alongside the buffer.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
        CloseHandle(handle);
        ok != 0 && contains_ignore_case(&String::from_utf16_lossy(&buf[..len as usize]), name)
    }
}

#[cfg(not(windows))]
fn windows_image_contains(_pid: u32, _name: &str) -> bool {
    false
}

/// Lists the pids in one distro whose command line contains `$1`. `/proc/<pid>/cmdline` is
/// NUL-separated, so grep needs `-a`; `exit 0` keeps grep's "no match" from looking like a
/// failure to run `wsl.exe` at all.
const WSL_PROC_SCRIPT: &str = "grep -lsa -- \"$1\" /proc/[0-9]*/cmdline 2>/dev/null; exit 0";

/// Pids of processes inside `distro` whose command line contains `name`, via one `wsl.exe` call.
/// Only ever called for a distro already known to be running.
fn take_wsl_proc_snapshot(distro: &str, name: &str, wsl_mount_root: &Path) -> ProcSnapshot {
    let args = [
        "-d",
        distro,
        "--exec",
        "sh",
        "-c",
        WSL_PROC_SCRIPT,
        "sh",
        name,
    ];
    let mut spawn_error = String::new();
    for candidate in crate::env::windows_tool_candidates("wsl.exe", wsl_mount_root) {
        match std::process::Command::new(&candidate).args(args).output() {
            Ok(output) if output.status.success() => {
                return Ok(parse_proc_cmdline_paths(&String::from_utf8_lossy(
                    &output.stdout,
                )));
            }
            Ok(output) => {
                return Err(format!(
                    "could not read session status in WSL distro {distro} ({})",
                    output.status
                ));
            }
            Err(e) => spawn_error = format!("could not run {}: {e}", candidate.display()),
        }
    }
    Err(format!(
        "could not run wsl.exe for {distro} session status: {spawn_error}"
    ))
}

/// Pids from `grep -l` output lines such as `/proc/1234/cmdline`.
pub fn parse_proc_cmdline_paths(output: &str) -> HashSet<u32> {
    output
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("/proc/")?
                .split('/')
                .next()?
                .parse()
                .ok()
        })
        .collect()
}

/// Running WSL distros, from a host that may be Windows or another distro.
fn take_running_distros(wsl_mount_root: &Path) -> Result<Vec<String>, String> {
    let mut spawn_error = String::new();
    for candidate in crate::env::windows_tool_candidates("wsl.exe", wsl_mount_root) {
        match std::process::Command::new(&candidate)
            .args(["-l", "--running", "-q"])
            .output()
        {
            // A non-zero exit means "no running distributions", not a failure.
            Ok(output) if !output.status.success() => return Ok(Vec::new()),
            Ok(output) => return Ok(crate::homes::parse_wsl_list(&output.stdout)),
            Err(e) => spawn_error = format!("could not run {}: {e}", candidate.display()),
        }
    }
    Err(format!(
        "could not run wsl.exe to list running distros: {spawn_error}"
    ))
}

/// `tasklist.exe` arguments to list only processes whose image name starts with `name`
/// (e.g. `claude` → `claude.exe`), so the snapshot doesn't enumerate every process.
fn tasklist_args(name: &str) -> Vec<String> {
    vec![
        "/FO".into(),
        "CSV".into(),
        "/NH".into(),
        "/FI".into(),
        format!("IMAGENAME eq {name}*"),
    ]
}

/// Runs `tasklist.exe`, trying the plain name first and falling back to its fixed path under
/// the WSL mount's `Windows\System32` if that spawn fails (e.g. `appendWindowsPath = false`).
fn take_tasklist_snapshot(name: &str, wsl_mount_root: &Path) -> Snapshot {
    let args = tasklist_args(name);
    let mut spawn_error = String::new();
    for candidate in crate::env::windows_tool_candidates("tasklist.exe", wsl_mount_root) {
        match std::process::Command::new(&candidate).args(&args).output() {
            Ok(output) if output.status.success() => {
                return Ok(parse_tasklist_csv(&String::from_utf8_lossy(&output.stdout)));
            }
            Ok(output) => return Err(format!("tasklist.exe failed ({})", output.status)),
            Err(e) => spawn_error = format!("could not run {}: {e}", candidate.display()),
        }
    }
    Err(format!(
        "could not run tasklist.exe for Windows session status: {spawn_error}"
    ))
}

/// pid → image name from `tasklist /FO CSV /NH` output.
pub fn parse_tasklist_csv(output: &str) -> HashMap<u32, String> {
    output
        .lines()
        .filter_map(|line| {
            let fields = split_csv_line(line);
            let pid = fields.get(1)?.trim().parse().ok()?;
            Some((pid, fields.first()?.clone()))
        })
        .collect()
}

fn split_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = line.trim_end_matches(['\r', '\n']).chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => fields.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    fields.push(field);
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ubuntu() -> Env {
        Env::Wsl {
            distro: "Ubuntu".into(),
        }
    }

    #[test]
    fn parses_pid_domains() {
        assert_eq!(
            PidDomain::parse("linux:abc:pid:[1]"),
            Some(PidDomain::Linux)
        );
        assert_eq!(PidDomain::parse("win32:host"), Some(PidDomain::Windows));
        assert_eq!(PidDomain::parse("darwin:x"), None);
        assert_eq!(PidDomain::of(&ubuntu()), Some(PidDomain::Linux));
        assert_eq!(PidDomain::of(&Env::Windows), Some(PidDomain::Windows));
        assert_eq!(PidDomain::of(&Env::MacOs), None);
    }

    #[test]
    fn tasklist_args_filter_by_image_name() {
        assert_eq!(
            tasklist_args("claude"),
            vec!["/FO", "CSV", "/NH", "/FI", "IMAGENAME eq claude*"]
        );
    }

    #[test]
    fn parses_tasklist_csv() {
        let out = "\"System Idle Process\",\"0\",\"Services\",\"0\",\"8 K\"\r\n\
                   \"claude.exe\",\"58892\",\"Console\",\"1\",\"311,464 K\"\r\n\
                   \"odd \"\"name\"\".exe\",\"77\",\"Console\",\"1\",\"1 K\"\r\n\
                   garbage line\r\n";
        let map = parse_tasklist_csv(out);
        assert_eq!(map.get(&58892).map(String::as_str), Some("claude.exe"));
        assert_eq!(map.get(&77).map(String::as_str), Some("odd \"name\".exe"));
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn windows_sessions_seen_from_wsl_use_the_snapshot() {
        let probe = ProcessProbe::with_windows_snapshot(
            ubuntu(),
            Ok(HashMap::from([
                (58892, "claude.exe".to_string()),
                (5, "notepad.exe".to_string()),
            ])),
        );
        assert!(probe.is_running(PidDomain::Windows, &Env::Windows, 58892, "claude"));
        assert!(!probe.is_running(PidDomain::Windows, &Env::Windows, 5, "claude"));
        assert!(!probe.is_running(PidDomain::Windows, &Env::Windows, 6, "claude"));
        assert!(probe.warnings().is_empty());
    }

    #[test]
    fn failed_snapshot_means_not_running_with_one_warning() {
        let probe = ProcessProbe::with_windows_snapshot(ubuntu(), Err("no interop".into()));
        assert!(!probe.is_running(PidDomain::Windows, &Env::Windows, 58892, "claude"));
        assert_eq!(probe.warnings(), vec!["no interop".to_string()]);
    }

    #[test]
    fn unreachable_domains_are_not_running() {
        let probe = ProcessProbe::new(Env::Linux);
        assert!(!probe.is_running(PidDomain::Windows, &Env::Windows, 1, "claude"));
        assert!(!probe.is_running(PidDomain::Linux, &Env::Linux, 0, "claude"));
        let mac = ProcessProbe::new(Env::MacOs);
        assert!(!mac.is_running(PidDomain::Linux, &Env::MacOs, 1, "x"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_cmdline_check_sees_this_test_binary() {
        let probe = ProcessProbe::new(Env::Linux);
        assert!(probe.is_running(PidDomain::Linux, &Env::Linux, std::process::id(), "ccpick"));
        assert!(!probe.is_running(
            PidDomain::Linux,
            &Env::Linux,
            std::process::id(),
            "definitely-not-here"
        ));
        // An empty name must not panic `bytes.windows(0)`-style; it just never matches.
        assert!(!probe.is_running(PidDomain::Linux, &Env::Linux, std::process::id(), ""));
    }

    static LAZY_CALLS: AtomicUsize = AtomicUsize::new(0);
    static LAZY_LAST_NAME: Mutex<String> = Mutex::new(String::new());
    fn lazy_snapshotter(name: &str, _root: &Path) -> Snapshot {
        LAZY_CALLS.fetch_add(1, Ordering::SeqCst);
        *LAZY_LAST_NAME.lock().unwrap() = name.to_string();
        Ok(HashMap::from([(58892, "claude.exe".to_string())]))
    }

    static ROOT_SEEN: Mutex<String> = Mutex::new(String::new());
    fn root_capturing_snapshotter(_name: &str, root: &Path) -> Snapshot {
        *ROOT_SEEN.lock().unwrap() = root.display().to_string();
        Ok(HashMap::new())
    }

    #[test]
    fn probe_threads_the_hosts_wsl_mount_root_to_the_snapshotter() {
        let probe = ProcessProbe::with_windows_snapshotter_and_root(
            ubuntu(),
            root_capturing_snapshotter,
            PathBuf::from("/custom-root/"),
        );
        probe.is_running(PidDomain::Windows, &Env::Windows, 1, "claude");
        assert_eq!(*ROOT_SEEN.lock().unwrap(), "/custom-root/");
    }

    #[test]
    fn lazy_snapshot_is_taken_once_and_filtered_by_the_probed_name() {
        let probe = ProcessProbe::with_windows_snapshotter(ubuntu(), lazy_snapshotter);
        // No snapshot is taken until a Windows-domain pid is actually probed.
        assert_eq!(LAZY_CALLS.load(Ordering::SeqCst), 0);
        assert!(probe.is_running(PidDomain::Windows, &Env::Windows, 58892, "claude"));
        assert!(!probe.is_running(PidDomain::Windows, &Env::Windows, 1, "claude"));
        assert!(probe.is_running(PidDomain::Windows, &Env::Windows, 58892, "claude"));
        assert_eq!(LAZY_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(*LAZY_LAST_NAME.lock().unwrap(), "claude");
    }

    fn ubuntu_env() -> Env {
        Env::Wsl {
            distro: "Ubuntu".into(),
        }
    }

    /// One recording `/proc`-snapshot double per test: they run in parallel, so a shared
    /// counter would race.
    macro_rules! proc_snapshot_double {
        ($calls:ident, $distros:ident, $f:ident) => {
            static $calls: AtomicUsize = AtomicUsize::new(0);
            static $distros: Mutex<Vec<String>> = Mutex::new(Vec::new());
            fn $f(distro: &str, _name: &str, _root: &Path) -> ProcSnapshot {
                $calls.fetch_add(1, Ordering::SeqCst);
                $distros.lock().unwrap().push(distro.to_string());
                Ok(HashSet::from([4242]))
            }
        };
    }

    fn running_ubuntu(_root: &Path) -> Result<Vec<String>, String> {
        Ok(vec!["Ubuntu".to_string()])
    }
    fn failing_lister(_root: &Path) -> Result<Vec<String>, String> {
        Err("could not run wsl.exe to list running distros: nope".into())
    }
    fn failing_distro_snapshotter(distro: &str, _name: &str, _root: &Path) -> ProcSnapshot {
        Err(format!(
            "could not read session status in WSL distro {distro}"
        ))
    }

    proc_snapshot_double!(
        FROM_WINDOWS_CALLS,
        FROM_WINDOWS_DISTROS,
        from_windows_snapshotter
    );

    #[test]
    fn wsl_sessions_seen_from_windows_use_the_distros_proc_snapshot() {
        let probe = ProcessProbe::with_distro_snapshotter(
            Env::Windows,
            from_windows_snapshotter,
            running_ubuntu,
        );
        assert!(probe.is_running(PidDomain::Linux, &ubuntu_env(), 4242, "claude"));
        assert!(!probe.is_running(PidDomain::Linux, &ubuntu_env(), 7, "claude"));
        // Taken once for the distro, however many pids are probed.
        assert_eq!(FROM_WINDOWS_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(
            *FROM_WINDOWS_DISTROS.lock().unwrap(),
            vec!["Ubuntu".to_string()]
        );
        assert!(probe.warnings().is_empty());
    }

    proc_snapshot_double!(STOPPED_CALLS, STOPPED_DISTROS, stopped_snapshotter);

    #[test]
    fn a_stopped_distro_is_never_probed_and_has_nothing_running() {
        let probe = ProcessProbe::with_distro_snapshotter(
            Env::Windows,
            stopped_snapshotter,
            running_ubuntu,
        );
        let stopped = Env::Wsl {
            distro: "Debian".into(),
        };
        assert!(!probe.is_running(PidDomain::Linux, &stopped, 4242, "claude"));
        assert_eq!(STOPPED_CALLS.load(Ordering::SeqCst), 0);
        assert!(STOPPED_DISTROS.lock().unwrap().is_empty());
        assert!(probe.warnings().is_empty());
    }

    proc_snapshot_double!(SIBLING_CALLS, SIBLING_DISTROS, sibling_snapshotter);

    #[test]
    fn a_sibling_distro_is_probed_rather_than_this_hosts_own_proc() {
        let probe = ProcessProbe::with_distro_snapshotter(ubuntu(), sibling_snapshotter, |_root| {
            Ok(vec!["Ubuntu".into(), "Debian".into()])
        });
        let sibling = Env::Wsl {
            distro: "Debian".into(),
        };
        assert!(probe.is_running(PidDomain::Linux, &sibling, 4242, "claude"));
        assert_eq!(*SIBLING_DISTROS.lock().unwrap(), vec!["Debian".to_string()]);
        // The host's own distro (any case) still goes straight to /proc, with no wsl.exe call.
        let same_case = Env::Wsl {
            distro: "ubuntu".into(),
        };
        assert!(!probe.is_running(PidDomain::Linux, &same_case, 4242, "no-such-process-name"));
        assert_eq!(SIBLING_CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_distro_probe_that_fails_warns_once_and_reports_not_running() {
        let probe = ProcessProbe::with_distro_snapshotter(
            Env::Windows,
            failing_distro_snapshotter,
            running_ubuntu,
        );
        assert!(!probe.is_running(PidDomain::Linux, &ubuntu_env(), 4242, "claude"));
        assert!(!probe.is_running(PidDomain::Linux, &ubuntu_env(), 7, "claude"));
        assert_eq!(
            probe.warnings(),
            vec!["could not read session status in WSL distro Ubuntu".to_string()]
        );
    }

    proc_snapshot_double!(NO_LIST_CALLS, NO_LIST_DISTROS, no_list_snapshotter);

    #[test]
    fn a_failing_distro_list_warns_rather_than_starting_anything() {
        let probe = ProcessProbe::with_distro_snapshotter(
            Env::Windows,
            no_list_snapshotter,
            failing_lister,
        );
        assert!(!probe.is_running(PidDomain::Linux, &ubuntu_env(), 4242, "claude"));
        assert_eq!(NO_LIST_CALLS.load(Ordering::SeqCst), 0);
        assert!(NO_LIST_DISTROS.lock().unwrap().is_empty());
        assert_eq!(probe.warnings().len(), 1);
    }

    #[test]
    fn windows_sessions_recorded_under_a_wsl_source_still_use_the_windows_snapshot() {
        let probe = ProcessProbe::with_windows_snapshot(
            ubuntu(),
            Ok(HashMap::from([(58892, "claude.exe".to_string())])),
        );
        assert!(probe.is_running(PidDomain::Windows, &ubuntu_env(), 58892, "claude"));
    }

    #[test]
    fn parses_pids_from_grep_output() {
        let out = "/proc/1234/cmdline\n/proc/7/cmdline\n\ngrep: /proc/9/cmdline: No such file\n";
        let pids = parse_proc_cmdline_paths(out);
        assert_eq!(pids, HashSet::from([1234, 7]));
        assert!(parse_proc_cmdline_paths("").is_empty());
    }

    proc_snapshot_double!(UNKNOWN_CALLS, UNKNOWN_DISTROS, unknown_host_snapshotter);

    #[test]
    fn a_wsl_host_with_an_unknown_distro_name_uses_its_own_proc() {
        let probe = ProcessProbe::with_distro_snapshotter(
            Env::Wsl {
                distro: UNKNOWN_DISTRO.into(),
            },
            unknown_host_snapshotter,
            running_ubuntu,
        );
        assert!(!probe.is_running(
            PidDomain::Linux,
            &ubuntu_env(),
            4242,
            "no-such-process-name"
        ));
        assert_eq!(UNKNOWN_CALLS.load(Ordering::SeqCst), 0);
        assert!(UNKNOWN_DISTROS.lock().unwrap().is_empty());
    }

    static PANIC_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn panicking_snapshotter(_name: &str, _root: &Path) -> Snapshot {
        PANIC_CALLS.fetch_add(1, Ordering::SeqCst);
        panic!("simulated tasklist.exe panic");
    }

    #[test]
    fn open_files_pairs_each_pid_with_what_it_has_open() {
        let probe = ProcessProbe::with_open_file_lister(Env::Linux, |_name| {
            vec![
                (
                    42,
                    PathBuf::from("/home/me/.codex/sessions/2026/09/18/rollout-a.jsonl"),
                ),
                (42, PathBuf::from("/home/me/.codex/state_5.sqlite")),
                (
                    7,
                    PathBuf::from("/home/me/.codex/sessions/2026/09/18/rollout-b.jsonl"),
                ),
            ]
        });
        let open = probe.open_files("codex");
        assert_eq!(open.len(), 3);
        assert!(
            open.iter()
                .any(|(pid, p)| *pid == 7 && p.ends_with("rollout-b.jsonl"))
        );
    }

    #[test]
    fn a_host_with_no_proc_filesystem_reports_nothing() {
        let probe = ProcessProbe::new(Env::Windows);
        assert!(
            probe.open_files("codex").is_empty(),
            "no handle enumeration on Windows yet"
        );
    }

    #[test]
    fn panicking_snapshot_is_reported_as_a_warning() {
        let probe = ProcessProbe::with_windows_snapshotter(ubuntu(), panicking_snapshotter);
        assert!(!probe.is_running(PidDomain::Windows, &Env::Windows, 58892, "claude"));
        assert_eq!(
            probe.warnings(),
            vec!["tasklist.exe snapshot panicked".to_string()]
        );
        assert_eq!(PANIC_CALLS.load(Ordering::SeqCst), 1);
    }
}
