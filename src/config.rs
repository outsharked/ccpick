use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ConfigFile {
    #[serde(rename = "source")]
    pub sources: Vec<SourceConfig>,
    pub environments: EnvironmentsConfig,
    /// Per-agent tables such as `[claude]`, interpreted by each provider.
    #[serde(flatten)]
    pub agents: toml::Table,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SourceConfig {
    #[serde(default = "default_agent")]
    pub agent: String,
    pub name: Option<String>,
    pub config_dir: String,
    pub command: Option<Vec<String>>,
    /// `windows`, `linux`, `macos` or `wsl:<distro>`; inferred from the path when omitted.
    pub env: Option<String>,
}

fn default_agent() -> String {
    "claude".to_string()
}

#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub home: PathBuf,
    pub host: crate::env::HostContext,
    pub env: HashMap<String, String>,
    pub file: ConfigFile,
    pub cli_config_dirs: Vec<PathBuf>,
}

impl Settings {
    /// Non-empty environment variable value.
    pub fn env_var(&self, key: &str) -> Option<&str> {
        self.env
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    pub fn expand(&self, path: &str) -> PathBuf {
        if path == "~" {
            self.home.clone()
        } else if let Some(rest) = path.strip_prefix("~/") {
            self.home.join(rest)
        } else {
            PathBuf::from(path)
        }
    }

    pub fn agent_table<T: DeserializeOwned + Default>(&self, agent: &str) -> Result<T> {
        match self.file.agents.get(agent) {
            Some(value) => value
                .clone()
                .try_into()
                .with_context(|| format!("invalid [{agent}] section in ccpick config")),
            None => Ok(T::default()),
        }
    }
}

pub fn parse_config(text: &str) -> Result<ConfigFile> {
    toml::from_str(text).context("invalid ccpick config")
}

pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("ccpick").join("config.toml"))
}

pub fn load_settings(
    config_path: Option<&Path>,
    cli_config_dirs: Vec<PathBuf>,
) -> Result<Settings> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let file = match config_path {
        Some(path) if path.exists() => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            parse_config(&text).with_context(|| format!("in {}", path.display()))?
        }
        _ => ConfigFile::default(),
    };
    let env: HashMap<String, String> = std::env::vars().collect();
    Ok(Settings {
        home,
        host: crate::env::HostContext::detect(&env),
        env,
        file,
        cli_config_dirs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sources_and_agent_tables() {
        let cfg = parse_config(
            r#"
[claude]
ccs = false

[[source]]
name = "work"
config_dir = "~/.claude-work"
command = ["wrap", "--p"]
"#,
        )
        .unwrap();
        assert_eq!(cfg.sources.len(), 1);
        assert_eq!(cfg.sources[0].agent, "claude");
        assert_eq!(cfg.sources[0].name.as_deref(), Some("work"));
        assert_eq!(
            cfg.sources[0].command,
            Some(vec!["wrap".to_string(), "--p".to_string()])
        );
        assert!(cfg.agents.contains_key("claude"));
    }

    #[test]
    fn empty_config_is_default() {
        let cfg = parse_config("").unwrap();
        assert!(cfg.sources.is_empty());
        assert!(cfg.agents.is_empty());
    }

    #[test]
    fn invalid_config_errors() {
        assert!(parse_config("[[source]]\nname = 1").is_err());
    }

    #[test]
    fn expand_tilde_and_env_var() {
        let mut s = Settings {
            home: PathBuf::from("/home/u"),
            ..Default::default()
        };
        assert_eq!(s.expand("~"), PathBuf::from("/home/u"));
        assert_eq!(s.expand("~/x/y"), PathBuf::from("/home/u/x/y"));
        assert_eq!(s.expand("/abs"), PathBuf::from("/abs"));
        s.env.insert("EMPTY".into(), "".into());
        s.env.insert("SET".into(), "v".into());
        assert_eq!(s.env_var("EMPTY"), None);
        assert_eq!(s.env_var("SET"), Some("v"));
        assert_eq!(s.env_var("MISSING"), None);
    }

    #[test]
    fn agent_table_defaults_and_errors() {
        #[derive(serde::Deserialize, Default, Debug, PartialEq)]
        #[serde(default)]
        struct T {
            flag: bool,
        }
        let mut s = Settings::default();
        assert_eq!(s.agent_table::<T>("x").unwrap(), T::default());
        s.file = parse_config("[x]\nflag = true").unwrap();
        assert_eq!(s.agent_table::<T>("x").unwrap(), T { flag: true });
        s.file = parse_config("[x]\nflag = \"yes\"").unwrap();
        assert!(s.agent_table::<T>("x").is_err());
    }

    #[test]
    fn load_settings_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let s = load_settings(Some(&dir.path().join("nope.toml")), vec![]).unwrap();
        assert!(s.file.sources.is_empty());
    }

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

    #[test]
    fn load_settings_bad_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(&p, "not = [valid").unwrap();
        assert!(load_settings(Some(&p), vec![]).is_err());
    }
}
