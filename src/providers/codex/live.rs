//! Whether a listed Codex thread is running right now.
//!
//! Codex writes no pid registry (unlike Claude's `sessions/<pid>.json`), but a running `codex`
//! process holds its rollout file open for as long as it runs, and that file's path is exactly
//! `Thread::rollout_path`. So liveness here is just: does any open file in
//! `ProcessProbe::open_files("codex")` match a thread's rollout path?
use super::db::Thread;
use crate::model::LaunchRecord;
use std::path::PathBuf;

/// One `LaunchRecord` per thread whose rollout file some `codex` process still holds open.
/// `open` is `ProcessProbe::open_files("codex")` — (pid, open file path) pairs, agent-neutral by
/// construction, so the matching against `Thread::rollout_path` happens here, not in
/// `src/process.rs`.
pub fn launch_records(threads: &[Thread], open: &[(u32, PathBuf)]) -> Vec<LaunchRecord> {
    threads
        .iter()
        .filter_map(|thread| {
            let (pid, _) = open.iter().find(|(_, path)| *path == thread.rollout_path)?;
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
    fn unrelated_open_files_do_not_mark_anything_running() {
        let threads = vec![thread_at("id-a", "/s/rollout-a.jsonl")];
        let open = vec![(4242, PathBuf::from("/home/me/.codex/state_5.sqlite"))];
        assert!(launch_records(&threads, &open).is_empty());
    }
}
