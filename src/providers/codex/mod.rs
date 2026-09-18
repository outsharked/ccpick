pub mod db;
pub mod rollout;
pub mod sources;

use crate::config::Settings;
use crate::model::{Discovery, LaunchPlan, LaunchRecord, Message, Provider, SessionMeta, Source};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

pub const AGENT: &str = "codex";
const TITLE_MAX: usize = 80;

/// The Codex provider. Its `threads` map is filled by `list_session_files` (one read of the
/// state database per store) and read by `scan_file` (a plain lookup, run in parallel over
/// cache misses) — see `src/catalog.rs`'s scan loop for why that split makes the database read
/// happen only once per store rather than once per session.
#[derive(Default)]
pub struct CodexProvider {
    threads: RwLock<HashMap<PathBuf, db::Thread>>,
}

/// The first non-blank line of `s`, trimmed and cut to `max` chars with a trailing ellipsis —
/// same shape as the Claude provider's own title truncation, kept local since neither provider
/// may depend on the other's module.
fn truncate(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let mut t: String = line.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// Title precedence: the name the user set, then the thread's own title (often model-generated),
/// then a truncation of the first message — the same custom > generated > first-message order
/// the Claude provider uses for its own title fields.
fn title_for(thread: &db::Thread) -> String {
    if let Some(name) = thread.name.as_deref().filter(|s| !s.trim().is_empty()) {
        return name.to_string();
    }
    if !thread.title.trim().is_empty() {
        return thread.title.clone();
    }
    if thread.first_user_message.trim().is_empty() {
        return "(untitled)".to_string();
    }
    truncate(&thread.first_user_message, TITLE_MAX)
}

pub fn launch_plan(source: &Source, session: &SessionMeta) -> LaunchPlan {
    let mut argv = source.launch.argv_prefix.clone();
    argv.push("resume".into());
    argv.push(session.id.clone());
    LaunchPlan {
        cwd: session.cwd.clone().unwrap_or_default(),
        argv,
        env_set: source.launch.env_set.clone(),
        env_remove: source.launch.env_remove.clone(),
    }
}

impl Provider for CodexProvider {
    fn id(&self) -> &'static str {
        AGENT
    }
    fn discover_sources(
        &self,
        settings: &Settings,
        home: &crate::homes::Home,
    ) -> anyhow::Result<Discovery> {
        sources::discover(settings, home)
    }
    fn home_markers(&self) -> &'static [&'static str] {
        &[".codex"]
    }
    fn store_for(&self, source: &Source) -> Option<PathBuf> {
        db::newest_state_db(&source.config_dir)
    }
    /// Loads every listable thread from the store's database into `self.threads`, keyed by its
    /// rollout path, so `scan_file` never has to touch the database again. A missing or
    /// unreadable database means no sessions, not a crash.
    fn list_session_files(&self, store: &Path) -> Vec<PathBuf> {
        let Ok(threads) = db::listable_threads(store) else {
            return Vec::new();
        };
        let mut map = self.threads.write().unwrap();
        let mut paths = Vec::with_capacity(threads.len());
        for thread in threads {
            paths.push(thread.rollout_path.clone());
            map.insert(thread.rollout_path.clone(), thread);
        }
        paths
    }
    fn scan_file(&self, path: &Path) -> Option<SessionMeta> {
        let threads = self.threads.read().unwrap();
        let thread = threads.get(path)?;
        Some(SessionMeta {
            agent: AGENT.to_string(),
            id: thread.id.clone(),
            path: path.to_path_buf(),
            title: title_for(thread),
            cwd: Some(PathBuf::from(&thread.cwd)),
            branch: thread.git_branch.clone(),
            first_ts: Some(thread.created_at_ms),
            last_ts: Some(thread.recency_at_ms),
            first_prompt: thread.first_user_message.clone(),
            // Not in the index; leave 0 rather than opening every rollout to count.
            msg_count: 0,
        })
    }
    fn messages(&self, path: &Path) -> Vec<Message> {
        rollout::messages(path)
    }
    fn may_contain(&self, path: &Path, needle_lower: &str) -> bool {
        rollout::may_contain(path, needle_lower)
    }
    fn launch_records(
        &self,
        _source: &Source,
        _probe: &crate::process::ProcessProbe,
    ) -> Vec<LaunchRecord> {
        Vec::new()
    }
    fn launch_plan(&self, source: &Source, session: &SessionMeta) -> LaunchPlan {
        launch_plan(source, session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LaunchSpec;

    fn source_at(config_dir: &str) -> Source {
        Source {
            agent: AGENT.into(),
            name: "codex".into(),
            config_dir: PathBuf::from(config_dir),
            env: crate::env::Env::Linux,
            env_config_dir: PathBuf::from(config_dir),
            env_home: PathBuf::new(),
            launch: LaunchSpec {
                argv_prefix: vec!["codex".into()],
                ..Default::default()
            },
        }
    }

    fn meta_with(id: &str, cwd: &str) -> SessionMeta {
        SessionMeta {
            agent: AGENT.into(),
            id: id.into(),
            path: PathBuf::from("/x.jsonl"),
            title: "t".into(),
            cwd: Some(PathBuf::from(cwd)),
            branch: None,
            first_ts: None,
            last_ts: None,
            first_prompt: String::new(),
            msg_count: 0,
        }
    }

    fn thread(name: &str, title: &str, first: &str) -> db::Thread {
        db::Thread {
            id: "id".into(),
            rollout_path: PathBuf::from("/r.jsonl"),
            cwd: "/home/me".into(),
            title: title.into(),
            name: if name.is_empty() {
                None
            } else {
                Some(name.into())
            },
            first_user_message: first.into(),
            git_branch: None,
            created_at_ms: 0,
            recency_at_ms: 0,
        }
    }

    #[test]
    fn scanning_uses_the_threads_loaded_by_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::providers::codex::db::tests_fixture(tmp.path());
        let provider = CodexProvider::default();
        let files = provider.list_session_files(&db);
        assert_eq!(files.len(), 3, "one path per listable thread");
        let meta = provider
            .scan_file(&files[0])
            .expect("listed files must scan");
        assert_eq!(meta.agent, "codex");
        assert!(!meta.id.is_empty());
    }

    #[test]
    fn the_title_prefers_the_name_the_user_set() {
        // name -> title -> first message, matching the Claude provider's own precedence.
        assert_eq!(title_for(&thread("Name", "Title", "first")), "Name");
        assert_eq!(title_for(&thread("", "Title", "first")), "Title");
        assert_eq!(
            title_for(&thread("", "", "first message here")),
            "first message here"
        );
    }

    #[test]
    fn resuming_runs_codex_in_the_sessions_directory() {
        let source = source_at("/home/me/.codex");
        let meta = meta_with("abc-123", "/home/me/proj");
        let plan = CodexProvider::default().launch_plan(&source, &meta);
        assert_eq!(plan.argv, vec!["codex", "resume", "abc-123"]);
        assert_eq!(plan.cwd, PathBuf::from("/home/me/proj"));
    }
}
