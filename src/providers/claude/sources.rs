//! Discovery of Claude config directories: ccs accounts, env, ~/.claude, config, CLI.
use super::transcript::AGENT;
use crate::config::Settings;
use crate::env::Env;
use crate::homes::Home;
use crate::model::{Discovery, LaunchSpec, Source};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const ENV_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    pub ccs: bool,
    pub home: bool,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            ccs: true,
            home: true,
        }
    }
}

#[derive(Deserialize)]
struct CcsConfig {
    default: Option<String>,
    #[serde(default)]
    accounts: serde_yaml::Mapping,
}

struct Candidate {
    source: Source,
    /// Explicitly requested: warn if it doesn't exist.
    explicit: bool,
    /// If true, `CLAUDE_CONFIG_DIR` is set to the source's *canonical*
    /// config dir once resolved, rather than whatever (possibly relative or
    /// `..`-containing) path was given. The launcher chdirs into the
    /// session's cwd before exec, so an unresolved value would point at the
    /// wrong directory.
    needs_config_dir_env: bool,
}

fn basename(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// `claude` launched with no env changes yet; the caller marks
/// `needs_config_dir_env` so the dedupe loop fills in `CLAUDE_CONFIG_DIR`
/// from the canonicalized `config_dir`.
fn bare_claude() -> LaunchSpec {
    LaunchSpec {
        argv_prefix: vec!["claude".into()],
        env_set: vec![],
        env_remove: vec![],
    }
}

fn source(
    name: String,
    config_dir: PathBuf,
    env: Env,
    env_home: PathBuf,
    launch: LaunchSpec,
) -> Source {
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

/// ccs account names, default account first. Ok(empty) if ccs isn't installed.
fn ccs_accounts(home: &Path) -> Result<Vec<String>, String> {
    let path = home.join(".ccs/config.yaml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(vec![]);
    };
    let cfg: CcsConfig = serde_yaml::from_str(&text)
        .map_err(|e| format!("ignoring ccs config {}: {e}", path.display()))?;
    let mut names: Vec<String> = cfg
        .accounts
        .keys()
        .filter_map(|k| k.as_str().map(String::from))
        .collect();
    if let Some(default) = cfg.default
        && let Some(pos) = names.iter().position(|n| *n == default)
    {
        let n = names.remove(pos);
        names.insert(0, n);
    }
    Ok(names)
}

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
                        source: source(
                            named(name),
                            dir,
                            home.env.clone(),
                            home.env_dir.clone(),
                            launch,
                        ),
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
                source: source(
                    basename(&dir),
                    dir,
                    host.env.clone(),
                    settings.home.clone(),
                    bare_claude(),
                ),
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
                source: source(
                    basename(dir),
                    dir.clone(),
                    host.env.clone(),
                    settings.home.clone(),
                    bare_claude(),
                ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config;
    use crate::env::{Env, HostContext};
    use crate::homes::Home;
    use std::fs;

    const CCS_YAML: &str = "default: \"c2\"\naccounts:\n  c1:\n    created: \"2026-01-01\"\n  c2:\n    created: \"2026-01-02\"\n  c3: {}\nprofiles: {}\n";

    fn settings(home: &Path) -> Settings {
        Settings {
            home: home.to_path_buf(),
            ..Default::default()
        }
    }

    fn native(s: &Settings) -> Home {
        Home::native(&s.host, s.home.clone())
    }

    fn setup_ccs(home: &Path) {
        fs::create_dir_all(home.join(".ccs")).unwrap();
        fs::write(home.join(".ccs/config.yaml"), CCS_YAML).unwrap();
        for n in ["c1", "c2", "c3"] {
            fs::create_dir_all(home.join(".ccs/instances").join(n)).unwrap();
        }
    }

    fn names(d: &Discovery) -> Vec<&str> {
        d.sources.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn ccs_accounts_default_first() {
        let tmp = tempfile::tempdir().unwrap();
        setup_ccs(tmp.path());
        let s = settings(tmp.path());
        let d = discover(&s, &native(&s)).unwrap();
        assert_eq!(names(&d), vec!["c2", "c1", "c3"]);
        assert_eq!(d.sources[0].launch.argv_prefix, vec!["ccs", "c2"]);
        assert_eq!(d.sources[0].agent, "claude");
        assert_eq!(
            d.sources[0].config_dir,
            dunce::canonicalize(tmp.path().join(".ccs/instances/c2")).unwrap()
        );
        assert!(d.warnings.is_empty());
    }

    #[test]
    fn ccs_can_be_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        setup_ccs(tmp.path());
        let mut s = settings(tmp.path());
        s.file = parse_config("[claude]\nccs = false").unwrap();
        assert!(discover(&s, &native(&s)).unwrap().sources.is_empty());
    }

    #[test]
    fn env_dir_duplicating_ccs_instance_is_deduped() {
        let tmp = tempfile::tempdir().unwrap();
        setup_ccs(tmp.path());
        let mut s = settings(tmp.path());
        let c1 = tmp.path().join(".ccs/instances/c1");
        s.env
            .insert(ENV_CONFIG_DIR.into(), c1.display().to_string());
        assert_eq!(
            names(&discover(&s, &native(&s)).unwrap()),
            vec!["c2", "c1", "c3"]
        );
    }

    #[test]
    fn env_dir_source_sets_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("alt-claude");
        fs::create_dir_all(&dir).unwrap();
        let mut s = settings(tmp.path());
        s.env
            .insert(ENV_CONFIG_DIR.into(), dir.display().to_string());
        let d = discover(&s, &native(&s)).unwrap();
        assert_eq!(names(&d), vec!["alt-claude"]);
        assert_eq!(d.sources[0].launch.argv_prefix, vec!["claude"]);
        assert_eq!(
            d.sources[0].launch.env_set,
            vec![(ENV_CONFIG_DIR.to_string(), dir.display().to_string())]
        );
    }

    #[test]
    fn non_canonical_env_dir_resolves_to_canonical_in_launch_env() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        // Non-canonical: contains a `..` segment, same as a relative
        // CLAUDE_CONFIG_DIR would be relative to the launcher's cwd rather
        // than ccpick's. discover() must resolve it before building the
        // launch env, not embed it verbatim.
        let non_canonical = tmp.path().join("a/../real");
        let mut s = settings(tmp.path());
        s.env
            .insert(ENV_CONFIG_DIR.into(), non_canonical.display().to_string());
        let d = discover(&s, &native(&s)).unwrap();
        let canonical = dunce::canonicalize(&real).unwrap();
        assert_eq!(names(&d), vec!["real"]);
        assert_eq!(d.sources[0].config_dir, canonical);
        assert_eq!(
            d.sources[0].launch.env_set,
            vec![(ENV_CONFIG_DIR.to_string(), canonical.display().to_string())]
        );
    }

    #[test]
    fn home_claude_removes_env() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        let s = settings(tmp.path());
        let d = discover(&s, &native(&s)).unwrap();
        assert_eq!(names(&d), vec!["claude"]);
        assert_eq!(
            d.sources[0].launch.env_remove,
            vec![ENV_CONFIG_DIR.to_string()]
        );
    }

    #[test]
    fn missing_home_claude_is_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let s = settings(tmp.path());
        let d = discover(&s, &native(&s)).unwrap();
        assert!(d.sources.is_empty());
        assert!(d.warnings.is_empty());
    }

    #[test]
    fn configured_and_cli_sources() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".claude-work")).unwrap();
        let cli = tmp.path().join("cli-dir");
        fs::create_dir_all(&cli).unwrap();
        let mut s = settings(tmp.path());
        s.file = parse_config(
            "[[source]]\nname = \"work\"\nconfig_dir = \"~/.claude-work\"\ncommand = [\"wrap\", \"-x\"]\n\n[[source]]\nconfig_dir = \"~/missing\"\n\n[[source]]\nagent = \"codex\"\nconfig_dir = \"~/.codex\"\n",
        )
        .unwrap();
        s.cli_config_dirs = vec![cli.clone()];
        let d = discover(&s, &native(&s)).unwrap();
        assert_eq!(names(&d), vec!["work", "cli-dir"]);
        assert_eq!(d.sources[0].launch.argv_prefix, vec!["wrap", "-x"]);
        assert!(d.sources[0].launch.env_set.is_empty());
        assert_eq!(
            d.sources[1].launch.env_set,
            vec![(ENV_CONFIG_DIR.to_string(), cli.display().to_string())]
        );
        assert_eq!(d.warnings.len(), 1);
        assert!(d.warnings[0].contains("missing"));
    }

    #[test]
    fn invalid_ccs_yaml_warns() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".ccs")).unwrap();
        fs::write(tmp.path().join(".ccs/config.yaml"), "accounts: [unclosed").unwrap();
        let s = settings(tmp.path());
        let d = discover(&s, &native(&s)).unwrap();
        assert!(d.sources.is_empty());
        assert_eq!(d.warnings.len(), 1);
    }

    #[test]
    fn invalid_claude_table_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = settings(tmp.path());
        s.file = parse_config("[claude]\nccs = \"yes\"").unwrap();
        assert!(discover(&s, &native(&s)).is_err());
    }

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
            env: Env::Wsl {
                distro: "Ubuntu".into(),
            },
            wsl_mount_root: PathBuf::from(format!("{}/", root.join("mnt").display())),
        };
        let mut s = settings(&root.join("linux-home"));
        s.host = host;
        s.env
            .insert(ENV_CONFIG_DIR.into(), "/should/not/apply".into());
        s.cli_config_dirs = vec![root.join("cli-dir")];
        let home = Home {
            env: Env::Windows,
            dir: profile,
            env_dir: PathBuf::from(r"C:\Users\me"),
            label: Some("win".into()),
        };
        (s, home)
    }

    // `windows_home_under_wsl` fakes a WSL host observing a Windows home mounted at
    // `/mnt/c/...`, which only exists as a real path shape when the test binary itself runs on
    // a Unix-like OS (forward-slash paths). A native Windows test process can't produce that
    // path shape even with an injected `HostContext`, since `dunce::canonicalize` always
    // normalizes to backslashes there.
    #[test]
    #[cfg(unix)]
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
        assert!(
            !names(&d)
                .iter()
                .any(|n| n.contains("cli-dir") || n.contains("apply"))
        );
    }

    // Same reason as `foreign_home_sources_are_prefixed_and_use_their_own_paths`: relies on a
    // genuine forward-slash WSL mount path shape that native Windows canonicalization can't
    // produce.
    #[test]
    #[cfg(unix)]
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
            vec![(
                ENV_CONFIG_DIR.to_string(),
                r"C:\Users\me\.claude".to_string()
            )]
        );
    }

    #[test]
    fn explicit_source_env_overrides_and_invalid_env_errors() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("cfg")).unwrap();
        let mut s = settings(tmp.path());
        // Literal (single-quoted) TOML strings don't process `\`, so a Windows path embeds
        // safely without escaping.
        s.file = parse_config(&format!(
            "[claude]\nccs = false\nhome = false\n\n[[source]]\nconfig_dir = '{}'\nenv = \"wsl:Debian\"\n",
            tmp.path().join("cfg").display()
        ))
        .unwrap();
        let d = discover(&s, &native(&s)).unwrap();
        assert_eq!(
            d.sources[0].env,
            Env::Wsl {
                distro: "Debian".into()
            }
        );

        s.file = parse_config(&format!(
            "[[source]]\nconfig_dir = '{}'\nenv = \"dos\"\n",
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
}
