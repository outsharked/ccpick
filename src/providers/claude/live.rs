//! Claude's per-process session registry: <config_dir>/sessions/<pid>.json.
use crate::model::LaunchRecord;
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
struct Record {
    pid: i32,
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "startedAt", default)]
    started_at: i64,
}

/// True if `pid` is a running process whose command line mentions claude
/// (guards against a stale record whose pid was reused).
pub fn is_claude_process(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| bytes.windows(6).any(|w| w == b"claude"))
        .unwrap_or(false)
}

pub fn launch_records(config_dir: &Path) -> Vec<LaunchRecord> {
    let Ok(entries) = std::fs::read_dir(config_dir.join("sessions")) else { return vec![] };
    let mut records: Vec<LaunchRecord> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| std::fs::read(p).ok())
        .filter_map(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
        .map(|r| LaunchRecord {
            pid: r.pid,
            alive: is_claude_process(r.pid),
            session_id: r.session_id,
            started_at_ms: r.started_at,
        })
        .collect();
    records.sort_by_key(|r| r.started_at_ms);
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn write_record(dir: &Path, pid: i32, session: &str, started: i64) {
        fs::create_dir_all(dir.join("sessions")).unwrap();
        fs::write(
            dir.join("sessions").join(format!("{pid}.json")),
            format!(r#"{{"pid":{pid},"sessionId":"{session}","cwd":"/x","startedAt":{started},"kind":"interactive"}}"#),
        )
        .unwrap();
    }

    #[test]
    fn missing_sessions_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(launch_records(tmp.path()).is_empty());
    }

    #[test]
    fn stale_record_is_not_alive_and_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        write_record(tmp.path(), 9_999_999, "late", 200);
        write_record(tmp.path(), 9_999_998, "early", 100);
        fs::write(tmp.path().join("sessions/junk.json"), "{nope").unwrap();
        fs::write(tmp.path().join("sessions/other.key"), "x").unwrap();
        let recs = launch_records(tmp.path());
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].session_id, "early");
        assert!(recs.iter().all(|r| !r.alive));
    }

    #[test]
    fn live_claude_process_is_alive() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_claude = tmp.path().join("claude-fake");
        fs::copy("/bin/sleep", &fake_claude).unwrap();
        let mut child = Command::new(&fake_claude).arg("30").spawn().unwrap();
        // Give the child time to exec so /proc/<pid>/cmdline reflects the new program.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let pid = child.id() as i32;
        write_record(tmp.path(), pid, "live", 1);
        let recs = launch_records(tmp.path());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(recs[0].alive);
    }

    #[test]
    fn non_claude_process_is_not_alive() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let alive = is_claude_process(child.id() as i32);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!alive);
        assert!(!is_claude_process(0));
    }
}
