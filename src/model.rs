use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::config::Settings;

/// How to start the agent against a source: argv prefix plus environment changes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LaunchSpec {
    pub argv_prefix: Vec<String>,
    pub env_set: Vec<(String, String)>,
    pub env_remove: Vec<String>,
}

/// One agent config directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub agent: String,
    pub name: String,
    pub config_dir: PathBuf,
    pub launch: LaunchSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub agent: String,
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub cwd: Option<PathBuf>,
    pub branch: Option<String>,
    /// Unix epoch milliseconds.
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    pub first_prompt: String,
    pub msg_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub ts: Option<i64>,
}

/// A record that a source started a session, and whether that process is still alive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRecord {
    pub pid: i32,
    pub session_id: String,
    pub started_at_ms: i64,
    pub alive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub env_set: Vec<(String, String)>,
    pub env_remove: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Discovery {
    pub sources: Vec<Source>,
    pub warnings: Vec<String>,
}

pub trait Provider: Send + Sync {
    /// Stable id used in config, cache keys and the UI.
    fn id(&self) -> &'static str;
    /// Sources for this agent. Err = invalid explicit configuration (fatal).
    fn discover_sources(&self, settings: &Settings) -> anyhow::Result<Discovery>;
    /// Canonical transcript store for a source, if present.
    fn store_for(&self, source: &Source) -> Option<PathBuf>;
    fn list_session_files(&self, store: &Path) -> Vec<PathBuf>;
    fn scan_file(&self, path: &Path) -> Option<SessionMeta>;
    /// Conversation text only (no tool payloads).
    fn messages(&self, path: &Path) -> Vec<Message>;
    /// False only if the file certainly cannot contain `needle_lower` in its messages.
    fn may_contain(&self, _path: &Path, _needle_lower: &str) -> bool {
        true
    }
    fn launch_records(&self, source: &Source) -> Vec<LaunchRecord>;
    fn launch_plan(&self, source: &Source, session: &SessionMeta) -> LaunchPlan;
}
