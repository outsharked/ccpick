//! Discovery of Claude config directories: ccs accounts, env, ~/.claude, config, CLI.
use super::transcript::AGENT;
use crate::config::Settings;
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
        Self { ccs: true, home: true }
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
}

fn basename(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

fn claude_with_dir(dir: &Path) -> LaunchSpec {
    LaunchSpec {
        argv_prefix: vec!["claude".into()],
        env_set: vec![(ENV_CONFIG_DIR.into(), dir.display().to_string())],
        env_remove: vec![],
    }
}

fn source(name: String, config_dir: PathBuf, launch: LaunchSpec) -> Source {
    Source { agent: AGENT.into(), name, config_dir, launch }
}

/// ccs account names, default account first. Ok(empty) if ccs isn't installed.
fn ccs_accounts(home: &Path) -> Result<Vec<String>, String> {
    let path = home.join(".ccs/config.yaml");
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(vec![]) };
    let cfg: CcsConfig = serde_yaml::from_str(&text)
        .map_err(|e| format!("ignoring ccs config {}: {e}", path.display()))?;
    let mut names: Vec<String> =
        cfg.accounts.keys().filter_map(|k| k.as_str().map(String::from)).collect();
    if let Some(default) = cfg.default
        && let Some(pos) = names.iter().position(|n| *n == default)
    {
        let n = names.remove(pos);
        names.insert(0, n);
    }
    Ok(names)
}

pub fn discover(settings: &Settings) -> anyhow::Result<Discovery> {
    let cfg: ClaudeConfig = settings.agent_table(AGENT)?;
    let mut out = Discovery::default();
    let mut candidates: Vec<Candidate> = Vec::new();

    if cfg.ccs {
        match ccs_accounts(&settings.home) {
            Ok(names) => {
                for name in names {
                    let dir = settings.home.join(".ccs/instances").join(&name);
                    let launch = LaunchSpec {
                        argv_prefix: vec!["ccs".into(), name.clone()],
                        ..Default::default()
                    };
                    candidates.push(Candidate { source: source(name, dir, launch), explicit: true });
                }
            }
            Err(warning) => out.warnings.push(warning),
        }
    }

    if cfg.home {
        if let Some(dir) = settings.env_var(ENV_CONFIG_DIR) {
            let dir = settings.expand(dir);
            let launch = claude_with_dir(&dir);
            candidates.push(Candidate { source: source(basename(&dir), dir, launch), explicit: true });
        }
        let launch = LaunchSpec {
            argv_prefix: vec!["claude".into()],
            env_remove: vec![ENV_CONFIG_DIR.into()],
            ..Default::default()
        };
        candidates.push(Candidate {
            source: source("claude".into(), settings.home.join(".claude"), launch),
            explicit: false,
        });
    }

    for sc in settings.file.sources.iter().filter(|s| s.agent == AGENT) {
        let dir = settings.expand(&sc.config_dir);
        let launch = match &sc.command {
            Some(argv) if !argv.is_empty() => {
                LaunchSpec { argv_prefix: argv.clone(), ..Default::default() }
            }
            _ => claude_with_dir(&dir),
        };
        let name = sc.name.clone().unwrap_or_else(|| basename(&dir));
        candidates.push(Candidate { source: source(name, dir, launch), explicit: true });
    }

    for dir in &settings.cli_config_dirs {
        let launch = claude_with_dir(dir);
        candidates.push(Candidate {
            source: source(basename(dir), dir.clone(), launch),
            explicit: true,
        });
    }

    let mut seen = HashSet::new();
    for Candidate { mut source, explicit } in candidates {
        match std::fs::canonicalize(&source.config_dir) {
            Ok(canonical) if canonical.is_dir() => {
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
    use std::fs;

    const CCS_YAML: &str = "default: \"c2\"\naccounts:\n  c1:\n    created: \"2026-01-01\"\n  c2:\n    created: \"2026-01-02\"\n  c3: {}\nprofiles: {}\n";

    fn settings(home: &Path) -> Settings {
        Settings { home: home.to_path_buf(), ..Default::default() }
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
        let d = discover(&settings(tmp.path())).unwrap();
        assert_eq!(names(&d), vec!["c2", "c1", "c3"]);
        assert_eq!(d.sources[0].launch.argv_prefix, vec!["ccs", "c2"]);
        assert_eq!(d.sources[0].agent, "claude");
        assert_eq!(
            d.sources[0].config_dir,
            fs::canonicalize(tmp.path().join(".ccs/instances/c2")).unwrap()
        );
        assert!(d.warnings.is_empty());
    }

    #[test]
    fn ccs_can_be_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        setup_ccs(tmp.path());
        let mut s = settings(tmp.path());
        s.file = parse_config("[claude]\nccs = false").unwrap();
        assert!(discover(&s).unwrap().sources.is_empty());
    }

    #[test]
    fn env_dir_duplicating_ccs_instance_is_deduped() {
        let tmp = tempfile::tempdir().unwrap();
        setup_ccs(tmp.path());
        let mut s = settings(tmp.path());
        let c1 = tmp.path().join(".ccs/instances/c1");
        s.env.insert(ENV_CONFIG_DIR.into(), c1.display().to_string());
        assert_eq!(names(&discover(&s).unwrap()), vec!["c2", "c1", "c3"]);
    }

    #[test]
    fn env_dir_source_sets_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("alt-claude");
        fs::create_dir_all(&dir).unwrap();
        let mut s = settings(tmp.path());
        s.env.insert(ENV_CONFIG_DIR.into(), dir.display().to_string());
        let d = discover(&s).unwrap();
        assert_eq!(names(&d), vec!["alt-claude"]);
        assert_eq!(d.sources[0].launch.argv_prefix, vec!["claude"]);
        assert_eq!(
            d.sources[0].launch.env_set,
            vec![(ENV_CONFIG_DIR.to_string(), dir.display().to_string())]
        );
    }

    #[test]
    fn home_claude_removes_env() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        let d = discover(&settings(tmp.path())).unwrap();
        assert_eq!(names(&d), vec!["claude"]);
        assert_eq!(d.sources[0].launch.env_remove, vec![ENV_CONFIG_DIR.to_string()]);
    }

    #[test]
    fn missing_home_claude_is_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let d = discover(&settings(tmp.path())).unwrap();
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
        let d = discover(&s).unwrap();
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
        let d = discover(&settings(tmp.path())).unwrap();
        assert!(d.sources.is_empty());
        assert_eq!(d.warnings.len(), 1);
    }

    #[test]
    fn invalid_claude_table_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = settings(tmp.path());
        s.file = parse_config("[claude]\nccs = \"yes\"").unwrap();
        assert!(discover(&s).is_err());
    }
}
