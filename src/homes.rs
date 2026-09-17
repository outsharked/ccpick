//! Home directories to scan: the native home plus, when enabled, the other side of a Windows +
//! WSL machine. Agent-neutral: a directory qualifies if it contains any provider's marker entry.
use crate::config::Settings;
use crate::env::{Env, HostContext, linux_to_unc};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    pub env: Env,
    /// The home as ccpick can read it.
    pub dir: PathBuf,
    /// The same home as its own environment sees it.
    pub env_dir: PathBuf,
    /// Prefix for source names; None for the native home.
    pub label: Option<String>,
}

impl Home {
    pub fn native(host: &HostContext, dir: PathBuf) -> Home {
        Home {
            env: host.env.clone(),
            env_dir: dir.clone(),
            dir,
            label: None,
        }
    }

    pub fn is_native(&self) -> bool {
        self.label.is_none()
    }
}

const WINDOWS_SYSTEM_PROFILES: &[&str] = &["Public", "Default", "Default User", "All Users"];

fn has_marker(dir: &Path, markers: &[&str]) -> bool {
    markers.iter().any(|m| dir.join(m).exists())
}

/// Windows user profiles under `<mount root>c/Users`, seen from WSL.
pub fn windows_homes(host: &HostContext, markers: &[&str]) -> Vec<Home> {
    let Ok(entries) = std::fs::read_dir(host.wsl_mount_root.join("c").join("Users")) else {
        return Vec::new();
    };
    let mut homes: Vec<Home> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let dir = entry.path();
            let system = WINDOWS_SYSTEM_PROFILES
                .iter()
                .any(|s| s.eq_ignore_ascii_case(&name));
            if system || !dir.is_dir() || !has_marker(&dir, markers) {
                return None;
            }
            Some(Home {
                env: Env::Windows,
                env_dir: PathBuf::from(format!(r"C:\Users\{name}")),
                dir,
                label: Some("win".into()),
            })
        })
        .collect();
    homes.sort_by(|a, b| a.dir.cmp(&b.dir));
    homes
}

/// Distro names from `wsl.exe -l -q` output: UTF-16LE (optional BOM) or UTF-8, with CRLF and
/// stray NULs.
pub fn parse_wsl_list(bytes: &[u8]) -> Vec<String> {
    let looks_utf16 = bytes.len() >= 2
        && bytes.len().is_multiple_of(2)
        && (bytes.starts_with(&[0xFF, 0xFE]) || bytes.iter().skip(1).step_by(2).all(|b| *b == 0));
    let text = if looks_utf16 {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    text.trim_start_matches('\u{feff}')
        .lines()
        .map(|line| {
            line.trim_matches(|c: char| c.is_whitespace() || c == '\0')
                .to_string()
        })
        .filter(|line| !line.is_empty())
        .collect()
}

/// Running WSL distros via `wsl.exe -l --running -q`. A non-zero exit (e.g. "no running
/// distributions") means none; only failing to run `wsl.exe` is an error.
pub fn running_distros() -> Result<Vec<String>, String> {
    let output = std::process::Command::new("wsl.exe")
        .args(["-l", "--running", "-q"])
        .output()
        .map_err(|e| format!("could not run wsl.exe: {e}"))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(parse_wsl_list(&output.stdout))
}

/// WSL homes (`/root`, `/home/<user>`) for distros whose root directory is given.
pub fn wsl_homes_in(distros: &[(String, PathBuf)], markers: &[&str]) -> Vec<Home> {
    let mut homes = Vec::new();
    for (distro, root) in distros {
        let mut candidates = vec![(root.join("root"), "/root".to_string())];
        if let Ok(entries) = std::fs::read_dir(root.join("home")) {
            for entry in entries.flatten() {
                let user = entry.file_name().to_string_lossy().into_owned();
                candidates.push((entry.path(), format!("/home/{user}")));
            }
        }
        for (dir, env_dir) in candidates {
            if dir.is_dir() && has_marker(&dir, markers) {
                homes.push(Home {
                    env: Env::Wsl {
                        distro: distro.clone(),
                    },
                    dir,
                    env_dir: PathBuf::from(env_dir),
                    label: None,
                });
            }
        }
    }
    let distinct: BTreeSet<String> = homes
        .iter()
        .filter_map(|h| match &h.env {
            Env::Wsl { distro } => Some(distro.clone()),
            _ => None,
        })
        .collect();
    for home in &mut homes {
        if let Env::Wsl { distro } = &home.env {
            home.label = Some(if distinct.len() == 1 {
                "wsl".into()
            } else {
                distro.clone()
            });
        }
    }
    homes
}

/// WSL homes in the named distros, via `\\wsl.localhost\<distro>`.
pub fn wsl_homes(distros: &[String], markers: &[&str]) -> Vec<Home> {
    let roots: Vec<(String, PathBuf)> = distros
        .iter()
        .filter_map(|d| linux_to_unc("/", d).map(|root| (d.clone(), root)))
        .collect();
    wsl_homes_in(&roots, markers)
}

/// All homes to scan, plus warnings. The native home is always first.
pub fn discover_homes(
    settings: &Settings,
    markers: &[&str],
    list_running: impl Fn() -> Result<Vec<String>, String>,
) -> (Vec<Home>, Vec<String>) {
    let host = &settings.host;
    let mut homes = vec![Home::native(host, settings.home.clone())];
    let mut warnings = Vec::new();
    if !settings.file.environments.auto {
        return (homes, warnings);
    }
    match &host.env {
        Env::Wsl { .. } => homes.extend(windows_homes(host, markers)),
        Env::Windows => {
            let mut distros = list_running().unwrap_or_else(|e| {
                warnings.push(e);
                Vec::new()
            });
            for forced in &settings.file.environments.wsl_distros {
                if !distros.iter().any(|d| d.eq_ignore_ascii_case(forced)) {
                    distros.push(forced.clone());
                }
            }
            homes.extend(wsl_homes(&distros, markers));
        }
        Env::Linux | Env::MacOs => {}
    }
    (homes, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config;
    use std::fs;

    const MARKERS: &[&str] = &[".agent"];

    #[test]
    fn decodes_wsl_list_output() {
        let utf16 = |s: &str, bom: bool| {
            let mut bytes: Vec<u8> = if bom { vec![0xFF, 0xFE] } else { vec![] };
            for unit in s.encode_utf16() {
                bytes.extend(unit.to_le_bytes());
            }
            bytes
        };
        assert_eq!(
            parse_wsl_list(&utf16("Ubuntu-24.04\r\nDebian\r\n", true)),
            vec!["Ubuntu-24.04", "Debian"]
        );
        assert_eq!(
            parse_wsl_list(&utf16("Ubuntu\r\n\0\r\n", false)),
            vec!["Ubuntu"]
        );
        assert_eq!(parse_wsl_list(b"Ubuntu\nArch\n"), vec!["Ubuntu", "Arch"]);
        assert!(parse_wsl_list(b"").is_empty());
    }

    #[test]
    fn finds_windows_profiles_with_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let users = tmp.path().join("c/Users");
        for (name, marked) in [("me", true), ("other", false), ("Public", true)] {
            fs::create_dir_all(users.join(name)).unwrap();
            if marked {
                fs::create_dir_all(users.join(name).join(".agent")).unwrap();
            }
        }
        let host = HostContext {
            env: Env::Wsl {
                distro: "Ubuntu".into(),
            },
            wsl_mount_root: PathBuf::from(format!("{}/", tmp.path().display())),
        };
        let homes = windows_homes(&host, MARKERS);
        assert_eq!(homes.len(), 1);
        assert_eq!(homes[0].env, Env::Windows);
        assert_eq!(homes[0].dir, users.join("me"));
        assert_eq!(homes[0].env_dir, PathBuf::from(r"C:\Users\me"));
        assert_eq!(homes[0].label.as_deref(), Some("win"));
    }

    #[test]
    fn finds_wsl_homes_and_labels_by_distro_count() {
        let tmp = tempfile::tempdir().unwrap();
        let ubuntu = tmp.path().join("ubuntu");
        fs::create_dir_all(ubuntu.join("home/me/.agent")).unwrap();
        fs::create_dir_all(ubuntu.join("home/nobody")).unwrap();
        fs::create_dir_all(ubuntu.join("root/.agent")).unwrap();

        let one = wsl_homes_in(&[("Ubuntu".into(), ubuntu.clone())], MARKERS);
        assert_eq!(one.len(), 2);
        assert!(one.iter().all(|h| h.label.as_deref() == Some("wsl")));
        assert!(one.iter().any(|h| h.env_dir == Path::new("/home/me")));
        assert!(one.iter().any(|h| h.env_dir == Path::new("/root")));

        let debian = tmp.path().join("debian");
        fs::create_dir_all(debian.join("home/me/.agent")).unwrap();
        let two = wsl_homes_in(
            &[("Ubuntu".into(), ubuntu), ("Debian".into(), debian)],
            MARKERS,
        );
        let labels: Vec<_> = two.iter().filter_map(|h| h.label.clone()).collect();
        assert!(labels.contains(&"Ubuntu".to_string()));
        assert!(labels.contains(&"Debian".to_string()));
    }

    #[test]
    fn discovery_respects_auto_flag_and_forced_distros() {
        let tmp = tempfile::tempdir().unwrap();
        let mut settings = Settings {
            home: tmp.path().to_path_buf(),
            host: HostContext {
                env: Env::Windows,
                wsl_mount_root: PathBuf::from("/mnt/"),
            },
            ..Default::default()
        };
        let (homes, warnings) = discover_homes(&settings, MARKERS, || Err("no wsl.exe".into()));
        assert_eq!(homes.len(), 1);
        assert!(homes[0].is_native());
        assert_eq!(warnings, vec!["no wsl.exe".to_string()]);

        settings.file = parse_config("[environments]\nauto = false\n").unwrap();
        let (homes, warnings) = discover_homes(&settings, MARKERS, || panic!("must not list"));
        assert_eq!(homes.len(), 1);
        assert!(warnings.is_empty());
    }
}
