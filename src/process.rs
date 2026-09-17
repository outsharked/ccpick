//! "Is this process running?" for pids in Linux or Windows process namespaces. Agent-neutral.
use crate::env::Env;
use std::collections::HashMap;
use std::sync::OnceLock;

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

/// Answers liveness questions for one ccpick run. The Windows snapshot (WSL hosts) is taken at
/// most once, lazily, the first time a Windows-domain pid is actually probed — filtered by the
/// image name that call supplies, so `tasklist.exe` doesn't have to enumerate every process.
pub struct ProcessProbe {
    host: Env,
    snapshot: OnceLock<Snapshot>,
    /// How to take the Windows snapshot, filtered by image name; swappable in tests.
    snapshotter: fn(&str) -> Snapshot,
}

impl ProcessProbe {
    pub fn new(host: Env) -> ProcessProbe {
        ProcessProbe {
            host,
            snapshot: OnceLock::new(),
            snapshotter: take_tasklist_snapshot,
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
    pub fn with_windows_snapshotter(host: Env, snapshotter: fn(&str) -> Snapshot) -> ProcessProbe {
        ProcessProbe {
            host,
            snapshot: OnceLock::new(),
            snapshotter,
        }
    }

    /// Takes the snapshot (filtered by `name`) on first use; a panic in the snapshotter is
    /// caught and reported as a warning instead of taking down the whole probe.
    fn windows_snapshot(&self, name: &str) -> &Snapshot {
        self.snapshot.get_or_init(|| {
            let snapshotter = self.snapshotter;
            let name = name.to_string();
            std::thread::spawn(move || snapshotter(&name))
                .join()
                .unwrap_or_else(|_| Err("tasklist.exe snapshot panicked".into()))
        })
    }

    pub fn is_running(&self, domain: PidDomain, pid: u32, name: &str) -> bool {
        if pid == 0 {
            return false;
        }
        match (domain, &self.host) {
            (PidDomain::Linux, Env::Linux | Env::Wsl { .. }) => linux_cmdline_contains(pid, name),
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

    /// Problems hit while probing (e.g. `tasklist.exe` unavailable).
    pub fn warnings(&self) -> Vec<String> {
        match self.snapshot.get() {
            Some(Err(e)) => vec![e.clone()],
            _ => Vec::new(),
        }
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

fn take_tasklist_snapshot(name: &str) -> Snapshot {
    let output = std::process::Command::new("tasklist.exe")
        .args(tasklist_args(name))
        .output()
        .map_err(|e| format!("could not run tasklist.exe for Windows session status: {e}"))?;
    if !output.status.success() {
        return Err(format!("tasklist.exe failed ({})", output.status));
    }
    Ok(parse_tasklist_csv(&String::from_utf8_lossy(&output.stdout)))
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
        assert!(probe.is_running(PidDomain::Windows, 58892, "claude"));
        assert!(!probe.is_running(PidDomain::Windows, 5, "claude"));
        assert!(!probe.is_running(PidDomain::Windows, 6, "claude"));
        assert!(probe.warnings().is_empty());
    }

    #[test]
    fn failed_snapshot_means_not_running_with_one_warning() {
        let probe = ProcessProbe::with_windows_snapshot(ubuntu(), Err("no interop".into()));
        assert!(!probe.is_running(PidDomain::Windows, 58892, "claude"));
        assert_eq!(probe.warnings(), vec!["no interop".to_string()]);
    }

    #[test]
    fn unreachable_domains_are_not_running() {
        let probe = ProcessProbe::new(Env::Linux);
        assert!(!probe.is_running(PidDomain::Windows, 1, "claude"));
        assert!(!probe.is_running(PidDomain::Linux, 0, "claude"));
        let mac = ProcessProbe::new(Env::MacOs);
        assert!(!mac.is_running(PidDomain::Linux, 1, "x"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_cmdline_check_sees_this_test_binary() {
        let probe = ProcessProbe::new(Env::Linux);
        assert!(probe.is_running(PidDomain::Linux, std::process::id(), "ccpick"));
        assert!(!probe.is_running(PidDomain::Linux, std::process::id(), "definitely-not-here"));
        // An empty name must not panic `bytes.windows(0)`-style; it just never matches.
        assert!(!probe.is_running(PidDomain::Linux, std::process::id(), ""));
    }

    static LAZY_CALLS: AtomicUsize = AtomicUsize::new(0);
    static LAZY_LAST_NAME: Mutex<String> = Mutex::new(String::new());
    fn lazy_snapshotter(name: &str) -> Snapshot {
        LAZY_CALLS.fetch_add(1, Ordering::SeqCst);
        *LAZY_LAST_NAME.lock().unwrap() = name.to_string();
        Ok(HashMap::from([(58892, "claude.exe".to_string())]))
    }

    #[test]
    fn lazy_snapshot_is_taken_once_and_filtered_by_the_probed_name() {
        let probe = ProcessProbe::with_windows_snapshotter(ubuntu(), lazy_snapshotter);
        // No snapshot is taken until a Windows-domain pid is actually probed.
        assert_eq!(LAZY_CALLS.load(Ordering::SeqCst), 0);
        assert!(probe.is_running(PidDomain::Windows, 58892, "claude"));
        assert!(!probe.is_running(PidDomain::Windows, 1, "claude"));
        assert!(probe.is_running(PidDomain::Windows, 58892, "claude"));
        assert_eq!(LAZY_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(*LAZY_LAST_NAME.lock().unwrap(), "claude");
    }

    static PANIC_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn panicking_snapshotter(_name: &str) -> Snapshot {
        PANIC_CALLS.fetch_add(1, Ordering::SeqCst);
        panic!("simulated tasklist.exe panic");
    }

    #[test]
    fn panicking_snapshot_is_reported_as_a_warning() {
        let probe = ProcessProbe::with_windows_snapshotter(ubuntu(), panicking_snapshotter);
        assert!(!probe.is_running(PidDomain::Windows, 58892, "claude"));
        assert_eq!(
            probe.warnings(),
            vec!["tasklist.exe snapshot panicked".to_string()]
        );
        assert_eq!(PANIC_CALLS.load(Ordering::SeqCst), 1);
    }
}
