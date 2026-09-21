//! Whether a listed Codex thread is running right now.
//!
//! Codex writes no pid registry (unlike Claude's `sessions/<pid>.json`), so liveness is read from
//! what a running `codex` process holds open. Which file that is has changed across Codex
//! versions, and both are accepted:
//!
//! - `<config>/thread-writer-locks/<thread id>.lock` — what 0.155.0 holds. The file name is the
//!   thread id, so no path mapping is needed.
//! - the thread's rollout file (`Thread::rollout_path`) — what earlier versions held open for as
//!   long as the session ran.
//!
//! Matching either means an upgrade or downgrade of Codex doesn't silently stop every session
//! from showing as running. A thread is live if a `codex` process holds *either* file open.
use super::db::Thread;
use crate::model::LaunchRecord;
use std::path::PathBuf;

/// The thread id a writer-lock path names, if it is one: `<dir>/thread-writer-locks/<id>.lock`.
/// Kept separate so the shape Codex uses is stated once and tested directly.
fn locked_thread_id(path: &std::path::Path) -> Option<&str> {
    if path.extension()? != "lock" {
        return None;
    }
    if path.parent()?.file_name()? != "thread-writer-locks" {
        return None;
    }
    path.file_stem()?.to_str()
}

/// One `LaunchRecord` per thread some `codex` process still holds open, by either signal above.
/// `open` is `ProcessProbe::open_files("codex")` — (pid, open file path) pairs, agent-neutral by
/// construction, so every Codex-specific path shape is matched here, not in `src/process.rs`.
pub fn launch_records(threads: &[Thread], open: &[(u32, PathBuf)]) -> Vec<LaunchRecord> {
    threads
        .iter()
        .filter_map(|thread| {
            let (pid, _) = open.iter().find(|(_, path)| {
                *path == thread.rollout_path || locked_thread_id(path) == Some(thread.id.as_str())
            })?;
            Some(LaunchRecord {
                pid: *pid as i32,
                session_id: thread.id.clone(),
                started_at_ms: 0,
                alive: true,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread_at(id: &str, rollout: &str) -> Thread {
        Thread {
            id: id.into(),
            rollout_path: PathBuf::from(rollout),
            cwd: "/home/me".into(),
            title: "t".into(),
            name: None,
            first_user_message: String::new(),
            git_branch: None,
            created_at_ms: 0,
            recency_at_ms: 0,
        }
    }

    #[test]
    fn a_thread_whose_rollout_is_held_open_is_running() {
        let threads = vec![
            thread_at("id-a", "/s/rollout-a.jsonl"),
            thread_at("id-b", "/s/rollout-b.jsonl"),
        ];
        let open = vec![(4242, PathBuf::from("/s/rollout-a.jsonl"))];
        let records = launch_records(&threads, &open);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session_id, "id-a");
        assert_eq!(records[0].pid, 4242);
        assert!(records[0].alive);
    }

    #[test]
    fn a_thread_whose_writer_lock_is_held_open_is_running() {
        // Codex 0.155.0 keeps no rollout file open: it holds
        // `<config>/thread-writer-locks/<thread id>.lock` instead. The lock names the thread
        // directly, so this is the signal even though the rollout path is untouched.
        let threads = vec![
            thread_at("01a0c42d-d66c-70b1-b66f-4ab0e4a59637", "/s/rollout-a.jsonl"),
            thread_at("01a0b4b1-825e-7752-a325-1394b611ec19", "/s/rollout-b.jsonl"),
        ];
        let open = vec![(
            90644,
            PathBuf::from(
                "/home/me/.codex/thread-writer-locks/01a0c42d-d66c-70b1-b66f-4ab0e4a59637.lock",
            ),
        )];
        let records = launch_records(&threads, &open);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].session_id,
            "01a0c42d-d66c-70b1-b66f-4ab0e4a59637"
        );
        assert_eq!(records[0].pid, 90644);
        assert!(records[0].alive);
    }

    #[test]
    fn a_lock_for_an_unlisted_thread_matches_nothing() {
        // Subagent threads get their own locks and are deliberately not listed.
        let threads = vec![thread_at("id-a", "/s/rollout-a.jsonl")];
        let open = vec![(
            90644,
            PathBuf::from("/home/me/.codex/thread-writer-locks/some-subagent-thread.lock"),
        )];
        assert!(launch_records(&threads, &open).is_empty());
    }

    #[test]
    fn unrelated_open_files_do_not_mark_anything_running() {
        let threads = vec![thread_at("id-a", "/s/rollout-a.jsonl")];
        let open = vec![(4242, PathBuf::from("/home/me/.codex/state_5.sqlite"))];
        assert!(launch_records(&threads, &open).is_empty());
    }
}
