pub mod db;
pub mod live;
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
    warnings: RwLock<Vec<String>>,
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
/// then the first message — the same custom > generated > first-message order the Claude provider
/// uses for its own title fields.
///
/// Every branch is truncated, which is where this differs from Claude: Codex's `title` column
/// frequently holds the entire first prompt rather than a short generated label, so it arrives
/// multi-line and sometimes kilobytes long. A title is a single short line everywhere it is
/// used — one record per line in `--list`, one row in the TUI, a window title to match when
/// focusing — so it is cut here, at the one place all three branches pass through.
fn title_for(thread: &db::Thread) -> String {
    if let Some(name) = thread.name.as_deref().filter(|s| !s.trim().is_empty()) {
        return truncate(name, TITLE_MAX);
    }
    if !thread.title.trim().is_empty() {
        return truncate(&thread.title, TITLE_MAX);
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
    /// rollout path, so `scan_file` never has to touch the database again. A database that cannot
    /// be read means no sessions, not a crash — but it is reported, because Codex's whole session
    /// list lives in this one file: schema drift or a permission problem would otherwise make
    /// every Codex session quietly disappear with nothing on screen to explain it.
    fn list_session_files(&self, store: &Path) -> Vec<PathBuf> {
        let threads = match db::listable_threads(store) {
            Ok(threads) => threads,
            Err(err) => {
                self.warnings
                    .write()
                    .unwrap()
                    .push(format!("cannot read {}: {err:#}", store.display()));
                return Vec::new();
            }
        };
        let mut map = self.threads.write().unwrap();
        let mut paths = Vec::with_capacity(threads.len());
        for thread in threads {
            paths.push(thread.rollout_path.clone());
            map.insert(thread.rollout_path.clone(), thread);
        }
        paths
    }
    fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.warnings.write().unwrap())
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
    /// Scoped to `source`'s own store: `self.threads` accumulates entries from every store this
    /// provider has scanned so far (Task 3's map), and two Codex sources sharing this one
    /// provider instance is an ordinary configuration (a foreign `CODEX_HOME`, two `[[source]]`
    /// entries, a native/foreign pair) — `catalog.rs` records only the *last* source that claims
    /// a running session's id, so an unscoped match would silently misattribute a live session
    /// to the wrong source and, downstream, the wrong process domain. Rollout paths are absolute
    /// and live under the store's own config directory, so that prefix is the discriminator —
    /// the same role `source.config_dir` plays in the Claude provider's own `launch_records`.
    fn launch_records(
        &self,
        source: &Source,
        probe: &crate::process::ProcessProbe,
    ) -> Vec<LaunchRecord> {
        let threads: Vec<db::Thread> = self
            .threads
            .read()
            .unwrap()
            .values()
            .filter(|t| t.rollout_path.starts_with(&source.config_dir))
            .cloned()
            .collect();
        let open = probe.open_files(AGENT);
        live::launch_records(&threads, &open)
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
    fn a_database_it_cannot_read_is_reported_not_silently_empty() {
        // Every Codex session lives behind this one file, so "no sessions" and "I could not open
        // the index" look identical on screen unless the error is carried out.
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("state_5.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE threads (id TEXT)").unwrap();
        drop(conn);

        let provider = CodexProvider::default();
        assert!(provider.list_session_files(&db).is_empty());
        let warnings = provider.take_warnings();
        assert_eq!(warnings.len(), 1, "one warning per unreadable store");
        assert!(
            warnings[0].contains("state_5.sqlite"),
            "the warning must name the file: {}",
            warnings[0]
        );
        assert!(
            provider.take_warnings().is_empty(),
            "draining twice must not repeat it"
        );
    }

    #[test]
    fn every_title_source_is_cut_to_one_short_line() {
        // Codex's `title` column is not a short generated label the way Claude's is: it is very
        // often the whole first prompt, multi-line and kilobytes long. Untruncated it breaks the
        // one-record-per-line `--list` format, overflows a TUI row, and is handed to the focus
        // code as a window title it could never match.
        let long = format!("{}\nsecond line", "x".repeat(200));
        let from_title = title_for(&thread("", &long, ""));
        assert_eq!(from_title.chars().count(), TITLE_MAX);
        assert!(!from_title.contains('\n'));

        let from_name = title_for(&thread(&long, "", ""));
        assert_eq!(from_name.chars().count(), TITLE_MAX);
        assert!(!from_name.contains('\n'));

        let from_first = title_for(&thread("", "", &long));
        assert_eq!(from_first.chars().count(), TITLE_MAX);
        assert!(!from_first.contains('\n'));
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

    fn thread_with_rollout(id: &str, rollout: &str) -> db::Thread {
        db::Thread {
            id: id.into(),
            rollout_path: PathBuf::from(rollout),
            cwd: "/home/me".into(),
            title: "t".into(),
            name: None,
            first_user_message: "first".into(),
            git_branch: None,
            created_at_ms: 0,
            recency_at_ms: 0,
        }
    }

    #[test]
    fn launch_records_are_scoped_to_the_calling_sources_own_store() {
        // Two Codex sources sharing this one provider instance — a foreign CODEX_HOME alongside
        // the home directory, say — each with a thread whose rollout file is running.
        // Unscoped matching would let the second source's iteration overwrite the first's
        // record in catalog.rs's (provider, session_id) -> source map.
        let provider = CodexProvider::default();
        {
            let mut threads = provider.threads.write().unwrap();
            let a = PathBuf::from("/home/a/.codex/sessions/rollout-a.jsonl");
            let b = PathBuf::from("/home/b/.codex/sessions/rollout-b.jsonl");
            threads.insert(a.clone(), thread_with_rollout("id-a", a.to_str().unwrap()));
            threads.insert(b.clone(), thread_with_rollout("id-b", b.to_str().unwrap()));
        }
        let probe =
            crate::process::ProcessProbe::with_open_file_lister(crate::env::Env::Linux, |_name| {
                vec![
                    (
                        111,
                        PathBuf::from("/home/a/.codex/sessions/rollout-a.jsonl"),
                    ),
                    (
                        222,
                        PathBuf::from("/home/b/.codex/sessions/rollout-b.jsonl"),
                    ),
                ]
            });
        let source_a = source_at("/home/a/.codex");
        let records_a = provider.launch_records(&source_a, &probe);
        assert_eq!(records_a.len(), 1, "only source a's own thread");
        assert_eq!(records_a[0].session_id, "id-a");
        assert_eq!(records_a[0].pid, 111);

        let source_b = source_at("/home/b/.codex");
        let records_b = provider.launch_records(&source_b, &probe);
        assert_eq!(records_b.len(), 1, "only source b's own thread");
        assert_eq!(records_b[0].session_id, "id-b");
        assert_eq!(records_b[0].pid, 222);
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
