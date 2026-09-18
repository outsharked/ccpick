//! Discovery of Codex config directories: `~/.codex` and `$CODEX_HOME`.
use super::AGENT;
use crate::config::Settings;
use crate::env::Env;
use crate::homes::Home;
use crate::model::{Discovery, LaunchSpec, Source};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const ENV_CONFIG_DIR: &str = "CODEX_HOME";

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct CodexConfig {
    pub home: bool,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self { home: true }
    }
}

struct Candidate {
    source: Source,
    /// Explicitly requested: warn if it doesn't exist.
    explicit: bool,
    /// If true, `CODEX_HOME` is set to the source's *canonical* config dir once resolved, rather
    /// than whatever (possibly relative or `..`-containing) path was given — the launcher chdirs
    /// into the session's cwd before exec, so an unresolved value would point at the wrong
    /// directory.
    needs_config_dir_env: bool,
}

fn basename(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// `codex` launched with no env changes yet; the caller marks `needs_config_dir_env` so the
/// dedupe loop fills in `CODEX_HOME` from the canonicalized `config_dir`.
fn bare_codex() -> LaunchSpec {
    LaunchSpec {
        argv_prefix: vec!["codex".into()],
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

pub fn discover(settings: &Settings, home: &Home) -> anyhow::Result<Discovery> {
    let cfg: CodexConfig = settings.agent_table(AGENT)?;
    let host = &settings.host;
    let mut out = Discovery::default();
    let mut candidates: Vec<Candidate> = Vec::new();
    let named = |name: String| match &home.label {
        Some(label) => format!("{label}:{name}"),
        None => name,
    };

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
                    bare_codex(),
                ),
                explicit: true,
                needs_config_dir_env: true,
            });
        }
        let launch = LaunchSpec {
            argv_prefix: vec!["codex".into()],
            env_remove: vec![ENV_CONFIG_DIR.into()],
            ..Default::default()
        };
        candidates.push(Candidate {
            source: source(
                named("codex".into()),
                home.dir.join(".codex"),
                home.env.clone(),
                home.env_dir.clone(),
                launch,
            ),
            explicit: false,
            needs_config_dir_env: false,
        });
    }

    // `--config-dir` is claimed by the Claude provider (it carries no `agent`, so only one
    // provider may claim it — otherwise every use would mint a source for each registered
    // provider pointing at the same directory). An explicit `[[source]] agent = "codex"` entry
    // has no such ambiguity, so it is the only way to name a Codex directory outside the home.
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
                _ => (bare_codex(), true),
            };
            candidates.push(Candidate {
                source: source(name, dir, env, env_home, launch),
                explicit: true,
                needs_config_dir_env,
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
    use crate::env::Env;
    use crate::homes::Home;
    use crate::model::Provider;
    use std::fs;

    fn settings(home: &Path) -> Settings {
        Settings {
            home: home.to_path_buf(),
            ..Default::default()
        }
    }

    fn native(s: &Settings) -> Home {
        Home::native(&s.host, s.home.clone())
    }

    fn names(d: &Discovery) -> Vec<&str> {
        d.sources.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn a_home_without_codex_yields_no_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = settings(tmp.path());
        let home = Home::native(&settings.host, tmp.path().to_path_buf());
        assert!(
            super::super::CodexProvider::default()
                .discover_sources(&settings, &home)
                .unwrap()
                .sources
                .is_empty()
        );
        // Missing, not explicitly configured: silent, no warning either.
        assert!(
            discover(&settings, &native(&settings))
                .unwrap()
                .warnings
                .is_empty()
        );
    }

    #[test]
    fn home_codex_is_found_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".codex")).unwrap();
        let s = settings(tmp.path());
        let d = discover(&s, &native(&s)).unwrap();
        assert_eq!(names(&d), vec!["codex"]);
        assert_eq!(d.sources[0].agent, "codex");
        assert_eq!(
            d.sources[0].launch.env_remove,
            vec![ENV_CONFIG_DIR.to_string()]
        );
    }

    #[test]
    fn home_can_be_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".codex")).unwrap();
        let mut s = settings(tmp.path());
        s.file = parse_config("[codex]\nhome = false").unwrap();
        assert!(discover(&s, &native(&s)).unwrap().sources.is_empty());
    }

    #[test]
    fn env_var_source_sets_launch_env() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("alt-codex");
        fs::create_dir_all(&dir).unwrap();
        let mut s = settings(tmp.path());
        s.env
            .insert(ENV_CONFIG_DIR.into(), dir.display().to_string());
        let d = discover(&s, &native(&s)).unwrap();
        assert_eq!(names(&d), vec!["alt-codex"]);
        assert_eq!(d.sources[0].launch.argv_prefix, vec!["codex"]);
        assert_eq!(
            d.sources[0].launch.env_set,
            vec![(
                ENV_CONFIG_DIR.to_string(),
                dunce::canonicalize(&dir).unwrap().display().to_string()
            )]
        );
    }

    #[test]
    fn env_var_source_disabled_with_home() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("alt-codex");
        fs::create_dir_all(&dir).unwrap();
        let mut s = settings(tmp.path());
        s.env
            .insert(ENV_CONFIG_DIR.into(), dir.display().to_string());
        s.file = parse_config("[codex]\nhome = false").unwrap();
        assert!(discover(&s, &native(&s)).unwrap().sources.is_empty());
    }

    #[test]
    fn configured_codex_source_is_discovered() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".codex-work")).unwrap();
        let mut s = settings(tmp.path());
        s.file = parse_config(
            "[codex]\nhome = false\n\n[[source]]\nagent = \"codex\"\nname = \"work\"\nconfig_dir = \"~/.codex-work\"\ncommand = [\"wrap\", \"-x\"]\n",
        )
        .unwrap();
        let d = discover(&s, &native(&s)).unwrap();
        assert_eq!(names(&d), vec!["work"]);
        assert_eq!(d.sources[0].agent, "codex");
        assert_eq!(d.sources[0].launch.argv_prefix, vec!["wrap", "-x"]);
        assert!(d.sources[0].launch.env_set.is_empty());
    }

    #[test]
    fn configured_source_for_another_agent_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = settings(tmp.path());
        s.file = parse_config(
            "[codex]\nhome = false\n\n[[source]]\nname = \"work\"\nconfig_dir = \"~/.claude-work\"\n",
        )
        .unwrap();
        // Default agent is "claude" (config.rs's default_agent), so this entry is not codex's.
        assert!(discover(&s, &native(&s)).unwrap().sources.is_empty());
    }

    #[test]
    fn cli_config_dirs_do_not_produce_a_codex_source() {
        let tmp = tempfile::tempdir().unwrap();
        let cli = tmp.path().join("cli-dir");
        fs::create_dir_all(&cli).unwrap();
        let mut s = settings(tmp.path());
        s.file = parse_config("[codex]\nhome = false").unwrap();
        s.cli_config_dirs = vec![cli];
        // --config-dir belongs to the Claude provider; Codex must not also claim it, or a single
        // flag would mint one source per provider for the same directory.
        assert!(discover(&s, &native(&s)).unwrap().sources.is_empty());
    }

    #[test]
    fn foreign_home_source_is_prefixed() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("mnt/c/Users/me");
        fs::create_dir_all(profile.join(".codex")).unwrap();
        let s = settings(tmp.path());
        let home = Home {
            env: Env::Windows,
            dir: profile,
            env_dir: PathBuf::from(r"C:\Users\me"),
            label: Some("win".into()),
        };
        let d = discover(&s, &home).unwrap();
        assert_eq!(names(&d), vec!["win:codex"]);
        assert_eq!(d.sources[0].env, Env::Windows);
    }

    #[test]
    fn invalid_codex_table_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = settings(tmp.path());
        s.file = parse_config("[codex]\nhome = \"yes\"").unwrap();
        assert!(discover(&s, &native(&s)).is_err());
    }

    #[test]
    fn native_sources_record_host_env_and_home() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".codex")).unwrap();
        let s = settings(tmp.path());
        let d = discover(&s, &native(&s)).unwrap();
        assert_eq!(d.sources[0].env, Env::Linux);
        assert_eq!(d.sources[0].env_home, tmp.path());
        assert_eq!(d.sources[0].env_config_dir, d.sources[0].config_dir);
    }
}
