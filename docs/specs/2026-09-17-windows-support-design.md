# Windows support and cross-environment sources

Date: 2026-09-17
Status: Draft, pending review
Issue: https://github.com/outsharked/ccpick/issues/1
Builds on: `docs/specs/2026-09-16-ccpick-design.md`

## Goals

1. **Cross-environment sources.** Find, list and search sessions in the *other* environment of a
   Windows + WSL machine: Windows Claude Code data when running in WSL, and WSL data when running
   on Windows. Discovered automatically.
2. **Native Windows.** ccpick runs in PowerShell/cmd, finds Windows sessions (including ccs
   accounts when ccs is installed) and resumes them.

Selecting a session that belongs to the other environment never launches it. ccpick shows a dialog
with the exact command to paste into a shell in that environment.

Non-goals: launching Claude across the WSL/Windows boundary; cmd.exe command syntax (PowerShell
only); running-session detection for WSL sessions when running on Windows; Windows on ARM builds.

## Environment model

New agent-neutral module `src/env.rs` (no Claude/ccs knowledge).

```rust
pub enum Env {
    Linux,                    // native Linux, not WSL
    Wsl { distro: String },
    Windows,
    MacOs,
}

pub struct Home {
    pub env: Env,
    /// The home directory as ccpick can read it (host form),
    /// e.g. /mnt/c/Users/jamie from WSL, \\wsl.localhost\Ubuntu\home\me from Windows.
    pub dir: PathBuf,
    /// The same directory as its own environment sees it, e.g. C:\Users\jamie, /home/me.
    pub env_dir: PathBuf,
    /// Prefix for source names; None for the native home.
    pub label: Option<String>,
}
```

**Host environment detection:** `cfg(windows)` → `Windows`; `cfg(target_os = "macos")` → `MacOs`;
Linux with `WSL_DISTRO_NAME` set, or `/proc/sys/fs/binfmt_misc/WSLInterop` present, → `Wsl`
(distro from `WSL_DISTRO_NAME`, or `"wsl"` if unset); otherwise `Linux`.

### Home discovery (shared code)

Always: the native home (`dirs::home_dir()`, `env` = host, `label` = None).

**Host is WSL:** Windows user profiles.
- Drive root: `[automount] root` in `/etc/wsl.conf` if set, else `/mnt/`.
- Every `<root>c/Users/<name>` directory containing `.claude` or `.ccs`, skipping `Public`,
  `Default`, `Default User`, `All Users`. No subprocess.
- `env` = `Windows`, `env_dir` = `C:\Users\<name>`, `label` = `win`.

**Host is Windows:** running WSL distros.
- `wsl.exe -l --running -q` (output is UTF-16LE with a BOM on some builds; decode accordingly,
  trim NULs and whitespace). Distros listed in `[environments] wsl_distros` are included even if
  not running.
- Stopped distros are never accessed unless listed in `wsl_distros` (touching
  `\\wsl.localhost\<distro>` boots the VM, so listing one is an explicit opt-in to that).
- For each distro: `\\wsl.localhost\<distro>\root` and every `\\wsl.localhost\<distro>\home\<user>`
  containing `.claude` or `.ccs`.
- `env` = `Wsl { distro }`, `env_dir` = `/home/<user>` (or `/root`), `label` = `wsl` if exactly one
  distro contributes homes, else the distro name.
- If `wsl.exe` is missing or fails: no WSL homes, one warning.

**Config** (`~/.config/ccpick/config.toml`, `%APPDATA%\ccpick\config.toml` on Windows):

```toml
[environments]
auto = true                   # discover the other environment's homes
wsl_distros = ["Ubuntu"]      # Windows host only: include even when not running
```

## Sources

`Provider::discover_sources(&self, settings, home: &Home)` is called once per home. The Claude
provider does exactly what it does today, rooted at `home.dir`: ccs accounts only if
`<home>/.ccs/config.yaml` exists, then `<home>/.claude`.

`$CLAUDE_CONFIG_DIR`, `[[source]]` entries and `--config-dir` apply only to the native home's
pass. A configured path in another environment's form gets that environment inferred:
`/mnt/<drive>/…` (from a WSL host) → `Windows`; `\\wsl.localhost\<distro>\…` or `\\wsl$\<distro>\…`
(from a Windows host) → `Wsl { distro }`. An explicit `env = "windows"` / `env = "wsl:<distro>"`
overrides inference.

`Source` gains:

```rust
pub env: Env,
/// config_dir as its own environment sees it (used in commands shown to the user).
pub env_config_dir: PathBuf,
```

- Foreign sources are named `<label>:<name>` (`win:c2`, `wsl:c1`).
- Dedupe stays by canonical host path.
- The Claude provider builds `LaunchSpec` env values (`CLAUDE_CONFIG_DIR`) from `env_config_dir`,
  so commands are correct in the session's own environment. For native sources
  `env_config_dir == config_dir`.
- A session is **launchable** iff its launch source's `env` equals the host env.

## Path translation (`src/env.rs`)

Pure functions, unit-tested on every platform:

| From → to | Example |
|---|---|
| Windows path → WSL host path | `C:\Users\me\proj` → `/mnt/c/Users/me/proj` (drive letter lowercased, root from wsl.conf) |
| WSL path → Windows host path | `/home/me/proj` → `\\wsl.localhost\<distro>\home\me\proj` |
| and the reverse of each | used to derive `env_dir` / `env_config_dir` |

Windows path comparison (prefix stripping for `~` shortening, dedupe, translation) is
case-insensitive; Windows data mixes `c:\Users` and `C:\Users`.

Used for: the missing-project-dir check on foreign sessions, `~` shortening per environment
(a Windows session's cwd is shortened against its own home), and `env_config_dir`.

## Running-session detection

Session records (`sessions/<pid>.json`) carry `pidDomain`: `linux:<machine-id>:pid:[ns]` or
`win32:<host>`. The Claude provider parses records (pid, session id, startedAt, pidDomain) as today;
liveness moves to a shared, agent-neutral probe (`src/process.rs`):

```rust
pub enum PidDomain { Linux, Windows }
pub trait ProcessProbe { fn is_running(&self, domain: PidDomain, pid: u32, name: &str) -> bool; }
```

`LaunchRecord` stays `{pid, session_id, started_at_ms, alive}`; the provider asks the probe for
`alive`, passing `name = "claude"`.

| Record domain | Host | Check |
|---|---|---|
| Linux | Linux / WSL | `/proc/<pid>/cmdline` contains the name (current behaviour) |
| Windows | WSL | pid present in a snapshot from `tasklist.exe /FO CSV /NH /FI "IMAGENAME eq claude.exe"` |
| Windows | Windows | `OpenProcess` + `QueryFullProcessImageNameW` (`windows-sys`), image path contains the name |
| Linux | Windows | not checked → not running |
| any | macOS | not checked → not running |
| missing | any | domain taken from the source's `env` |

The WSL `tasklist.exe` snapshot is taken at most once per launch, only if some record has a Windows
domain, on a background thread started alongside scanning. Failure (no interop, non-zero exit) →
Windows sessions show no badge, one warning. CSV parsing is a pure, tested function.

Enter on any running session — native or foreign — is blocked with `running in <source> (pid N)`.

## Foreign-session dialog

Enter on a non-running session whose launch source is not launchable opens a centered modal:

```
┌ Resume in Windows · win:c2 ───────────────────────────────┐
│ This session lives in Windows. Paste into PowerShell:     │
│                                                           │
│ Set-Location 'C:\Users\jamie\proj'; ccs c2 --resume 0d42… │
│                                                           │
│ c copy   ^A source   esc close                            │
└───────────────────────────────────────────────────────────┘
```

- The command is always shown in full (wrapped).
- If the project dir doesn't exist (translated check), an extra warning line says so; the command
  is still shown.
- `Ctrl-A` cycles the session's sources and regenerates the command. `Esc` closes. Other keys are
  ignored while the dialog is open.
- `c` copies: WSL host → `clip.exe` (text passed as UTF-16LE); Windows host → `clip.exe`
  (UTF-16LE); Linux → `wl-copy`, else `xclip -selection clipboard`, else `xsel -b`;
  macOS → `pbcopy`; if none succeed → OSC 52 escape sequence. The footer shows `copied` or the
  failure.

### Command formatting (shared, from the neutral `LaunchPlan` + target `Env`)

| Target env | Shell | Form |
|---|---|---|
| Linux / WSL / macOS | bash/zsh | `cd '<cwd>' && [env -u VAR] [VAR='v'] <argv…>` |
| Windows | PowerShell | `Set-Location '<cwd>'; [Remove-Item Env:VAR -ErrorAction Ignore; ] [$env:VAR='v'; ] <argv…>` |

- POSIX quoting: single quotes, `'` → `'\''`. Arguments that are plain (`[A-Za-z0-9_./:=@%+-]`)
  are left unquoted.
- PowerShell quoting: single quotes, `'` → `''`. The first argument is prefixed with `& ` only when
  it needs quoting.
- `LaunchPlan.cwd` for a foreign session is the session's cwd in its own form (not translated).

## Native Windows

- **Launch:** no `exec`. Restore the terminal, resolve `argv[0]` against `PATH` using `PATHEXT`
  (so `ccs` → `ccs.cmd`), spawn with the plan's cwd/env and inherited stdio, ignore Ctrl-C in
  ccpick while waiting (`SetConsoleCtrlHandler(None, TRUE)`), then exit with the child's code.
  `launch::find_in_path` gains `PATHEXT` handling on Windows (and keeps its Unix behaviour).
- **Canonical paths:** use `dunce::canonicalize` everywhere paths are canonicalized, so Windows
  paths don't carry the `\\?\` verbatim prefix (display and dedupe stay consistent).
- **Config/cache:** unchanged code; `dirs` yields `%APPDATA%\ccpick\config.toml` and
  `%LOCALAPPDATA%\ccpick\meta.json`.
- **Terminal:** ratatui/crossterm support the Windows console; no changes expected.
- Windows-only code is behind `cfg(windows)`; Unix-only code behind `cfg(unix)`.

## Release and CI

- `dist-workspace.toml`: add `x86_64-pc-windows-msvc`; `installers = ["shell", "powershell"]`;
  regenerate `release.yml` with `dist generate`.
- README install: add
  `powershell -ExecutionPolicy Bypass -c "irm https://github.com/outsharked/ccpick/releases/latest/download/ccpick-installer.ps1 | iex"`.
- `ci.yml`: add `windows-latest` to the matrix running `mise run check`. Any mise task invoked in
  CI must not require bash.
- AGENTS.md: replace "native Windows is not supported" with the Windows rules (`dunce`,
  case-insensitive Windows path comparison, `cfg` gating, never touch stopped WSL distros).

## Error handling

- Home discovery failures (unreadable `/mnt/c/Users`, `wsl.exe` errors, bad `wsl.conf`) → skip that
  home source, one warning each; never fatal.
- Slow foreign filesystems: the metadata cache applies unchanged; first-run scans of `/mnt/c` or
  `\\wsl.localhost` may be slower than native targets.
- Clipboard failure → status message; the command stays on screen to select manually.

## Performance

- Native-only machines: existing targets unchanged (warm < 100 ms, cold < 1 s, full-text < 500 ms).
- With cross-environment homes: warm start < 300 ms (includes the `tasklist.exe` snapshot);
  full-text search over foreign stores has no hard target but must not block the UI thread
  (already true: the search worker runs off-thread).

## Implementation phases

Each phase leaves a working build.

1. **Environments and discovery:** `env` module, `Home`, host detection, WSL→Windows home discovery,
   provider signature change, `Source.env`/`env_config_dir`, name prefixes, launchability, path
   translation, `[environments]` config, missing-dir check via translation.
2. **Running detection:** `process` probe, `pidDomain` parsing, `tasklist.exe` snapshot.
3. **Foreign-session dialog:** command formatting, dialog state and rendering, clipboard.
4. **Native Windows:** Windows home discovery (`wsl.exe`), spawn-and-wait launch, `PATHEXT`,
   `dunce`, Windows process probe, release targets, CI, AGENTS.md/README.

## Testing

- **Pure unit tests (all platforms):** host env detection from injected inputs; `wsl.conf` root
  parsing; path translation both ways incl. case-insensitivity; `wsl.exe -l` UTF-16 decoding
  (with and without BOM, embedded NULs); env inference for configured paths; `pidDomain` parsing
  and check selection; `tasklist` CSV parsing; POSIX and PowerShell quoting and command
  formatting; `PATHEXT` resolution logic with an injected directory listing.
- **Discovery:** tempdir fake drive root (`<tmp>/c/Users/<name>/.claude`) for WSL→Windows homes.
- **Catalog:** fake provider with multiple homes — prefixed names, launchability, foreign sessions
  never produce `Action::Launch`.
- **UI:** App tests for opening/closing the dialog, `Ctrl-A` in the dialog, copy action;
  `TestBackend` render test for the dialog.
- **Windows-only code:** compiled and tested by the Windows CI job; locally type-checked with
  `cargo check --target x86_64-pc-windows-gnu`.
- **Manual acceptance (WSL):** Windows ccs accounts appear as `win:c1`..`win:c3` (shared stores
  deduped); a running Windows `claude.exe` session shows the badge and blocks Enter; Enter on
  another Windows session opens the dialog and `c` puts a working PowerShell command on the
  Windows clipboard.
- **Manual acceptance (Windows):** native resume via `ccs.cmd`; WSL sessions appear as `wsl:…`
  while the distro is running and not when it is stopped (and the distro is not booted).
