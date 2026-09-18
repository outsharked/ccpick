//! Execution environments (native Linux, WSL, Windows, macOS) and path translation between a
//! WSL distro and its Windows host. Agent-neutral.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Stand-in distro name for a WSL host whose `WSL_DISTRO_NAME` isn't set.
pub const UNKNOWN_DISTRO: &str = "wsl";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Env {
    /// Native Linux, not WSL.
    Linux,
    Wsl {
        distro: String,
    },
    Windows,
    MacOs,
}

impl Env {
    /// Parses `linux`, `windows`, `macos` (any case) or `wsl:<distro>`.
    pub fn parse(s: &str) -> Option<Env> {
        let t = s.trim();
        match t.to_ascii_lowercase().as_str() {
            "linux" => Some(Env::Linux),
            "windows" => Some(Env::Windows),
            "macos" => Some(Env::MacOs),
            _ if t.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("wsl:")) => {
                let distro = &t[4..];
                if distro.is_empty() {
                    None
                } else {
                    Some(Env::Wsl {
                        distro: distro.to_string(),
                    })
                }
            }
            _ => None,
        }
    }

    /// Stable identifier, the inverse of `parse`.
    pub fn id(&self) -> String {
        match self {
            Env::Linux => "linux".into(),
            Env::Wsl { distro } => format!("wsl:{distro}"),
            Env::Windows => "windows".into(),
            Env::MacOs => "macos".into(),
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            Env::Linux => "Linux".into(),
            Env::Wsl { distro } => format!("WSL ({distro})"),
            Env::Windows => "Windows".into(),
            Env::MacOs => "macOS".into(),
        }
    }

    /// The shell a resume command for this environment is written for.
    pub fn shell_name(&self) -> &'static str {
        match self {
            Env::Windows => "PowerShell",
            Env::Wsl { .. } => "a WSL shell",
            Env::Linux | Env::MacOs => "a shell",
        }
    }

    pub fn is_windows(&self) -> bool {
        matches!(self, Env::Windows)
    }
}

/// Where ccpick itself is running, plus what's needed to translate the other side's paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostContext {
    pub env: Env,
    /// WSL drive mount root, e.g. `/mnt/` (always ends with `/`). Only meaningful in WSL.
    pub wsl_mount_root: PathBuf,
}

impl Default for HostContext {
    fn default() -> Self {
        HostContext {
            env: Env::Linux,
            wsl_mount_root: PathBuf::from("/mnt/"),
        }
    }
}

/// Pure host detection from compile-time platform flags, environment variables and whether the
/// WSL interop binfmt entry exists.
pub fn detect_env(
    is_windows: bool,
    is_macos: bool,
    env_vars: &HashMap<String, String>,
    wsl_interop: bool,
) -> Env {
    if is_windows {
        return Env::Windows;
    }
    if is_macos {
        return Env::MacOs;
    }
    match env_vars.get("WSL_DISTRO_NAME").filter(|d| !d.is_empty()) {
        Some(distro) => Env::Wsl {
            distro: distro.clone(),
        },
        None if wsl_interop => Env::Wsl {
            distro: UNKNOWN_DISTRO.into(),
        },
        None => Env::Linux,
    }
}

/// `[automount] root` from `/etc/wsl.conf`, defaulting to `/mnt/`. Always ends with `/`.
pub fn parse_automount_root(wsl_conf: &str) -> PathBuf {
    let mut in_automount = false;
    for raw in wsl_conf.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_automount = line.eq_ignore_ascii_case("[automount]");
            continue;
        }
        if !in_automount {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("root")
        {
            let mut root = value.trim().trim_matches('"').to_string();
            if root.is_empty() {
                continue;
            }
            if !root.ends_with('/') {
                root.push('/');
            }
            return PathBuf::from(root);
        }
    }
    PathBuf::from("/mnt/")
}

impl HostContext {
    /// Detects the real host.
    pub fn detect(env_vars: &HashMap<String, String>) -> HostContext {
        let env = detect_env(
            cfg!(windows),
            cfg!(target_os = "macos"),
            env_vars,
            Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists(),
        );
        let wsl_mount_root = match env {
            Env::Wsl { .. } => std::fs::read_to_string("/etc/wsl.conf")
                .map(|text| parse_automount_root(&text))
                .unwrap_or_else(|_| PathBuf::from("/mnt/")),
            _ => PathBuf::from("/mnt/"),
        };
        HostContext {
            env,
            wsl_mount_root,
        }
    }

    /// A path as `from` sees it → a path ccpick can read. None when there's no mapping.
    pub fn to_host(&self, path: &Path, from: &Env) -> Option<PathBuf> {
        if *from == self.env {
            return Some(path.to_path_buf());
        }
        let s = path.to_string_lossy();
        match (&self.env, from) {
            (Env::Wsl { .. }, Env::Windows) => windows_to_wsl(&s, &self.wsl_mount_root),
            (Env::Windows, Env::Wsl { distro }) => linux_to_unc(&s, distro),
            _ => None,
        }
    }

    /// A path ccpick can read → how `to` sees it. Unchanged when there's no mapping.
    pub fn to_env(&self, host_path: &Path, to: &Env) -> PathBuf {
        if *to == self.env {
            return host_path.to_path_buf();
        }
        let s = host_path.to_string_lossy();
        let translated = match (&self.env, to) {
            (Env::Wsl { .. }, Env::Windows) => wsl_to_windows(&s, &self.wsl_mount_root),
            (Env::Windows, Env::Wsl { .. }) => unc_to_linux(&s).map(|(_, linux)| linux),
            _ => None,
        };
        translated
            .map(PathBuf::from)
            .unwrap_or_else(|| host_path.to_path_buf())
    }

    /// The environment a configured path belongs to, judged by its form.
    pub fn infer_env(&self, path: &Path) -> Env {
        let s = path.to_string_lossy();
        match &self.env {
            Env::Wsl { .. } if wsl_to_windows(&s, &self.wsl_mount_root).is_some() => Env::Windows,
            Env::Windows => match unc_to_linux(&s) {
                Some((distro, _)) => Env::Wsl { distro },
                None => Env::Windows,
            },
            other => other.clone(),
        }
    }
}

/// `C:\Users\me` (or `C:/Users/me`) → `<root>c/Users/me`.
pub fn windows_to_wsl(path: &str, root: &Path) -> Option<PathBuf> {
    let bytes = path.as_bytes();
    if bytes.len() < 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return None;
    }
    let drive = (bytes[0] as char).to_ascii_lowercase();
    let rest = path[2..]
        .trim_start_matches(['\\', '/'])
        .trim_end_matches(['\\', '/'])
        .replace('\\', "/");
    let mut out = format!("{}{drive}", root.to_string_lossy());
    if !rest.is_empty() {
        out.push('/');
        out.push_str(&rest);
    }
    Some(PathBuf::from(out))
}

/// `<root>c/Users/me` → `C:\Users\me`. None when not a drive under the mount root.
pub fn wsl_to_windows(path: &str, root: &Path) -> Option<String> {
    let root = root.to_string_lossy();
    let rest = path.strip_prefix(root.as_ref())?;
    let (drive, tail) = rest.split_once('/').unwrap_or((rest, ""));
    if drive.len() != 1 || !drive.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }
    let mut out = format!("{}:\\", drive.to_ascii_uppercase());
    out.push_str(&tail.trim_end_matches('/').replace('/', "\\"));
    Some(out)
}

/// `/home/me` → `\\wsl.localhost\<distro>\home\me`. None for relative paths.
pub fn linux_to_unc(path: &str, distro: &str) -> Option<PathBuf> {
    let rest = path.strip_prefix('/')?.trim_end_matches('/');
    let mut out = format!(r"\\wsl.localhost\{distro}");
    if !rest.is_empty() {
        out.push('\\');
        out.push_str(&rest.replace('/', "\\"));
    }
    Some(PathBuf::from(out))
}

/// `\\wsl.localhost\<distro>\home\me` or `\\wsl$\<distro>\home\me` (either slash style,
/// any case), or the verbatim UNC form `\\?\UNC\wsl.localhost\...` / `\\?\UNC\wsl$\...` that
/// `dunce::canonicalize` leaves on UNC paths → (distro, `/home/me`).
pub fn unc_to_linux(path: &str) -> Option<(String, String)> {
    let mut norm = path.replace('/', "\\");
    if let Some(prefix) = norm.get(..8)
        && prefix.eq_ignore_ascii_case(r"\\?\UNC\")
    {
        norm = format!(r"\\{}", &norm[8..]);
    }
    let lower = norm.to_ascii_lowercase();
    let prefix_len = if lower.starts_with(r"\\wsl.localhost\") {
        r"\\wsl.localhost\".len()
    } else if lower.starts_with(r"\\wsl$\") {
        r"\\wsl$\".len()
    } else {
        return None;
    };
    let rest = &norm[prefix_len..];
    let (distro, tail) = rest.split_once('\\').unwrap_or((rest, ""));
    if distro.is_empty() {
        return None;
    }
    Some((
        distro.to_string(),
        format!("/{}", tail.trim_end_matches('\\').replace('\\', "/")),
    ))
}

/// Where to find a Windows tool (`tasklist.exe`, `clip.exe`) ccpick shells out to from a WSL
/// host, in order: first the plain name (works via `PATH` when Windows interop appends it, the
/// default), then its fixed location under the WSL mount's `Windows\System32`, for a host with
/// `[interop] appendWindowsPath = false` in `wsl.conf`.
pub fn windows_tool_candidates(name: &str, wsl_mount_root: &Path) -> Vec<PathBuf> {
    // Built with an explicit `/` rather than `Path::join` (whose separator depends on the
    // *compiling* target) because `wsl_mount_root` is always a WSL-side (forward-slash) path,
    // regardless of what platform this binary itself was built for.
    let root = wsl_mount_root.to_string_lossy();
    let root = root.trim_end_matches('/');
    vec![
        PathBuf::from(name),
        PathBuf::from(format!("{root}/c/Windows/System32/{name}")),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
    fn wsl_host() -> HostContext {
        HostContext {
            env: Env::Wsl {
                distro: "Ubuntu".into(),
            },
            wsl_mount_root: PathBuf::from("/mnt/"),
        }
    }
    fn windows_host() -> HostContext {
        HostContext {
            env: Env::Windows,
            wsl_mount_root: PathBuf::from("/mnt/"),
        }
    }

    #[test]
    fn env_parse_and_ids_round_trip() {
        for e in [
            Env::Linux,
            Env::Windows,
            Env::MacOs,
            Env::Wsl {
                distro: "Ubuntu-24.04".into(),
            },
        ] {
            assert_eq!(Env::parse(&e.id()), Some(e.clone()));
        }
        assert_eq!(Env::parse("WINDOWS"), Some(Env::Windows));
        assert_eq!(Env::parse("wsl:"), None);
        assert_eq!(Env::parse("dos"), None);
        assert_eq!(
            Env::Wsl {
                distro: "Ubuntu".into()
            }
            .display_name(),
            "WSL (Ubuntu)"
        );
        assert_eq!(Env::Windows.shell_name(), "PowerShell");
    }

    #[test]
    fn parse_does_not_panic_on_non_ascii() {
        assert_eq!(Env::parse("wslé"), None);
        assert_eq!(
            Env::parse("wsl:é"),
            Some(Env::Wsl {
                distro: "é".into()
            })
        );
        assert_eq!(Env::parse("éééé"), None);
    }

    #[test]
    fn detects_host_environment() {
        assert_eq!(detect_env(true, false, &vars(&[]), false), Env::Windows);
        assert_eq!(detect_env(false, true, &vars(&[]), false), Env::MacOs);
        assert_eq!(
            detect_env(false, false, &vars(&[("WSL_DISTRO_NAME", "Ubuntu")]), false),
            Env::Wsl {
                distro: "Ubuntu".into()
            }
        );
        assert_eq!(
            detect_env(false, false, &vars(&[]), true),
            Env::Wsl {
                distro: "wsl".into()
            }
        );
        assert_eq!(detect_env(false, false, &vars(&[]), false), Env::Linux);
    }

    #[test]
    fn parses_automount_root() {
        assert_eq!(parse_automount_root(""), PathBuf::from("/mnt/"));
        assert_eq!(
            parse_automount_root("[boot]\nsystemd=true\n[automount]\nroot = /win\n"),
            PathBuf::from("/win/")
        );
        assert_eq!(
            parse_automount_root("[automount]\nroot=\"/drives/\" # comment\n"),
            PathBuf::from("/drives/")
        );
        assert_eq!(
            parse_automount_root("[network]\nroot=/nope\n"),
            PathBuf::from("/mnt/")
        );
    }

    #[test]
    fn translates_windows_and_wsl_mount_paths() {
        let root = Path::new("/mnt/");
        assert_eq!(
            windows_to_wsl(r"C:\Users\me\proj", root),
            Some(PathBuf::from("/mnt/c/Users/me/proj"))
        );
        assert_eq!(
            windows_to_wsl("d:/data/", root),
            Some(PathBuf::from("/mnt/d/data"))
        );
        assert_eq!(windows_to_wsl("C:", root), Some(PathBuf::from("/mnt/c")));
        assert_eq!(windows_to_wsl("/home/me", root), None);
        assert_eq!(
            wsl_to_windows("/mnt/c/Users/me/proj", root).as_deref(),
            Some(r"C:\Users\me\proj")
        );
        assert_eq!(wsl_to_windows("/mnt/c", root).as_deref(), Some(r"C:\"));
        assert_eq!(wsl_to_windows("/home/me", root), None);
        assert_eq!(wsl_to_windows("/mnt/wsl/x", root), None);
    }

    #[test]
    fn translates_linux_and_unc_paths() {
        assert_eq!(
            linux_to_unc("/home/me/proj", "Ubuntu"),
            Some(PathBuf::from(r"\\wsl.localhost\Ubuntu\home\me\proj"))
        );
        assert_eq!(
            linux_to_unc("/", "Ubuntu"),
            Some(PathBuf::from(r"\\wsl.localhost\Ubuntu"))
        );
        assert_eq!(linux_to_unc("relative", "Ubuntu"), None);
        assert_eq!(
            unc_to_linux(r"\\wsl.localhost\Ubuntu\home\me"),
            Some(("Ubuntu".into(), "/home/me".into()))
        );
        assert_eq!(
            unc_to_linux(r"\\WSL$\Debian\root\"),
            Some(("Debian".into(), "/root".into()))
        );
        assert_eq!(
            unc_to_linux("//wsl.localhost/Ubuntu"),
            Some(("Ubuntu".into(), "/".into()))
        );
        assert_eq!(unc_to_linux(r"C:\Users"), None);
    }

    #[test]
    fn windows_tool_candidates_tries_plain_name_then_the_system32_fallback() {
        assert_eq!(
            windows_tool_candidates("tasklist.exe", Path::new("/mnt/")),
            vec![
                PathBuf::from("tasklist.exe"),
                PathBuf::from("/mnt/c/Windows/System32/tasklist.exe"),
            ]
        );
        assert_eq!(
            windows_tool_candidates("clip.exe", Path::new("/custom-root/")),
            vec![
                PathBuf::from("clip.exe"),
                PathBuf::from("/custom-root/c/Windows/System32/clip.exe"),
            ]
        );
    }

    #[test]
    fn unc_to_linux_accepts_the_verbatim_prefix_dunce_leaves_on_unc_paths() {
        // dunce::canonicalize keeps `\\?\UNC\...` verbatim for UNC paths (unlike `\\?\C:\...`,
        // which it simplifies), so a configured/discovered WSL source canonicalizes to this form.
        assert_eq!(
            unc_to_linux(r"\\?\UNC\wsl.localhost\Ubuntu\home\me"),
            Some(("Ubuntu".into(), "/home/me".into()))
        );
        assert_eq!(
            unc_to_linux(r"\\?\unc\wsl$\Debian\root"),
            Some(("Debian".into(), "/root".into()))
        );
        assert_eq!(
            unc_to_linux("//?/UNC/wsl.localhost/Ubuntu"),
            Some(("Ubuntu".into(), "/".into()))
        );
    }

    #[test]
    fn host_context_maps_between_environments() {
        let wsl = wsl_host();
        assert_eq!(
            wsl.to_host(Path::new(r"C:\Users\me"), &Env::Windows),
            Some(PathBuf::from("/mnt/c/Users/me"))
        );
        assert_eq!(
            wsl.to_host(Path::new("/home/x"), &wsl.env.clone()),
            Some(PathBuf::from("/home/x"))
        );
        assert_eq!(wsl.to_host(Path::new("/x"), &Env::MacOs), None);
        assert_eq!(
            wsl.to_env(Path::new("/mnt/c/Users/me/.ccs"), &Env::Windows),
            PathBuf::from(r"C:\Users\me\.ccs")
        );
        assert_eq!(
            wsl.infer_env(Path::new("/mnt/c/Users/me/.claude")),
            Env::Windows
        );
        assert_eq!(
            wsl.infer_env(Path::new("/home/me/.claude")),
            wsl.env.clone()
        );

        let win = windows_host();
        let ubuntu = Env::Wsl {
            distro: "Ubuntu".into(),
        };
        assert_eq!(
            win.to_host(Path::new("/home/me"), &ubuntu),
            Some(PathBuf::from(r"\\wsl.localhost\Ubuntu\home\me"))
        );
        assert_eq!(
            win.to_env(
                Path::new(r"\\wsl.localhost\Ubuntu\home\me\.claude"),
                &ubuntu
            ),
            PathBuf::from("/home/me/.claude")
        );
        assert_eq!(
            win.infer_env(Path::new(r"\\wsl$\Ubuntu\home\me\.claude")),
            ubuntu
        );
        assert_eq!(
            win.infer_env(Path::new(r"C:\Users\me\.claude")),
            Env::Windows
        );
        assert_eq!(
            win.to_env(
                Path::new(r"\\?\UNC\wsl.localhost\Ubuntu\home\me\.claude"),
                &ubuntu
            ),
            PathBuf::from("/home/me/.claude")
        );
        assert_eq!(
            win.infer_env(Path::new(r"\\?\UNC\wsl.localhost\Ubuntu\home\me\.claude")),
            ubuntu
        );
    }
}
