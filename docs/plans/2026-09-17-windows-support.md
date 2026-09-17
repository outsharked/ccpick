# Windows Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make ccpick list/search sessions from the other side of a Windows + WSL machine (with a copy/paste resume dialog), and run natively on Windows.

**Architecture:** A new agent-neutral `env` module models execution environments and translates paths between WSL and Windows. `homes` discovers the native home plus the other environment's homes; providers discover sources per home, and each `Source` carries its `env`. A shared `process` probe answers "is pid N running" per pid domain. Sessions whose source env differs from the host open a dialog with a shell-specific command (`shell` + `clipboard` modules) instead of launching. Native Windows swaps Unix `exec` for spawn-and-wait.

**Tech Stack:** Rust 2024, ratatui 0.30, dunce (new), windows-sys (new, Windows only), existing crates.

**Spec:** `docs/specs/2026-09-17-windows-support-design.md` (builds on `docs/specs/2026-09-16-ccpick-design.md`). Also read `AGENTS.md`.

## Global Constraints

- Nothing outside `src/providers/claude/` may know Claude file formats, ccs, `CLAUDE_CONFIG_DIR`, `.claude`, `.ccs` or `pidDomain` field names. `env`, `homes`, `process`, `shell`, `clipboard`, `catalog`, `ui` stay agent-neutral.
- Stopped WSL distros are never accessed unless listed in `[environments] wsl_distros`.
- Foreign source names are `<label>:<name>`; labels are `win` (Windows homes seen from WSL), `wsl` (exactly one WSL distro contributes homes) or the distro name.
- A session is launchable iff its launch source's `env` equals the host env. Non-launchable sessions never produce `Action::Launch`.
- Commands shown to the user use the session environment's own path form (`C:\…` / `/home/…`), never ccpick's host form.
- Windows command syntax is PowerShell only; POSIX syntax for Linux/WSL/macOS.
- Canonicalize with `dunce::canonicalize`, not `std::fs::canonicalize`.
- Windows path comparisons are ASCII case-insensitive.
- Windows-only code behind `cfg(windows)`, Unix-only behind `cfg(unix)`.
- Run `mise check` before every commit (fmt check, clippy `-D warnings`, tests; no compiler warnings).
- Commit at the end of each task. Never push. Commit messages end with `Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>`.
- Never run the TUI (`ccpick`/`mise dev` without `--list`/`--sources`); never modify `~/.ccs`, `~/.claude`, or anything under `/mnt/c/Users/*/.ccs` / `.claude`.
- Tests must not depend on the real environment: inject `HostContext`, env maps, directory roots and process snapshots.

## File Structure

```
src/env.rs            NEW  Env, HostContext, host detection, wsl.conf root, path translation
src/homes.rs          NEW  Home, windows_homes (from WSL), wsl_homes (from Windows), parse_wsl_list, discover_homes
src/process.rs        NEW  PidDomain, ProcessProbe (/proc, tasklist.exe snapshot, Win32), tasklist CSV parsing
src/shell.rs          NEW  resume_command: LaunchPlan + Env → PowerShell / POSIX command text
src/clipboard.rs      NEW  copy(): clip.exe / pbcopy / wl-copy / xclip / xsel / OSC 52
src/testutil.rs       NEW  (cfg(test)) manifest_dir / fixture paths that work in Windows test binaries
src/model.rs          MOD  Source.env/env_config_dir/env_home; Provider::discover_sources(settings, home), home_markers, launch_records(source, probe)
src/config.rs         MOD  EnvironmentsConfig, SourceConfig.env, Settings.host
src/catalog.rs        MOD  build(providers, settings, homes, cache); host; is_launchable; host_cwd; probe
src/format.rs         MOD  shorten_home_in (env-aware)
src/report.rs         MOD  env column in --sources
src/launch.rs         MOD  cfg-gated exec / run_and_wait; PATHEXT-aware find_in_path
src/main.rs           MOD  home discovery; platform launch
src/providers/claude/sources.rs  MOD  discover per home; labels; env inference; env_config_dir
src/providers/claude/live.rs     MOD  pidDomain; liveness via ProcessProbe
src/providers/claude/mod.rs      MOD  new trait signatures; dunce
src/providers/fake.rs            MOD  env-aware sources
src/ui/app.rs         MOD  foreign sessions → ResumeDialog; Action::Copy
src/ui/render.rs      MOD  env-aware shortening/dimming; dialog rendering
src/ui/mod.rs         MOD  Copy action; resolved launcher path
mise.toml, dist-workspace.toml, .github/workflows/*, README.md, AGENTS.md, docs/specs/2026-09-17-windows-support-design.md
```

---

## Phase 1 — Environments and discovery

### Task 1: `env` module

**Files:**
- Create: `src/env.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces:
  - `env::Env { Linux, Wsl { distro: String }, Windows, MacOs }` (`Debug, Clone, PartialEq, Eq, Hash`) with `parse(&str) -> Option<Env>`, `id(&self) -> String` (`linux`/`windows`/`macos`/`wsl:<distro>`), `display_name(&self) -> String` (`Linux`/`Windows`/`macOS`/`WSL (<distro>)`), `shell_name(&self) -> &'static str` (`PowerShell` / `a WSL shell` / `a shell`), `is_windows(&self) -> bool`
  - `env::HostContext { env: Env, wsl_mount_root: PathBuf }` (`Debug, Clone, PartialEq, Eq`, `Default` = Linux, `/mnt/`) with `detect(&HashMap<String,String>) -> HostContext`, `to_host(&self, &Path, from: &Env) -> Option<PathBuf>`, `to_env(&self, &Path, to: &Env) -> PathBuf`, `infer_env(&self, &Path) -> Env`
  - free fns: `detect_env(is_windows: bool, is_macos: bool, &HashMap<String,String>, wsl_interop: bool) -> Env`, `parse_automount_root(&str) -> PathBuf`, `windows_to_wsl(&str, &Path) -> Option<PathBuf>`, `wsl_to_windows(&str, &Path) -> Option<String>`, `linux_to_unc(&str, distro: &str) -> Option<PathBuf>`, `unc_to_linux(&str) -> Option<(String, String)>`

- [ ] **Step 1: Register the module**

In `src/lib.rs` add `pub mod env;` (keep alphabetical order).

- [ ] **Step 2: Write the failing tests** at the bottom of `src/env.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }
    fn wsl_host() -> HostContext {
        HostContext {
            env: Env::Wsl { distro: "Ubuntu".into() },
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
            Env::Wsl { distro: "Ubuntu-24.04".into() },
        ] {
            assert_eq!(Env::parse(&e.id()), Some(e.clone()));
        }
        assert_eq!(Env::parse("WINDOWS"), Some(Env::Windows));
        assert_eq!(Env::parse("wsl:"), None);
        assert_eq!(Env::parse("dos"), None);
        assert_eq!(Env::Wsl { distro: "Ubuntu".into() }.display_name(), "WSL (Ubuntu)");
        assert_eq!(Env::Windows.shell_name(), "PowerShell");
    }

    #[test]
    fn detects_host_environment() {
        assert_eq!(detect_env(true, false, &vars(&[]), false), Env::Windows);
        assert_eq!(detect_env(false, true, &vars(&[]), false), Env::MacOs);
        assert_eq!(
            detect_env(false, false, &vars(&[("WSL_DISTRO_NAME", "Ubuntu")]), false),
            Env::Wsl { distro: "Ubuntu".into() }
        );
        assert_eq!(
            detect_env(false, false, &vars(&[]), true),
            Env::Wsl { distro: "wsl".into() }
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
        assert_eq!(windows_to_wsl("d:/data/", root), Some(PathBuf::from("/mnt/d/data")));
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
        assert_eq!(wsl.infer_env(Path::new("/mnt/c/Users/me/.claude")), Env::Windows);
        assert_eq!(wsl.infer_env(Path::new("/home/me/.claude")), wsl.env.clone());

        let win = windows_host();
        let ubuntu = Env::Wsl { distro: "Ubuntu".into() };
        assert_eq!(
            win.to_host(Path::new("/home/me"), &ubuntu),
            Some(PathBuf::from(r"\\wsl.localhost\Ubuntu\home\me"))
        );
        assert_eq!(
            win.to_env(Path::new(r"\\wsl.localhost\Ubuntu\home\me\.claude"), &ubuntu),
            PathBuf::from("/home/me/.claude")
        );
        assert_eq!(
            win.infer_env(Path::new(r"\\wsl$\Ubuntu\home\me\.claude")),
            ubuntu
        );
        assert_eq!(win.infer_env(Path::new(r"C:\Users\me\.claude")), Env::Windows);
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test env::`
Expected: compile errors (`Env`, `HostContext`, functions not defined).

- [ ] **Step 4: Implement `src/env.rs`** (above the tests):

```rust
//! Execution environments (native Linux, WSL, Windows, macOS) and path translation between a
//! WSL distro and its Windows host. Agent-neutral.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Env {
    /// Native Linux, not WSL.
    Linux,
    Wsl { distro: String },
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
            _ if t.len() > 4 && t[..4].eq_ignore_ascii_case("wsl:") => Some(Env::Wsl {
                distro: t[4..].to_string(),
            }),
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
            distro: "wsl".into(),
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
/// any case) → (distro, `/home/me`).
pub fn unc_to_linux(path: &str) -> Option<(String, String)> {
    let norm = path.replace('/', "\\");
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
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test env::`
Expected: 7 tests PASS.

- [ ] **Step 6: Check and commit**

```bash
mise check
git add src/env.rs src/lib.rs
git commit -m "Add env module: environments, host detection, WSL/Windows path translation

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 2: Environments config and home discovery

**Files:**
- Create: `src/homes.rs`
- Modify: `src/config.rs`, `src/lib.rs`

**Interfaces:**
- Consumes: `env::{Env, HostContext, linux_to_unc}` (Task 1)
- Produces:
  - `config::EnvironmentsConfig { auto: bool /*default true*/, wsl_distros: Vec<String> }`; `ConfigFile.environments: EnvironmentsConfig`; `SourceConfig.env: Option<String>`; `Settings.host: HostContext` (set by `load_settings` via `HostContext::detect`)
  - `homes::Home { env: Env, dir: PathBuf, env_dir: PathBuf, label: Option<String> }` (`Debug, Clone, PartialEq, Eq`) with `Home::native(&HostContext, PathBuf) -> Home`, `is_native(&self) -> bool`
  - `homes::windows_homes(&HostContext, markers: &[&str]) -> Vec<Home>`
  - `homes::wsl_homes(distros: &[String], markers: &[&str]) -> Vec<Home>` and testable `homes::wsl_homes_in(distros: &[(String, PathBuf)], markers: &[&str]) -> Vec<Home>`
  - `homes::parse_wsl_list(&[u8]) -> Vec<String>`, `homes::running_distros() -> Result<Vec<String>, String>`
  - `homes::discover_homes(&Settings, markers: &[&str], list_running: impl Fn() -> Result<Vec<String>, String>) -> (Vec<Home>, Vec<String>)`

- [ ] **Step 1: Register the module** — add `pub mod homes;` to `src/lib.rs`.

- [ ] **Step 2: Write failing config tests** — add to the tests module in `src/config.rs`:

```rust
    #[test]
    fn parses_environments_and_source_env() {
        let cfg = parse_config(
            "[environments]\nauto = false\nwsl_distros = [\"Ubuntu\"]\n\n[[source]]\nconfig_dir = \"/mnt/c/Users/me/.claude\"\nenv = \"windows\"\n",
        )
        .unwrap();
        assert!(!cfg.environments.auto);
        assert_eq!(cfg.environments.wsl_distros, vec!["Ubuntu".to_string()]);
        assert_eq!(cfg.sources[0].env.as_deref(), Some("windows"));
        assert!(!cfg.agents.contains_key("environments"));
        assert!(parse_config("").unwrap().environments.auto);
    }
```

- [ ] **Step 3: Write failing homes tests** at the bottom of `src/homes.rs`:

```rust
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
        assert_eq!(parse_wsl_list(&utf16("Ubuntu\r\n\0\r\n", false)), vec!["Ubuntu"]);
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
            env: Env::Wsl { distro: "Ubuntu".into() },
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
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cargo test homes:: config::`
Expected: compile errors.

- [ ] **Step 5: Implement config changes** in `src/config.rs`:

Add, above `ConfigFile`:

```rust
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct EnvironmentsConfig {
    /// Discover the other side of a Windows + WSL machine.
    pub auto: bool,
    /// Windows host only: WSL distros to scan even when not running (boots them).
    pub wsl_distros: Vec<String>,
}

impl Default for EnvironmentsConfig {
    fn default() -> Self {
        EnvironmentsConfig {
            auto: true,
            wsl_distros: Vec::new(),
        }
    }
}
```

In `ConfigFile`, add the field before the flattened `agents`:

```rust
    pub environments: EnvironmentsConfig,
```

In `SourceConfig`, add:

```rust
    /// `windows`, `linux`, `macos` or `wsl:<distro>`; inferred from the path when omitted.
    pub env: Option<String>,
```

In `Settings`, add `pub host: crate::env::HostContext,` and in `load_settings` build it from the same environment map:

```rust
    let env: HashMap<String, String> = std::env::vars().collect();
    Ok(Settings {
        home,
        host: crate::env::HostContext::detect(&env),
        env,
        file,
        cli_config_dirs,
    })
```

- [ ] **Step 6: Implement `src/homes.rs`** (above the tests):

```rust
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
        && bytes.len() % 2 == 0
        && (bytes.starts_with(&[0xFF, 0xFE]) || bytes.iter().skip(1).step_by(2).all(|b| *b == 0));
    let text = if looks_utf16 {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
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
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test homes:: config::`
Expected: all PASS (5 new).

- [ ] **Step 8: Check and commit**

```bash
mise check
git add src/homes.rs src/config.rs src/lib.rs
git commit -m "Add home discovery and [environments] config

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 3: Env-aware sources and provider discovery per home

**Files:**
- Modify: `Cargo.toml` (add `dunce`), `src/model.rs`, `src/providers/claude/sources.rs`, `src/providers/claude/mod.rs`, `src/providers/fake.rs`

**Interfaces:**
- Consumes: `Env`, `HostContext` (Task 1); `Home`, `Settings.host`, `SourceConfig.env` (Task 2)
- Produces:
  - `model::Source { agent, name, config_dir, env: Env, env_config_dir: PathBuf, env_home: PathBuf, launch }` — `env_config_dir`: `config_dir` as its environment sees it; `env_home`: that environment's home dir for `~` shortening (empty `PathBuf` when unknown)
  - `Provider::discover_sources(&self, settings: &Settings, home: &Home) -> anyhow::Result<Discovery>`
  - `Provider::home_markers(&self) -> &'static [&'static str]` (default `&[]`; Claude: `&[".claude", ".ccs"]`)
  - `providers::claude::sources::discover(&Settings, &Home) -> anyhow::Result<Discovery>`
  - `FakeProvider::add_source_in(&mut self, name: &str, store: &str, env: Env, env_home: &str)`; `add_source` keeps its signature (env `Linux`, env_home `/fake`)

- [ ] **Step 1: Add dependency**

```bash
cargo add dunce
```

- [ ] **Step 2: Update `src/model.rs`**

Replace `Source` with:

```rust
/// One agent config directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub agent: String,
    pub name: String,
    /// Canonical path ccpick reads.
    pub config_dir: PathBuf,
    /// Environment the source belongs to.
    pub env: crate::env::Env,
    /// `config_dir` as the source's own environment sees it (used in commands for the user).
    pub env_config_dir: PathBuf,
    /// Home directory as the source's environment sees it, for `~` shortening (empty if unknown).
    pub env_home: PathBuf,
    pub launch: LaunchSpec,
}
```

In `Provider`, replace `discover_sources` and add `home_markers`:

```rust
    /// Sources for this agent in one home. Err = invalid explicit configuration (fatal).
    fn discover_sources(
        &self,
        settings: &Settings,
        home: &crate::homes::Home,
    ) -> anyhow::Result<Discovery>;
    /// Entries (e.g. a config directory name) whose presence makes a directory worth scanning
    /// as a home in another environment.
    fn home_markers(&self) -> &'static [&'static str] {
        &[]
    }
```

- [ ] **Step 3: Write failing tests** in `src/providers/claude/sources.rs` tests module. First update the existing tests to the new signature — add helpers and change every `discover(&s)` / `discover(&settings(tmp.path()))` call to `discover(&s, &native(&s))`:

```rust
    use crate::env::{Env, HostContext};
    use crate::homes::Home;

    fn native(s: &Settings) -> Home {
        Home::native(&s.host, s.home.clone())
    }
```

(For calls written as `discover(&settings(tmp.path()))`, bind `let s = settings(tmp.path());` first.)

Then add:

```rust
    fn windows_home_under_wsl(tmp: &Path) -> (Settings, Home) {
        let root = dunce::canonicalize(tmp).unwrap();
        let profile = root.join("mnt/c/Users/me");
        fs::create_dir_all(profile.join(".ccs")).unwrap();
        fs::write(profile.join(".ccs/config.yaml"), CCS_YAML).unwrap();
        for n in ["c1", "c2", "c3"] {
            fs::create_dir_all(profile.join(".ccs/instances").join(n)).unwrap();
        }
        fs::create_dir_all(profile.join(".claude")).unwrap();
        let host = HostContext {
            env: Env::Wsl { distro: "Ubuntu".into() },
            wsl_mount_root: PathBuf::from(format!("{}/", root.join("mnt").display())),
        };
        let mut s = settings(&root.join("linux-home"));
        s.host = host;
        s.env.insert(ENV_CONFIG_DIR.into(), "/should/not/apply".into());
        s.cli_config_dirs = vec![root.join("cli-dir")];
        let home = Home {
            env: Env::Windows,
            dir: profile,
            env_dir: PathBuf::from(r"C:\Users\me"),
            label: Some("win".into()),
        };
        (s, home)
    }

    #[test]
    fn foreign_home_sources_are_prefixed_and_use_their_own_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let (s, home) = windows_home_under_wsl(tmp.path());
        let d = discover(&s, &home).unwrap();
        assert_eq!(names(&d), vec!["win:c2", "win:c1", "win:c3", "win:claude"]);
        assert!(d.sources.iter().all(|src| src.env == Env::Windows));
        assert_eq!(
            d.sources[1].env_config_dir,
            PathBuf::from(r"C:\Users\me\.ccs\instances\c1")
        );
        assert_eq!(d.sources[1].env_home, PathBuf::from(r"C:\Users\me"));
        assert_eq!(d.sources[1].launch.argv_prefix, vec!["ccs", "c1"]);
    }

    #[test]
    fn env_var_config_and_cli_sources_only_apply_to_native_home() {
        let tmp = tempfile::tempdir().unwrap();
        let (s, home) = windows_home_under_wsl(tmp.path());
        let d = discover(&s, &home).unwrap();
        assert!(d.warnings.is_empty());
        assert!(!names(&d).iter().any(|n| n.contains("cli-dir") || n.contains("apply")));
    }

    #[test]
    fn configured_windows_path_is_inferred_and_uses_windows_env_value() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, _) = windows_home_under_wsl(tmp.path());
        s.env.clear();
        s.cli_config_dirs.clear();
        let dir = tmp.path().join("mnt/c/Users/me/.claude");
        s.file = parse_config(&format!(
            "[claude]\nccs = false\nhome = false\n\n[[source]]\nname = \"winclaude\"\nconfig_dir = \"{}\"\n",
            dunce::canonicalize(&dir).unwrap().display()
        ))
        .unwrap();
        let d = discover(&s, &native(&s)).unwrap();
        assert_eq!(names(&d), vec!["winclaude"]);
        assert_eq!(d.sources[0].env, Env::Windows);
        assert_eq!(
            d.sources[0].launch.env_set,
            vec![(ENV_CONFIG_DIR.to_string(), r"C:\Users\me\.claude".to_string())]
        );
    }

    #[test]
    fn explicit_source_env_overrides_and_invalid_env_errors() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("cfg")).unwrap();
        let mut s = settings(tmp.path());
        s.file = parse_config(&format!(
            "[claude]\nccs = false\nhome = false\n\n[[source]]\nconfig_dir = \"{}\"\nenv = \"wsl:Debian\"\n",
            tmp.path().join("cfg").display()
        ))
        .unwrap();
        let d = discover(&s, &native(&s)).unwrap();
        assert_eq!(d.sources[0].env, Env::Wsl { distro: "Debian".into() });

        s.file = parse_config(&format!(
            "[[source]]\nconfig_dir = \"{}\"\nenv = \"dos\"\n",
            tmp.path().join("cfg").display()
        ))
        .unwrap();
        assert!(discover(&s, &native(&s)).is_err());
    }

    #[test]
    fn native_sources_record_host_env_and_home() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        let s = settings(tmp.path());
        let d = discover(&s, &native(&s)).unwrap();
        assert_eq!(d.sources[0].env, Env::Linux);
        assert_eq!(d.sources[0].env_home, tmp.path());
        assert_eq!(d.sources[0].env_config_dir, d.sources[0].config_dir);
    }
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cargo test sources::`
Expected: compile errors (`discover` arity, `Source` fields).

- [ ] **Step 5: Implement in `src/providers/claude/sources.rs`**

Change imports to include `crate::env::Env` and `crate::homes::Home`. Replace `fn source(...)` with:

```rust
fn source(name: String, config_dir: PathBuf, env: Env, env_home: PathBuf, launch: LaunchSpec) -> Source {
    Source {
        agent: AGENT.into(),
        name,
        env_config_dir: config_dir.clone(),
        config_dir,
        env,
        env_home,
        launch,
    }
}
```

Replace `pub fn discover(settings: &Settings) -> anyhow::Result<Discovery>` and its body with:

```rust
pub fn discover(settings: &Settings, home: &Home) -> anyhow::Result<Discovery> {
    let cfg: ClaudeConfig = settings.agent_table(AGENT)?;
    let host = &settings.host;
    let mut out = Discovery::default();
    let mut candidates: Vec<Candidate> = Vec::new();
    let named = |name: String| match &home.label {
        Some(label) => format!("{label}:{name}"),
        None => name,
    };

    if cfg.ccs {
        match ccs_accounts(&home.dir) {
            Ok(names) => {
                for name in names {
                    let dir = home.dir.join(".ccs").join("instances").join(&name);
                    let launch = LaunchSpec {
                        argv_prefix: vec!["ccs".into(), name.clone()],
                        ..Default::default()
                    };
                    candidates.push(Candidate {
                        source: source(named(name), dir, home.env.clone(), home.env_dir.clone(), launch),
                        explicit: true,
                        needs_config_dir_env: false,
                    });
                }
            }
            Err(warning) => out.warnings.push(warning),
        }
    }

    if cfg.home {
        if home.is_native()
            && let Some(dir) = settings.env_var(ENV_CONFIG_DIR)
        {
            let dir = settings.expand(dir);
            candidates.push(Candidate {
                source: source(basename(&dir), dir, host.env.clone(), settings.home.clone(), bare_claude()),
                explicit: true,
                needs_config_dir_env: true,
            });
        }
        let launch = LaunchSpec {
            argv_prefix: vec!["claude".into()],
            env_remove: vec![ENV_CONFIG_DIR.into()],
            ..Default::default()
        };
        candidates.push(Candidate {
            source: source(
                named("claude".into()),
                home.dir.join(".claude"),
                home.env.clone(),
                home.env_dir.clone(),
                launch,
            ),
            explicit: false,
            needs_config_dir_env: false,
        });
    }

    if home.is_native() {
        for sc in settings.file.sources.iter().filter(|s| s.agent == AGENT) {
            let dir = settings.expand(&sc.config_dir);
            let name = sc.name.clone().unwrap_or_else(|| basename(&dir));
            let env = match &sc.env {
                Some(text) => Env::parse(text).ok_or_else(|| {
                    anyhow::anyhow!(
                        "source {name}: invalid env {text:?} (use windows, linux, macos or wsl:<distro>)"
                    )
                })?,
                None => host.infer_env(&dir),
            };
            let env_home = if env == host.env {
                settings.home.clone()
            } else {
                PathBuf::new()
            };
            let (launch, needs_config_dir_env) = match &sc.command {
                Some(argv) if !argv.is_empty() => (
                    LaunchSpec {
                        argv_prefix: argv.clone(),
                        ..Default::default()
                    },
                    false,
                ),
                _ => (bare_claude(), true),
            };
            candidates.push(Candidate {
                source: source(name, dir, env, env_home, launch),
                explicit: true,
                needs_config_dir_env,
            });
        }

        for dir in &settings.cli_config_dirs {
            candidates.push(Candidate {
                source: source(basename(dir), dir.clone(), host.env.clone(), settings.home.clone(), bare_claude()),
                explicit: true,
                needs_config_dir_env: true,
            });
        }
    }

    let mut seen = HashSet::new();
    for Candidate {
        mut source,
        explicit,
        needs_config_dir_env,
    } in candidates
    {
        match dunce::canonicalize(&source.config_dir) {
            Ok(canonical) if canonical.is_dir() => {
                source.env_config_dir = host.to_env(&canonical, &source.env);
                if needs_config_dir_env {
                    source.launch.env_set = vec![(
                        ENV_CONFIG_DIR.into(),
                        source.env_config_dir.display().to_string(),
                    )];
                }
                if seen.insert(canonical.clone()) {
                    source.config_dir = canonical;
                    out.sources.push(source);
                }
            }
            _ if explicit => out.warnings.push(format!(
                "source {} skipped: {} does not exist",
                source.name,
                source.config_dir.display()
            )),
            _ => {}
        }
    }
    Ok(out)
}
```

- [ ] **Step 6: Update `src/providers/claude/mod.rs`**

```rust
    fn discover_sources(
        &self,
        settings: &Settings,
        home: &crate::homes::Home,
    ) -> anyhow::Result<Discovery> {
        sources::discover(settings, home)
    }
    fn home_markers(&self) -> &'static [&'static str] {
        &[".claude", ".ccs"]
    }
    fn store_for(&self, source: &Source) -> Option<PathBuf> {
        dunce::canonicalize(source.config_dir.join("projects"))
            .ok()
            .filter(|p| p.is_dir())
    }
```

In its tests, every `Source { .. }` literal gains:

```rust
            env: crate::env::Env::Linux,
            env_config_dir: PathBuf::from("/c"),
            env_home: PathBuf::new(),
```

(use the literal's own `config_dir` value for `env_config_dir`), and any `std::fs::canonicalize` in assertions becomes `dunce::canonicalize`.

- [ ] **Step 7: Update `src/providers/fake.rs`**

Replace `add_source` with:

```rust
    pub fn add_source(&mut self, name: &str, store: &str) {
        self.add_source_in(name, store, crate::env::Env::Linux, "/fake");
    }

    /// A source in a specific environment (e.g. Windows seen from WSL).
    pub fn add_source_in(&mut self, name: &str, store: &str, env: crate::env::Env, env_home: &str) {
        let config_dir = PathBuf::from(format!("/fake/{name}"));
        self.sources.push(Source {
            agent: "fake".into(),
            name: name.into(),
            env_config_dir: config_dir.clone(),
            config_dir,
            env,
            env_home: PathBuf::from(env_home),
            launch: LaunchSpec {
                argv_prefix: vec!["fake".into(), name.into()],
                ..Default::default()
            },
        });
        self.stores.insert(name.into(), PathBuf::from(store));
    }
```

and `discover_sources` becomes (sources are reported once, on the native home's pass):

```rust
    fn discover_sources(
        &self,
        _settings: &Settings,
        home: &crate::homes::Home,
    ) -> anyhow::Result<Discovery> {
        let sources = if home.is_native() {
            self.sources.clone()
        } else {
            Vec::new()
        };
        Ok(Discovery {
            sources,
            warnings: vec![],
        })
    }
```

Its own test calls `p.discover_sources(&Settings::default(), &crate::homes::Home::native(&Default::default(), "/fake".into()))`.

- [ ] **Step 8: Make the crate compile**

`src/catalog.rs` still calls `provider.discover_sources(settings)`. Minimal bridge for this task only (Task 4 replaces it):

```rust
            let home = crate::homes::Home::native(&settings.host, settings.home.clone());
            let discovery = provider.discover_sources(settings, &home)?;
```

- [ ] **Step 9: Run tests**

Run: `cargo test`
Expected: all PASS (5 new in `sources::`).

- [ ] **Step 10: Check and commit**

```bash
mise check
git add -A
git commit -m "Sources carry their environment; providers discover per home

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 4: Catalog over homes; launchability; `--sources` env column; wiring

**Files:**
- Modify: `src/catalog.rs`, `src/main.rs`, `src/report.rs`, `src/ui/render.rs` (test helpers only)

**Interfaces:**
- Consumes: `Home`, `discover_homes`, `running_distros` (Task 2); `Source.env` (Task 3)
- Produces:
  - `Catalog::build(providers: Vec<Box<dyn Provider>>, settings: &Settings, homes: &[Home], cache: &mut Cache) -> anyhow::Result<Catalog>`
  - `Catalog.host: HostContext`
  - `Catalog::is_launchable(&self, source: usize) -> bool`
  - `Catalog::host_cwd(&self, idx: usize, source: usize) -> Option<PathBuf>` — the session cwd translated to a host-readable path (None when no cwd or no mapping)
  - `#[cfg(test)] catalog::build_fake(p: FakeProvider) -> Catalog` and `catalog::build_fake_on(p: FakeProvider, host: HostContext) -> Catalog`
  - `#[cfg(test)] catalog::fake_catalog_with_foreign() -> Catalog`: host `Wsl{Ubuntu}` with mount root `/nonexistent-root/`; source 0 `one` (env `Wsl{Ubuntu}` = host, store `/s`), source 1 `win:c1` (env `Windows`, env_home `C:\Users\me`, store `/w`); session `n` in `/s` (cwd `/tmp`, last_ts 2000), session `w` in `/w` (cwd `C:\Users\me\proj`, last_ts 1000)
  - `--sources` line format: `agent\tname\tenv-id\tconfig_dir\tstore\tlaunch`

- [ ] **Step 1: Write failing tests** in `src/catalog.rs`. Replace the body of `fake_catalog()`'s final line with `build_fake(p)` and add the helpers above the tests module:

```rust
#[cfg(test)]
pub fn build_fake(p: crate::providers::fake::FakeProvider) -> Catalog {
    build_fake_on(p, crate::env::HostContext::default())
}

#[cfg(test)]
pub fn build_fake_on(
    p: crate::providers::fake::FakeProvider,
    host: crate::env::HostContext,
) -> Catalog {
    let settings = Settings {
        host,
        ..Default::default()
    };
    let homes = [crate::homes::Home::native(&settings.host, "/fake".into())];
    Catalog::build(vec![Box::new(p)], &settings, &homes, &mut Cache::in_memory()).unwrap()
}

#[cfg(test)]
pub fn fake_catalog_with_foreign() -> Catalog {
    use crate::env::{Env, HostContext};
    use crate::model::Role;
    use crate::providers::fake::FakeProvider;
    let ubuntu = Env::Wsl {
        distro: "Ubuntu".into(),
    };
    let mut p = FakeProvider::default();
    p.add_source_in("one", "/s", ubuntu.clone(), "/fake");
    p.add_source_in("win:c1", "/w", Env::Windows, r"C:\Users\me");
    p.add_session("/s", "n", "Native session", 2000, 2000, &[(Role::User, "hello")]);
    p.add_session("/w", "w", "Windows session", 1000, 1000, &[(Role::User, "from windows")]);
    p.set_cwd("/w", "w", Some(r"C:\Users\me\proj"));
    build_fake_on(
        p,
        HostContext {
            env: ubuntu,
            wsl_mount_root: PathBuf::from("/nonexistent-root/"),
        },
    )
}
```

Update every other `Catalog::build(vec![Box::new(p)], &Settings::default(), &mut Cache::in_memory())` in `src/catalog.rs` tests to `build_fake(p)`. Update `real_files_use_cache` to pass homes:

```rust
        let homes = [crate::homes::Home::native(&settings.host, settings.home.clone())];
        let c1 = Catalog::build(vec![Box::new(ClaudeProvider)], &settings, &homes, &mut cache).unwrap();
```

(and the same for `c2`). Add tests:

```rust
    #[test]
    fn foreign_sources_are_not_launchable_and_cwd_translates() {
        let c = fake_catalog_with_foreign();
        let w = c.sessions.iter().position(|s| s.meta.id == "w").unwrap();
        let n = c.sessions.iter().position(|s| s.meta.id == "n").unwrap();
        assert!(c.is_launchable(c.sessions[n].default_source));
        assert!(!c.is_launchable(c.sessions[w].default_source));
        assert_eq!(
            c.host_cwd(w, c.sessions[w].default_source),
            Some(PathBuf::from("/nonexistent-root/c/Users/me/proj"))
        );
        assert_eq!(
            c.host_cwd(n, c.sessions[n].default_source),
            Some(PathBuf::from("/tmp"))
        );
    }

    #[test]
    fn scans_every_home() {
        use crate::homes::Home;
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["native/.claude/projects/p", "win/.claude/projects/p"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        let fixture = crate::providers::claude::transcript::fixture_path("basic.jsonl");
        std::fs::copy(&fixture, tmp.path().join("native/.claude/projects/p/a.jsonl")).unwrap();
        std::fs::copy(&fixture, tmp.path().join("win/.claude/projects/p/b.jsonl")).unwrap();
        let settings = Settings {
            home: tmp.path().join("native"),
            ..Default::default()
        };
        let homes = [
            Home::native(&settings.host, settings.home.clone()),
            Home {
                env: crate::env::Env::Windows,
                dir: tmp.path().join("win"),
                env_dir: PathBuf::from(r"C:\Users\me"),
                label: Some("win".into()),
            },
        ];
        let c = Catalog::build(vec![Box::new(ClaudeProvider)], &settings, &homes, &mut Cache::in_memory()).unwrap();
        let names: Vec<&str> = c.sources.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["claude", "win:claude"]);
        assert_eq!(c.sessions.len(), 2);
    }
```

`fixture_path` doesn't exist yet: add to `src/providers/claude/transcript.rs` (outside its tests module):

```rust
/// Path of a test fixture under `tests/fixtures/claude/`.
#[cfg(test)]
pub fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude")
        .join(name)
}
```

and make transcript's own tests' `fixture()` call it.

In `src/report.rs` tests, change the expected `--sources` line to `"fake\ttwo\tlinux\t/fake/two\t/s\tfake two"`.

In `src/ui/render.rs` tests, replace each `Catalog::build(vec![Box::new(p)], &Settings::default(), &mut Cache::in_memory()).unwrap()` with `crate::catalog::build_fake(p)` (drop the now-unused imports).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test`
Expected: compile errors (`build` arity, `is_launchable`, `host_cwd`).

- [ ] **Step 3: Implement in `src/catalog.rs`**

Add `pub host: crate::env::HostContext,` to `Catalog`. Change `build`'s signature to take `homes: &[crate::homes::Home]` after `settings`, and replace the discovery loop with:

```rust
        for home in homes {
            for (pi, provider) in providers.iter().enumerate() {
                let discovery = provider.discover_sources(settings, home)?;
                warnings.extend(discovery.warnings);
                for source in discovery.sources {
                    sources.push(source);
                    source_provider.push(pi);
                }
            }
        }
```

Return `host: settings.host.clone()` in the `Catalog` literal. Add methods:

```rust
    /// True when ccpick can launch sessions from this source itself.
    pub fn is_launchable(&self, source: usize) -> bool {
        self.sources[source].env == self.host.env
    }

    /// The session's project dir as a path ccpick can check on disk.
    pub fn host_cwd(&self, idx: usize, source: usize) -> Option<PathBuf> {
        let cwd = self.sessions[idx].meta.cwd.as_ref()?;
        self.host.to_host(cwd, &self.sources[source].env)
    }
```

- [ ] **Step 4: Implement `--sources` env column** in `src/report.rs` `sources_report`:

```rust
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}",
            source.agent,
            source.name,
            source.env.id(),
            source.config_dir.display(),
            store,
            describe_launch(&source.launch)
        );
```

- [ ] **Step 5: Wire homes in `src/main.rs`**

Replace the `let catalog = Catalog::build(...)` line with:

```rust
    let providers = providers::all();
    let markers: Vec<&str> = providers
        .iter()
        .flat_map(|p| p.home_markers().iter().copied())
        .collect();
    let (homes, home_warnings) =
        homes::discover_homes(&settings, &markers, homes::running_distros);
    let mut catalog = Catalog::build(providers, &settings, &homes, &mut cache)?;
    catalog.warnings.extend(home_warnings);
```

and import `homes` in `use ccpick::{...}`.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test`
Expected: all PASS.

- [ ] **Step 7: Smoke test on real data (read-only)**

Run: `mise dev -- --sources`
Expected (on a WSL host with Windows Claude data): native lines with env `wsl:<distro>` plus `win:…` lines with env `windows` and config dirs under `/mnt/c/Users/<user>/`. Include the output in the report.

- [ ] **Step 8: Check and commit**

```bash
mise check
git add -A
git commit -m "Catalog scans every home; launchability and host cwd translation

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 5: UI shortening/dimming per environment; foreign sessions don't launch

**Files:**
- Modify: `src/format.rs`, `src/ui/render.rs`, `src/ui/app.rs`, `src/ui/mod.rs`

**Interfaces:**
- Consumes: `Catalog::{is_launchable, host_cwd}`, `Source.{env, env_home}` (Tasks 3–4), `fake_catalog_with_foreign` (Task 4)
- Produces:
  - `format::shorten_home_in(path: &Path, home: &Path, env: &Env) -> String`
  - `render::draw(frame, app, now_ms)` — the `home: &Path` parameter is removed (shortening uses each source's `env_home`)
  - Enter on a non-launchable, non-running session sets status `this session lives in <Env::display_name> — resume it there` and returns `Action::None` (Task 8 replaces this with the dialog)

- [ ] **Step 1: Write failing tests**

In `src/format.rs` tests:

```rust
    #[test]
    fn shortens_windows_paths_case_insensitively() {
        use crate::env::Env;
        let home = Path::new(r"C:\Users\me");
        assert_eq!(
            shorten_home_in(Path::new(r"c:\users\me\code\x"), home, &Env::Windows),
            r"~\code\x"
        );
        assert_eq!(
            shorten_home_in(Path::new(r"C:\Users\me"), home, &Env::Windows),
            "~"
        );
        assert_eq!(
            shorten_home_in(Path::new(r"C:\Users\meow"), home, &Env::Windows),
            r"C:\Users\meow"
        );
        assert_eq!(
            shorten_home_in(Path::new("/home/u/x"), Path::new("/home/u"), &Env::Linux),
            "~/x"
        );
        assert_eq!(
            shorten_home_in(Path::new("/x"), Path::new(""), &Env::Linux),
            "/x"
        );
    }
```

In `src/ui/app.rs` tests:

```rust
    #[test]
    fn enter_on_foreign_session_does_not_launch() {
        let mut a = App::new(Arc::new(crate::catalog::fake_catalog_with_foreign()), "");
        a.selected = 1; // session "w" (older)
        assert_eq!(a.handle_key(key(KeyCode::Enter)), Action::None);
        assert_eq!(
            a.status.as_deref(),
            Some("this session lives in Windows — resume it there")
        );
    }
```

In `src/ui/render.rs` tests, update every `draw(f, app, 10_000, Path::new("/home/x"))` / `draw(f, &mut app, …)` call to drop the last argument, and add:

```rust
    #[test]
    fn foreign_rows_shorten_against_their_own_home() {
        let mut app = App::new(Arc::new(crate::catalog::fake_catalog_with_foreign()), "");
        let text: String = draw_to(&mut app, 100, 30)
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains(r"~\proj"));
        assert!(text.contains("win:c1"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test format:: ui::`
Expected: compile errors / assertion failures.

- [ ] **Step 3: Implement `shorten_home_in`** in `src/format.rs`:

```rust
/// Like `shorten_home`, but for a path in `env`'s own form: Windows paths compare
/// case-insensitively with `\` separators. An empty `home` leaves the path unchanged.
pub fn shorten_home_in(path: &Path, home: &Path, env: &crate::env::Env) -> String {
    if home.as_os_str().is_empty() {
        return path.display().to_string();
    }
    if !env.is_windows() {
        return shorten_home(path, home);
    }
    let p = path.to_string_lossy().replace('/', "\\");
    let h = home.to_string_lossy().replace('/', "\\");
    let h = h.trim_end_matches('\\');
    if p.len() >= h.len() && p.is_char_boundary(h.len()) && p[..h.len()].eq_ignore_ascii_case(h) {
        let rest = &p[h.len()..];
        if rest.is_empty() {
            return "~".into();
        }
        if rest.starts_with('\\') {
            return format!("~{rest}");
        }
    }
    p
}
```


- [ ] **Step 4: Update rendering** in `src/ui/render.rs`:

- `draw(frame: &mut Frame, app: &mut App, now_ms: i64)`; `draw_list(frame, app, area, now_ms)`; `draw_preview(frame, app, area)`.
- In `draw_list`, for a session row compute the launch source and use it:

```rust
                let source_idx = app.launch_source(*idx);
                let source = &catalog.sources[source_idx];
                let cwd = s
                    .meta
                    .cwd
                    .as_deref()
                    .map(|p| shorten_home_in(p, &source.env_home, &source.env))
                    .unwrap_or_else(|| "?".into());
```

use `source.name` in the detail line, and replace the dimming condition with:

```rust
                let cwd_missing = match catalog.host_cwd(*idx, source_idx) {
                    Some(p) => !p.is_dir(),
                    None => s.meta.cwd.is_none(),
                };
```

- In `draw_preview`, shorten the header cwd the same way with the launch source's `env_home`/`env`.
- In `src/ui/mod.rs`, remove the `home` variable and pass `render::draw(frame, app, now_ms())`; drop the `home` parameter of `event_loop`.

- [ ] **Step 5: Update `enter`** in `src/ui/app.rs` — after the running check, before the cwd match:

```rust
        let source = self.launch_source(idx);
        if !self.catalog.is_launchable(source) {
            self.status = Some(format!(
                "this session lives in {} — resume it there",
                self.catalog.sources[source].env.display_name()
            ));
            return Action::None;
        }
        match self.catalog.host_cwd(idx, source) {
            Some(cwd) if cwd.is_dir() => Action::Launch(self.catalog.launch_plan(idx, source)),
            Some(cwd) => {
                self.status = Some(format!("project dir no longer exists: {}", cwd.display()));
                Action::None
            }
            None => {
                self.status = Some("session has no recorded project dir".into());
                Action::None
            }
        }
```

(Keep the existing running-session block as is; it must come first.)

- [ ] **Step 6: Run tests**

Run: `cargo test`
Expected: all PASS.

- [ ] **Step 7: Check and commit**

```bash
mise check
git add -A
git commit -m "UI: per-environment path shortening and dimming; foreign sessions don't launch

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Phase 2 — Running detection

### Task 6: Process probe and `pidDomain`

**Files:**
- Create: `src/process.rs`
- Modify: `Cargo.toml` (Windows-only `windows-sys`), `src/lib.rs`, `src/model.rs`, `src/catalog.rs`, `src/providers/claude/live.rs`, `src/providers/claude/mod.rs`, `src/providers/fake.rs`

**Interfaces:**
- Consumes: `Env` (Task 1), `Source.env` (Task 3)
- Produces:
  - `process::PidDomain { Linux, Windows }` with `parse(&str) -> Option<PidDomain>` (`linux…` / `win32…` / `windows…`), `of(&Env) -> Option<PidDomain>`
  - `process::parse_tasklist_csv(&str) -> HashMap<u32, String>` (pid → image name)
  - `process::ProcessProbe` with `new(host: Env) -> ProcessProbe`, `prewarm_windows(&self)`, `is_running(&self, PidDomain, pid: u32, name: &str) -> bool`, `warnings(&self) -> Vec<String>`; `#[cfg(test)] with_windows_snapshot(host: Env, snapshot: Result<HashMap<u32, String>, String>) -> ProcessProbe`
  - `Provider::launch_records(&self, source: &Source, probe: &ProcessProbe) -> Vec<LaunchRecord>`
  - `providers::claude::live::launch_records(config_dir: &Path, env: &Env, probe: &ProcessProbe) -> Vec<LaunchRecord>` (the old `is_claude_process` is removed)

- [ ] **Step 1: Dependency and module**

```bash
cargo add windows-sys --target 'cfg(windows)' -F Win32_Foundation,Win32_System_Threading,Win32_System_Console
```

Add `pub mod process;` to `src/lib.rs`.

- [ ] **Step 2: Write failing tests** at the bottom of `src/process.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn ubuntu() -> Env {
        Env::Wsl {
            distro: "Ubuntu".into(),
        }
    }

    #[test]
    fn parses_pid_domains() {
        assert_eq!(PidDomain::parse("linux:abc:pid:[1]"), Some(PidDomain::Linux));
        assert_eq!(PidDomain::parse("win32:music3"), Some(PidDomain::Windows));
        assert_eq!(PidDomain::parse("darwin:x"), None);
        assert_eq!(PidDomain::of(&ubuntu()), Some(PidDomain::Linux));
        assert_eq!(PidDomain::of(&Env::Windows), Some(PidDomain::Windows));
        assert_eq!(PidDomain::of(&Env::MacOs), None);
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
            Ok(HashMap::from([(58892, "claude.exe".to_string()), (5, "notepad.exe".to_string())])),
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
    }
}
```

In `src/providers/claude/live.rs` tests, replace uses of `is_claude_process`/`launch_records(tmp.path())` with a Linux-host probe, and add a `pidDomain` test:

```rust
    use crate::env::Env;
    use crate::process::ProcessProbe;

    fn records(dir: &Path) -> Vec<LaunchRecord> {
        launch_records(dir, &Env::Linux, &ProcessProbe::new(Env::Linux))
    }

    #[test]
    fn windows_pid_domain_is_checked_against_windows_processes() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("sessions")).unwrap();
        fs::write(
            tmp.path().join("sessions/58892.json"),
            r#"{"pid":58892,"sessionId":"s","startedAt":1,"pidDomain":"win32:host"}"#,
        )
        .unwrap();
        let wsl = Env::Wsl { distro: "Ubuntu".into() };
        let probe = ProcessProbe::with_windows_snapshot(
            wsl.clone(),
            Ok(std::collections::HashMap::from([(58892, "claude.exe".to_string())])),
        );
        assert!(launch_records(tmp.path(), &Env::Windows, &probe)[0].alive);
        // Same record, but the host can't see Windows processes.
        assert!(!launch_records(tmp.path(), &Env::Windows, &ProcessProbe::new(Env::Linux))[0].alive);
    }
```

The live-process test that spawns a copy of `/bin/sleep` named `claude-fake` keeps working through `records(...)`; the non-claude test becomes `assert!(!ProcessProbe::new(Env::Linux).is_running(PidDomain::Linux, pid, "claude"))` (import `crate::process::PidDomain`).

In `src/catalog.rs` tests, update the fake provider's records API usage: nothing changes for `p.records` (the fake ignores the probe).

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test process:: live::`
Expected: compile errors.

- [ ] **Step 4: Implement `src/process.rs`** (above the tests):

```rust
//! "Is this process running?" for pids in Linux or Windows process namespaces. Agent-neutral.
use crate::env::Env;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

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
/// most once, optionally started early on a background thread.
pub struct ProcessProbe {
    host: Env,
    pending: Mutex<Option<JoinHandle<Snapshot>>>,
    snapshot: OnceLock<Snapshot>,
}

impl ProcessProbe {
    pub fn new(host: Env) -> ProcessProbe {
        ProcessProbe {
            host,
            pending: Mutex::new(None),
            snapshot: OnceLock::new(),
        }
    }

    #[cfg(test)]
    pub fn with_windows_snapshot(host: Env, snapshot: Snapshot) -> ProcessProbe {
        let probe = ProcessProbe::new(host);
        let _ = probe.snapshot.set(snapshot);
        probe
    }

    /// Starts the Windows process snapshot in the background. No-op except on WSL hosts.
    pub fn prewarm_windows(&self) {
        if !matches!(self.host, Env::Wsl { .. }) || self.snapshot.get().is_some() {
            return;
        }
        let mut pending = self.pending.lock().expect("probe lock");
        if pending.is_none() {
            *pending = Some(std::thread::spawn(take_tasklist_snapshot));
        }
    }

    fn windows_snapshot(&self) -> &Snapshot {
        self.snapshot.get_or_init(|| {
            let handle = self.pending.lock().expect("probe lock").take();
            match handle {
                Some(h) => h
                    .join()
                    .unwrap_or_else(|_| Err("tasklist.exe snapshot thread panicked".into())),
                None => take_tasklist_snapshot(),
            }
        })
    }

    pub fn is_running(&self, domain: PidDomain, pid: u32, name: &str) -> bool {
        if pid == 0 {
            return false;
        }
        match (domain, &self.host) {
            (PidDomain::Linux, Env::Linux | Env::Wsl { .. }) => linux_cmdline_contains(pid, name),
            (PidDomain::Windows, Env::Wsl { .. }) => match self.windows_snapshot() {
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

fn take_tasklist_snapshot() -> Snapshot {
    let output = std::process::Command::new("tasklist.exe")
        .args(["/FO", "CSV", "/NH"])
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
```

If the resolved `windows-sys` version names differ (e.g. `HANDLE` not a pointer, or `PROCESS_NAME_WIN32` in another module), adapt minimally and record it; verify with `cargo check --target x86_64-pc-windows-gnu`.

- [ ] **Step 5: Update `src/providers/claude/live.rs`** (replace `is_claude_process` and `launch_records`):

```rust
use crate::env::Env;
use crate::process::{PidDomain, ProcessProbe};

#[derive(Deserialize)]
struct Record {
    pid: i64,
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "startedAt", default)]
    started_at: i64,
    #[serde(rename = "pidDomain", default)]
    pid_domain: Option<String>,
}

/// Records from `<config_dir>/sessions/*.json`, with liveness from `probe`. A record without
/// `pidDomain` belongs to the source's environment.
pub fn launch_records(config_dir: &Path, env: &Env, probe: &ProcessProbe) -> Vec<LaunchRecord> {
    let Ok(entries) = std::fs::read_dir(config_dir.join("sessions")) else {
        return vec![];
    };
    let mut records: Vec<LaunchRecord> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| std::fs::read(p).ok())
        .filter_map(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
        .map(|r| {
            let domain = r
                .pid_domain
                .as_deref()
                .and_then(PidDomain::parse)
                .or_else(|| PidDomain::of(env));
            let alive = match (domain, u32::try_from(r.pid)) {
                (Some(domain), Ok(pid)) => probe.is_running(domain, pid, "claude"),
                _ => false,
            };
            LaunchRecord {
                pid: r.pid as i32,
                alive,
                session_id: r.session_id,
                started_at_ms: r.started_at,
            }
        })
        .collect();
    records.sort_by_key(|r| r.started_at_ms);
    records
}
```

- [ ] **Step 6: Trait and callers**

`src/model.rs`: `fn launch_records(&self, source: &Source, probe: &crate::process::ProcessProbe) -> Vec<LaunchRecord>;`
`src/providers/claude/mod.rs`: `live::launch_records(&source.config_dir, &source.env, probe)`.
`src/providers/fake.rs`: add the `_probe` parameter.

`src/catalog.rs` `build`: create the probe right after discovery, prewarm when any Windows source exists, and move the launch-records loop to **after** the scanning loop (so the snapshot runs while scanning):

```rust
        let probe = crate::process::ProcessProbe::new(settings.host.env.clone());
        if sources.iter().any(|s| s.env == crate::env::Env::Windows) {
            probe.prewarm_windows();
        }
```

Then restructure the rest of `build` so scanning happens before launch records (the snapshot runs meanwhile):

```rust
        // 1. Scan every store (cache hits + parallel scan of misses), remembering the store.
        let mut found: Vec<(usize, SessionMeta)> = Vec::new();
        for (store_idx, store) in stores.iter().enumerate() {
            let provider = &providers[store.provider];
            let agent = provider.id();
            let mut metas = Vec::new();
            let mut misses = Vec::new();
            for file in provider.list_session_files(&store.path) {
                let stamp = Stamp::of(&file);
                match stamp.and_then(|s| cache.get(agent, &file, s)) {
                    Some(meta) => metas.push(meta),
                    None => misses.push((file, stamp)),
                }
            }
            let scanned: Vec<(PathBuf, Option<Stamp>, SessionMeta)> = misses
                .into_par_iter()
                .filter_map(|(file, stamp)| provider.scan_file(&file).map(|m| (file, stamp, m)))
                .collect();
            for (file, stamp, meta) in scanned {
                if let Some(stamp) = stamp {
                    cache.put(agent, &file, stamp, meta.clone());
                }
                metas.push(meta);
            }
            found.extend(metas.into_iter().map(|meta| (store_idx, meta)));
        }

        // 2. Launch records, with liveness from the probe.
        let mut launches: HashMap<(usize, String), Vec<(i64, usize)>> = HashMap::new();
        let mut live: HashMap<(usize, String), (i32, usize)> = HashMap::new();
        for (si, source) in sources.iter().enumerate() {
            let pi = source_provider[si];
            for record in providers[pi].launch_records(source, &probe) {
                let k = (pi, record.session_id.clone());
                if record.alive {
                    live.insert(k.clone(), (record.pid, si));
                }
                launches
                    .entry(k)
                    .or_default()
                    .push((record.started_at_ms, si));
            }
        }

        // 3. Assemble sessions.
        let mut sessions = Vec::new();
        for (store_idx, meta) in found {
            let store = &stores[store_idx];
            let k = (store.provider, meta.id.clone());
            let default_source = launches
                .get(&k)
                .and_then(|l| {
                    l.iter()
                        .filter(|(_, si)| store.sources.contains(si))
                        .max_by_key(|(ts, _)| *ts)
                })
                .map(|(_, si)| *si)
                .unwrap_or(store.sources[0]);
            sessions.push(Session {
                live: live.get(&k).copied(),
                meta,
                provider: store.provider,
                sources: store.sources.clone(),
                default_source,
            });
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.meta.last_ts));
        warnings.extend(probe.warnings());
```

This replaces the previous records loop, scan loop and session assembly; the store-grouping code above it is unchanged.

- [ ] **Step 7: Run tests**

Run: `cargo test` and `cargo check --target x86_64-pc-windows-gnu --all-targets`
Expected: all tests PASS; Windows type-check succeeds (it may still fail only in `launch.rs` Unix-only code, fixed in Task 9 — report that error text if so, but no errors in `process.rs`).

- [ ] **Step 8: Smoke test (read-only)**

Run: `mise dev -- --list | awk -F'\t' '$2 ~ /^win:/'`
Expected: Windows sessions listed; a session whose Windows `claude.exe` is currently running shows its pid in column 3. Include output in the report.

- [ ] **Step 9: Check and commit**

```bash
mise check
git add -A
git commit -m "Shared process probe: pidDomain-aware liveness incl. Windows sessions from WSL

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Phase 3 — Foreign-session dialog

### Task 7: Resume command formatting and clipboard

**Files:**
- Create: `src/shell.rs`, `src/clipboard.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `Env` (Task 1), `LaunchPlan`
- Produces:
  - `shell::resume_command(plan: &LaunchPlan, env: &Env) -> String`
  - `shell::posix_quote(&str) -> String`, `shell::powershell_quote(&str) -> String`
  - `clipboard::copy(text: &str, host: &Env) -> Result<&'static str, String>` (Ok = how it was copied)
  - `clipboard::base64(&[u8]) -> String`, `clipboard::utf16le_with_bom(&str) -> Vec<u8>`

- [ ] **Step 1: Register modules** — add `pub mod clipboard;` and `pub mod shell;` to `src/lib.rs`.

- [ ] **Step 2: Write failing tests**

`src/shell.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn plan(cwd: &str, argv: &[&str], set: &[(&str, &str)], remove: &[&str]) -> LaunchPlan {
        LaunchPlan {
            cwd: PathBuf::from(cwd),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env_set: set.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            env_remove: remove.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn quotes_for_posix() {
        assert_eq!(posix_quote("c1"), "c1");
        assert_eq!(posix_quote("/home/me/proj"), "/home/me/proj");
        assert_eq!(posix_quote("has space"), "'has space'");
        assert_eq!(posix_quote("it's"), r"'it'\''s'");
        assert_eq!(posix_quote(""), "''");
    }

    #[test]
    fn quotes_for_powershell() {
        assert_eq!(powershell_quote("c2"), "c2");
        assert_eq!(powershell_quote(r"C:\x"), r"C:\x");
        assert_eq!(powershell_quote("a b"), "'a b'");
        assert_eq!(powershell_quote("it's"), "'it''s'");
    }

    #[test]
    fn posix_commands() {
        assert_eq!(
            resume_command(&plan("/home/me/proj", &["ccs", "c1", "--resume", "abc"], &[], &[]), &Env::Linux),
            "cd '/home/me/proj' && ccs c1 --resume abc"
        );
        assert_eq!(
            resume_command(
                &plan("/p", &["claude", "--resume", "abc"], &[], &["CLAUDE_CONFIG_DIR"]),
                &Env::Wsl { distro: "Ubuntu".into() }
            ),
            "cd '/p' && env -u CLAUDE_CONFIG_DIR claude --resume abc"
        );
        assert_eq!(
            resume_command(
                &plan("", &["claude", "--resume", "abc"], &[("CLAUDE_CONFIG_DIR", "/home/me/.claude work")], &[]),
                &Env::MacOs
            ),
            "CLAUDE_CONFIG_DIR='/home/me/.claude work' claude --resume abc"
        );
    }

    #[test]
    fn powershell_commands() {
        assert_eq!(
            resume_command(&plan(r"C:\Users\me\proj", &["ccs", "c2", "--resume", "abc"], &[], &[]), &Env::Windows),
            r"Set-Location 'C:\Users\me\proj'; ccs c2 --resume abc"
        );
        assert_eq!(
            resume_command(
                &plan(r"C:\p", &["claude", "--resume", "abc"], &[("CLAUDE_CONFIG_DIR", r"C:\Users\me\.claude")], &["X"]),
                &Env::Windows
            ),
            r"Set-Location 'C:\p'; Remove-Item Env:X -ErrorAction Ignore; $env:CLAUDE_CONFIG_DIR='C:\Users\me\.claude'; claude --resume abc"
        );
        assert_eq!(
            resume_command(&plan("", &[r"C:\Program Files\c.exe", "--resume", "a"], &[], &[]), &Env::Windows),
            r"& 'C:\Program Files\c.exe' --resume a"
        );
    }
}
```

`src/clipboard.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected);
        }
    }

    #[test]
    fn utf16le_has_bom() {
        assert_eq!(utf16le_with_bom("A→"), vec![0xFF, 0xFE, 0x41, 0x00, 0x92, 0x21]);
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test shell:: clipboard::`
Expected: compile errors.

- [ ] **Step 4: Implement `src/shell.rs`** (above the tests):

```rust
//! A LaunchPlan as a command the user can paste into a shell in the session's environment.
use crate::env::Env;
use crate::model::LaunchPlan;

pub fn resume_command(plan: &LaunchPlan, env: &Env) -> String {
    if env.is_windows() {
        powershell(plan)
    } else {
        posix(plan)
    }
}

fn is_plain(s: &str, extra: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

/// Single-quotes unless the string only has characters no POSIX shell treats specially.
pub fn posix_quote(s: &str) -> String {
    if is_plain(s, "_./:=@%+-") {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Single-quotes unless the string only has characters PowerShell treats literally.
pub fn powershell_quote(s: &str) -> String {
    if is_plain(s, r"_.:\/-") {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

fn posix(plan: &LaunchPlan) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !plan.env_remove.is_empty() {
        parts.push("env".into());
        for key in &plan.env_remove {
            parts.push(format!("-u {key}"));
        }
    }
    for (key, value) in &plan.env_set {
        parts.push(format!("{key}={}", posix_quote_always(value)));
    }
    parts.extend(plan.argv.iter().map(|a| posix_quote(a)));
    let command = parts.join(" ");
    if plan.cwd.as_os_str().is_empty() {
        command
    } else {
        format!("cd {} && {command}", posix_quote_always(&plan.cwd.to_string_lossy()))
    }
}

fn posix_quote_always(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn powershell_quote_always(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn powershell(plan: &LaunchPlan) -> String {
    let mut statements: Vec<String> = Vec::new();
    if !plan.cwd.as_os_str().is_empty() {
        statements.push(format!(
            "Set-Location {}",
            powershell_quote_always(&plan.cwd.to_string_lossy())
        ));
    }
    for key in &plan.env_remove {
        statements.push(format!("Remove-Item Env:{key} -ErrorAction Ignore"));
    }
    for (key, value) in &plan.env_set {
        statements.push(format!("$env:{key}={}", powershell_quote_always(value)));
    }
    let mut argv: Vec<String> = plan.argv.iter().map(|a| powershell_quote(a)).collect();
    if let Some(first) = argv.first_mut()
        && first.starts_with('\'')
    {
        *first = format!("& {first}");
    }
    statements.push(argv.join(" "));
    statements.join("; ")
}
```

(Note: `posix_quote` keeps `:` plain, so the `win:c1`-style names in fake tests stay unquoted.)

- [ ] **Step 5: Implement `src/clipboard.rs`** (above the tests):

```rust
//! Copy text to the system clipboard with whatever the host offers, falling back to OSC 52.
use crate::env::Env;
use std::io::Write;
use std::process::{Command, Stdio};

struct Tool {
    program: &'static str,
    args: &'static [&'static str],
    utf16: bool,
}

/// Returns a short description of how the text was copied.
pub fn copy(text: &str, host: &Env) -> Result<&'static str, String> {
    let tools: &[Tool] = match host {
        Env::Wsl { .. } | Env::Windows => &[Tool { program: "clip.exe", args: &[], utf16: true }],
        Env::MacOs => &[Tool { program: "pbcopy", args: &[], utf16: false }],
        Env::Linux => &[
            Tool { program: "wl-copy", args: &[], utf16: false },
            Tool { program: "xclip", args: &["-selection", "clipboard"], utf16: false },
            Tool { program: "xsel", args: &["-b", "-i"], utf16: false },
        ],
    };
    for tool in tools {
        let input = if tool.utf16 {
            utf16le_with_bom(text)
        } else {
            text.as_bytes().to_vec()
        };
        if pipe_to(tool.program, tool.args, &input) {
            return Ok(tool.program);
        }
    }
    osc52(text)
        .map(|_| "terminal")
        .map_err(|e| format!("could not copy: {e}"))
}

fn pipe_to(program: &str, args: &[&str], input: &[u8]) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(input).is_ok());
    child.wait().is_ok_and(|status| status.success()) && wrote
}

fn osc52(text: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()
}

pub fn utf16le_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend(unit.to_le_bytes());
    }
    bytes
}

pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = match chunk.len() {
            1 => (chunk[0] as u32) << 16,
            2 => ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8),
            _ => ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | chunk[2] as u32,
        };
        let symbols = chunk.len() + 1;
        for i in 0..4 {
            if i < symbols {
                out.push(TABLE[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
```

- [ ] **Step 6: Run tests**

Run: `cargo test shell:: clipboard::`
Expected: 6 tests PASS.

- [ ] **Step 7: Check and commit**

```bash
mise check
git add -A
git commit -m "Shell-specific resume commands and clipboard copy

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 8: Resume dialog in the TUI

**Files:**
- Modify: `src/ui/app.rs`, `src/ui/render.rs`, `src/ui/mod.rs`

**Interfaces:**
- Consumes: `shell::resume_command`, `clipboard::copy` (Task 7); `Catalog::{is_launchable, host_cwd, host}` (Task 4); `fake_catalog_with_foreign` (Task 4)
- Produces:
  - `app::ResumeDialog { session: usize, source: usize, command: String, dir_missing: bool, note: Option<String> }` (`Debug, Clone, PartialEq`)
  - `App.dialog: Option<ResumeDialog>`
  - `Action::Copy(String)`
  - `App::set_copy_result(&mut self, Result<&'static str, String>)` — note becomes `copied (<how>)` or the error

- [ ] **Step 1: Write failing tests**

In `src/ui/app.rs` tests, replace `enter_on_foreign_session_does_not_launch` with:

```rust
    fn foreign_app() -> App {
        let mut a = App::new(Arc::new(crate::catalog::fake_catalog_with_foreign()), "");
        a.selected = 1; // session "w"
        a
    }

    #[test]
    fn enter_on_foreign_session_opens_resume_dialog() {
        let mut a = foreign_app();
        assert_eq!(a.handle_key(key(KeyCode::Enter)), Action::None);
        let d = a.dialog.clone().expect("dialog open");
        assert_eq!(
            d.command,
            r"Set-Location 'C:\Users\me\proj'; fake win:c1 --resume w"
        );
        assert!(d.dir_missing);
        assert_eq!(d.note, None);
    }

    #[test]
    fn dialog_keys_copy_close_and_capture_input() {
        let mut a = foreign_app();
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.handle_key(key(KeyCode::Char('x'))), Action::None);
        assert_eq!(a.query, "");
        let command = a.dialog.as_ref().unwrap().command.clone();
        assert_eq!(a.handle_key(key(KeyCode::Char('c'))), Action::Copy(command));
        a.set_copy_result(Ok("clip.exe"));
        assert_eq!(a.dialog.as_ref().unwrap().note.as_deref(), Some("copied (clip.exe)"));
        a.set_copy_result(Err("could not copy: nope".into()));
        assert_eq!(a.dialog.as_ref().unwrap().note.as_deref(), Some("could not copy: nope"));
        assert_eq!(a.handle_key(key(KeyCode::Esc)), Action::None);
        assert!(a.dialog.is_none());
        a.handle_key(key(KeyCode::Enter));
        assert_eq!(a.handle_key(ctrl('c')), Action::Quit);
    }

    #[test]
    fn native_session_in_wsl_host_still_launches() {
        let mut a = App::new(Arc::new(crate::catalog::fake_catalog_with_foreign()), "");
        a.selected = 0; // session "n", cwd /tmp
        assert!(matches!(a.handle_key(key(KeyCode::Enter)), Action::Launch(_)));
        assert!(a.dialog.is_none());
    }
```

In `src/ui/render.rs` tests:

```rust
    #[test]
    fn renders_resume_dialog() {
        let mut app = App::new(Arc::new(crate::catalog::fake_catalog_with_foreign()), "");
        app.selected = 1;
        app.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Enter,
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        let text: String = draw_to(&mut app, 100, 30)
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Resume in Windows"));
        assert!(text.contains("Paste into PowerShell"));
        assert!(text.contains("Set-Location"));
        assert!(text.contains("no longer exists"));
        assert!(text.contains("c copy"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ui::`
Expected: compile errors.

- [ ] **Step 3: Implement dialog state** in `src/ui/app.rs`

Add `Copy(String)` to `Action`. Add the struct:

```rust
/// Shown instead of launching when a session belongs to another environment.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeDialog {
    pub session: usize,
    pub source: usize,
    pub command: String,
    pub dir_missing: bool,
    /// Result of the last copy attempt.
    pub note: Option<String>,
}
```

Add `pub dialog: Option<ResumeDialog>,` to `App` (init `None`). Add methods:

```rust
    pub fn set_copy_result(&mut self, result: Result<&'static str, String>) {
        if let Some(dialog) = &mut self.dialog {
            dialog.note = Some(match result {
                Ok(how) => format!("copied ({how})"),
                Err(e) => e,
            });
        }
    }

    fn open_dialog(&mut self, idx: usize) {
        let source = self.launch_source(idx);
        let plan = self.catalog.launch_plan(idx, source);
        let command = crate::shell::resume_command(&plan, &self.catalog.sources[source].env);
        let dir_missing = self
            .catalog
            .host_cwd(idx, source)
            .is_some_and(|p| !p.is_dir());
        self.dialog = Some(ResumeDialog {
            session: idx,
            source,
            command,
            dir_missing,
            note: None,
        });
    }

    fn handle_dialog_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('c'), true) => Action::Quit,
            (KeyCode::Esc, _) => {
                self.dialog = None;
                Action::None
            }
            (KeyCode::Char('c'), false) => match &self.dialog {
                Some(d) => Action::Copy(d.command.clone()),
                None => Action::None,
            },
            (KeyCode::Char('a'), true) => {
                if let Some(idx) = self.dialog.as_ref().map(|d| d.session) {
                    self.cycle_source();
                    self.open_dialog(idx);
                }
                Action::None
            }
            _ => Action::None,
        }
    }
```

At the top of `handle_key`: `if self.dialog.is_some() { return self.handle_dialog_key(key); }`.

In `enter`, replace the Task 5 non-launchable status block with:

```rust
        if !self.catalog.is_launchable(source) {
            self.open_dialog(idx);
            return Action::None;
        }
```

- [ ] **Step 4: Render the dialog** in `src/ui/render.rs`

Import `ratatui::widgets::Clear`. At the end of `draw`, add `if app.dialog.is_some() { draw_dialog(frame, app); }` and:

```rust
fn draw_dialog(frame: &mut Frame, app: &App) {
    let Some(dialog) = &app.dialog else {
        return;
    };
    let source = &app.catalog.sources[dialog.source];
    let area = frame.area();
    let width = area.width.saturating_sub(4).clamp(20, 90).min(area.width);
    let mut lines = vec![
        Line::from(format!(
            "This session lives in {}. Paste into {}:",
            source.env.display_name(),
            source.env.shell_name()
        )),
        Line::from(""),
        Line::from(Span::styled(
            dialog.command.clone(),
            Style::new().fg(Color::Cyan),
        )),
    ];
    if dialog.dir_missing {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Note: the project directory no longer exists.",
            Style::new().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(""));
    lines.push(match &dialog.note {
        Some(note) => Line::from(Span::styled(note.clone(), Style::new().fg(Color::Yellow))),
        None => Line::from(" c copy   ^A source   esc close").dim(),
    });
    let inner_width = width.saturating_sub(4).max(1);
    let content_height = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(inner_width) as u16;
    let height = content_height.saturating_add(2).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " Resume in {} · {} ",
                    source.env.display_name(),
                    source.name
                ))
                .padding(Padding::horizontal(1)),
        ),
        rect,
    );
}
```

- [ ] **Step 5: Handle the copy action** in `src/ui/mod.rs` `event_loop`:

```rust
            Action::Copy(text) => {
                let result = crate::clipboard::copy(&text, &app.catalog.host.env);
                app.set_copy_result(result);
            }
```

- [ ] **Step 6: Run tests**

Run: `cargo test`
Expected: all PASS.

- [ ] **Step 7: Check and commit**

```bash
mise check
git add -A
git commit -m "Resume dialog with copyable command for sessions in the other environment

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

## Phase 4 — Native Windows

### Task 9: Cross-platform launch

**Files:**
- Modify: `src/launch.rs`, `src/main.rs`, `src/ui/mod.rs`

**Interfaces:**
- Consumes: `LaunchPlan`
- Produces:
  - `#[cfg(unix)] launch::exec(&LaunchPlan) -> std::io::Error` (unchanged)
  - `#[cfg(windows)] launch::run_and_wait(&LaunchPlan) -> std::io::Result<i32>`
  - `launch::find_in_path(&str) -> Option<PathBuf>` (PATHEXT-aware on Windows)
  - `launch::resolve(program: &str, dirs: impl IntoIterator<Item = PathBuf>, exts: &[String], usable: impl Fn(&Path) -> bool) -> Option<PathBuf>`
  - `launch::windows_exts(program: &str, pathext: Option<&str>) -> Vec<String>`
  - The TUI replaces `plan.argv[0]` with the resolved path before returning the plan.

- [ ] **Step 1: Write failing tests** in `src/launch.rs` tests:

```rust
    use std::path::Path;

    #[test]
    fn windows_exts_use_pathext_unless_program_has_extension() {
        assert_eq!(windows_exts("ccs", Some(".COM;.EXE;.CMD;")), vec![".com", ".exe", ".cmd"]);
        assert_eq!(windows_exts("ccs", None), vec![".com", ".exe", ".bat", ".cmd"]);
        assert_eq!(windows_exts("ccs.cmd", Some(".EXE")), vec![""]);
    }

    #[test]
    fn resolves_first_usable_candidate_in_dir_order() {
        let usable = |p: &Path| {
            let s = p.to_string_lossy().replace('\\', "/");
            s == "/b/ccs.cmd" || s == "/c/ccs.exe" || s == "/a/ccs"
        };
        let dirs = || vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")];
        let exts = vec![".exe".to_string(), ".cmd".to_string()];
        // The extensionless npm shim in /a is ignored when extensions are required.
        assert_eq!(
            resolve("ccs", dirs(), &exts, usable).map(|p| p.to_string_lossy().replace('\\', "/")),
            Some("/b/ccs.cmd".to_string())
        );
        assert_eq!(
            resolve("ccs", dirs(), &[String::new()], usable).map(|p| p.to_string_lossy().replace('\\', "/")),
            Some("/a/ccs".to_string())
        );
        assert_eq!(resolve("/b/ccs.cmd", dirs(), &[String::new()], usable), Some(PathBuf::from("/b/ccs.cmd")));
        assert_eq!(resolve("missing", dirs(), &exts, usable), None);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test launch::`
Expected: compile errors.

- [ ] **Step 3: Implement `src/launch.rs`** (replace everything above the tests):

```rust
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

/// Runs the agent in this console and returns its exit code. Ctrl-C goes to the agent, not
/// ccpick, while it runs.
#[cfg(windows)]
pub fn run_and_wait(plan: &LaunchPlan) -> std::io::Result<i32> {
    // SAFETY: a null handler with TRUE makes this process ignore Ctrl-C; the child still gets it.
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(None, 1);
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
```

Mark the existing `finds_programs_on_path` test `#[cfg(unix)]`.

- [ ] **Step 4: Use the resolved path** in `src/ui/mod.rs`:

```rust
            Action::Launch(mut plan) => match find_in_path(&plan.argv[0]) {
                Some(path) => {
                    plan.argv[0] = path.display().to_string();
                    return Ok(Some(plan));
                }
                None => app.status = Some(format!("{} not found on PATH", plan.argv[0])),
            },
```

- [ ] **Step 5: Platform launch in `src/main.rs`** — replace the `Some(plan) => { … }` arm with `Some(plan) => launch_session(&plan),` and add:

```rust
#[cfg(unix)]
fn launch_session(plan: &ccpick::model::LaunchPlan) -> Result<i32> {
    let err = launch::exec(plan);
    anyhow::bail!("failed to launch {}: {err}", plan.argv.join(" "))
}

#[cfg(windows)]
fn launch_session(plan: &ccpick::model::LaunchPlan) -> Result<i32> {
    launch::run_and_wait(plan)
        .map_err(|err| anyhow::anyhow!("failed to launch {}: {err}", plan.argv.join(" ")))
}
```

- [ ] **Step 6: Verify both platforms compile**

Run: `cargo test` then `cargo check --target x86_64-pc-windows-gnu --all-targets`
Expected: tests PASS; Windows check succeeds with no errors or warnings.

- [ ] **Step 7: Check and commit**

```bash
mise check
git add -A
git commit -m "Cross-platform launch: exec on Unix, spawn-and-wait with PATHEXT on Windows

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 10: Run the test suite as Windows binaries

**Files:**
- Create: `src/testutil.rs`
- Modify: `src/lib.rs`, `mise.toml`, test modules that use `env!("CARGO_MANIFEST_DIR")` or Unix-only facilities

**Interfaces:**
- Produces:
  - `#[cfg(test)] testutil::manifest_dir() -> PathBuf` (runtime `CARGO_MANIFEST_DIR`, falling back to compile time)
  - mise tasks `test-windows` and `lint-windows` (WSL with `x86_64-w64-mingw32-gcc` and the `x86_64-pc-windows-gnu` rustup target)

- [ ] **Step 1: Test helper** — `src/testutil.rs`:

```rust
//! Test-only helpers.
use std::path::PathBuf;

/// The repository root while tests run. Prefers the runtime `CARGO_MANIFEST_DIR` so Windows
/// test binaries started from WSL (with `WSLENV=CARGO_MANIFEST_DIR/p`) get a Windows path.
pub fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}
```

`src/lib.rs`: `#[cfg(test)] pub mod testutil;`. Replace every `PathBuf::from(env!("CARGO_MANIFEST_DIR"))` / `Path::new(env!("CARGO_MANIFEST_DIR"))` in the crate with `crate::testutil::manifest_dir()` (`grep -rn CARGO_MANIFEST_DIR src`).

- [ ] **Step 2: mise tasks** — append to `mise.toml`:

```toml
[tasks.test-windows]
description = "WSL only: build tests for Windows (mingw) and run them via interop"
run = 'WSLENV="CARGO_MANIFEST_DIR/p${WSLENV:+:$WSLENV}" cargo test --target x86_64-pc-windows-gnu'

[tasks.lint-windows]
description = "Clippy for the Windows target (warnings as errors)"
run = "cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings"
```

- [ ] **Step 3: Run the Windows suite and fix failures**

Run: `mise test-windows`
Expected at first: some failures. Fix each by category, recording every change in the report:
- **Unix-only facilities** (`std::os::unix`, `/bin/sleep`, `/proc`, Unix symlinks, executable bits): mark the test `#[cfg(unix)]` (or `#[cfg(target_os = "linux")]` for `/proc`). Don't weaken the assertion.
- **Path formatting** (expected strings with `/` that are now `\`): compare `Path`/`PathBuf` values instead of strings, or build expectations with `Path::join`.
- **Canonical path prefixes:** use `dunce::canonicalize` in tests too.
- **Real logic bugs on Windows** (not test-only): fix the code and add or adjust a test that pins the fix.
Never skip or `#[ignore]` a test to make the suite pass.

Run again until: `mise test-windows` → all PASS, and `mise lint-windows` → no warnings.

- [ ] **Step 4: Linux still green**

Run: `mise check`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "Test suite runs as Windows binaries (mise test-windows)

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 11: Release targets, CI and docs

**Files:**
- Modify: `dist-workspace.toml`, `.github/workflows/release.yml` (generated), `.github/workflows/ci.yml`, `README.md`, `AGENTS.md`, `docs/specs/2026-09-17-windows-support-design.md`

- [ ] **Step 1: dist** — in `dist-workspace.toml` set:

```toml
installers = ["shell", "powershell"]
targets = ["aarch64-apple-darwin", "aarch64-unknown-linux-gnu", "x86_64-apple-darwin", "x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"]
```

Run: `mise exec -- dist generate` then `mise exec -- dist plan`
Expected: plan lists `ccpick-installer.ps1` and `ccpick-x86_64-pc-windows-msvc.zip`.

- [ ] **Step 2: CI** — in `.github/workflows/ci.yml` set `os: [ubuntu-latest, macos-latest, windows-latest]`.

- [ ] **Step 3: README** — in "Install", replace the "Native Windows isn't supported…" paragraph with:

````markdown
Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/outsharked/ccpick/releases/latest/download/ccpick-installer.ps1 | iex"
```

On macOS, sessions are listed and resumable but never shown as running.
````

Add a section after "Sources":

````markdown
## Windows and WSL

On a Windows machine with WSL, ccpick also lists the other side's sessions:

- In WSL, Windows users' Claude Code data under `/mnt/c/Users/<user>` appears as `win:<name>` sources.
- On Windows, sessions in *running* WSL distros appear as `wsl:<name>` (or `<distro>:<name>` with several distros). Stopped distros aren't started.

Those sessions are searchable like any other. Pressing Enter on one opens a dialog with the command to paste into a shell on the other side (`c` copies it).

```toml
[environments]
auto = true                # set false to only scan the native environment
wsl_distros = ["Ubuntu"]   # Windows only: also scan these distros when stopped (boots them)
```

A `[[source]]` may point at the other side's path; its environment is inferred, or set `env = "windows"` / `env = "wsl:<distro>"`.
````

Add to the Development task list: `mise test-windows` and `mise lint-windows` (WSL with the mingw toolchain and `rustup target add x86_64-pc-windows-gnu`).

- [ ] **Step 4: AGENTS.md** — replace the Releases bullet "Targets: … Native Windows is not supported (launch relies on Unix `exec`); don't re-add it without that work." with:

```markdown
- Targets: Linux x86_64/aarch64, macOS x86_64/aarch64, Windows x86_64; shell and PowerShell
  installers.
```

Add `| mise test-windows | WSL only: run the test suite as Windows binaries via interop |` and `| mise lint-windows | Clippy for the Windows target |` to the task table, and a new section before "Testing":

```markdown
## Windows and WSL

- Environment logic (`src/env.rs`, `src/homes.rs`, `src/process.rs`, `src/shell.rs`,
  `src/clipboard.rs`) is agent-neutral; keep Claude specifics in `src/providers/claude/`.
- Windows-only code behind `cfg(windows)`, Unix-only behind `cfg(unix)`. After touching either,
  run `mise lint-windows` and `mise test-windows` as well as `mise check`.
- Canonicalize with `dunce::canonicalize`. Compare Windows paths case-insensitively.
- Never access a stopped WSL distro (`\\wsl.localhost\<distro>` boots it) unless it's listed in
  `[environments] wsl_distros`.
- Commands shown for another environment use that environment's own path form and shell syntax
  (PowerShell on Windows, POSIX elsewhere).
```

and remove "Linux-first: … Don't add the `nix` crate." in favour of: "- **Platforms:** process liveness goes through `src/process.rs`; launching uses `exec` on Unix and spawn-and-wait on Windows (`src/launch.rs`). Don't add the `nix` crate."

- [ ] **Step 5: Spec deviation** — in `docs/specs/2026-09-17-windows-support-design.md`, in the dialog section's copy bullet, change "Windows host → Win32 clipboard API" to "Windows host → `clip.exe` (UTF-16LE)".

- [ ] **Step 6: Check and commit**

```bash
mise check
git add -A
git commit -m "Windows release target, PowerShell installer, Windows CI; docs

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>"
```

---

### Task 12: Acceptance and performance

**Files:** none (report only), unless a check fails — then fix, test, and commit the fix.

- [ ] **Step 1: All suites**

Run: `mise check`, `mise test-windows`, `mise lint-windows`
Expected: all PASS, no warnings.

- [ ] **Step 2: WSL host (read-only)**

Run: `mise dev -- --sources` and `mise dev -- --list | awk -F'\t' '{print $2}' | sort | uniq -c`
Expected: native sources plus `win:` sources (env `windows`); Windows ccs accounts sharing a store are one store; session counts per source are plausible. Pick a currently running Windows `claude.exe` session from `tasklist.exe /FI "IMAGENAME eq claude.exe"` and confirm its row shows the pid.

- [ ] **Step 3: Windows host via interop (read-only)**

```bash
cargo build --release --target x86_64-pc-windows-gnu
./target/x86_64-pc-windows-gnu/release/ccpick.exe --sources
./target/x86_64-pc-windows-gnu/release/ccpick.exe --list | head
```

Expected: native Windows sources (env `windows`, `C:\…` config dirs) plus `wsl:` sources from the running distro (env `wsl:<distro>`, `\\wsl.localhost\…` paths); WSL sessions listed. Never run it without `--sources`/`--list`.

- [ ] **Step 4: Performance**

```bash
time mise dev -- --list > /dev/null        # warm, WSL host with Windows sources: target < 300 ms
time ./target/release/ccpick --list > /dev/null   # after `mise build`
time ./target/x86_64-pc-windows-gnu/release/ccpick.exe --list > /dev/null
```

Record the timings (3 runs each for the Linux release binary). If the WSL-host warm start exceeds 300 ms, report it rather than tuning silently.

- [ ] **Step 5: Report**

List results of every step, plus the manual checks that remain for the user (TUI needs a real terminal):
- WSL TUI: Windows session shows `●`/`[running]` and Enter is blocked; Enter on another Windows session opens the dialog; `c` puts a working PowerShell command on the Windows clipboard; `^A` switches `win:` accounts in the dialog.
- Windows TUI (PowerShell): native resume through `ccs.cmd`; Ctrl-C inside the resumed Claude doesn't kill it via ccpick; WSL sessions appear while the distro runs and not after `wsl --shutdown` (and ccpick doesn't restart it).
