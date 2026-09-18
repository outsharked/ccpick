//! Claude's per-process session registry: <config_dir>/sessions/<pid>.json.
use crate::env::Env;
use crate::model::LaunchRecord;
use crate::process::{PidDomain, ProcessProbe};
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
struct Record {
    pid: i64,
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "startedAt", default)]
    started_at: i64,
    #[serde(rename = "pidDomain", default)]
    pid_domain: Option<String>,
}

/// Records from `<config_dir>/sessions/*.json`, with liveness from `probe`. A record without
/// `pidDomain` belongs to the source's environment.
pub fn launch_records(config_dir: &Path, env: &Env, probe: &ProcessProbe) -> Vec<LaunchRecord> {
    let Ok(entries) = std::fs::read_dir(config_dir.join("sessions")) else {
        return vec![];
    };
    let mut records: Vec<LaunchRecord> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| std::fs::read(p).ok())
        .filter_map(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
        .map(|r| {
            let domain = r
                .pid_domain
                .as_deref()
                .and_then(PidDomain::parse)
                .or_else(|| PidDomain::of(env));
            let alive = match (domain, u32::try_from(r.pid)) {
                (Some(domain), Ok(pid)) => probe.is_running(domain, env, pid, "claude"),
                _ => false,
            };
            LaunchRecord {
                pid: r.pid as i32,
                alive,
                session_id: r.session_id,
                started_at_ms: r.started_at,
            }
        })
        .collect();
    records.sort_by_key(|r| r.started_at_ms);
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Env;
    use crate::process::ProcessProbe;
    use std::fs;
    #[cfg(target_os = "linux")]
    use std::process::Command;

    fn records(dir: &Path) -> Vec<LaunchRecord> {
        launch_records(dir, &Env::Linux, &ProcessProbe::new(Env::Linux))
    }

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
        assert!(records(tmp.path()).is_empty());
    }

    #[test]
    fn stale_record_is_not_alive_and_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        write_record(tmp.path(), 9_999_999, "late", 200);
        write_record(tmp.path(), 9_999_998, "early", 100);
        fs::write(tmp.path().join("sessions/junk.json"), "{nope").unwrap();
        fs::write(tmp.path().join("sessions/other.key"), "x").unwrap();
        let recs = records(tmp.path());
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].session_id, "early");
        assert!(recs.iter().all(|r| !r.alive));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn live_claude_process_is_alive() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_claude = tmp.path().join("claude-fake");
        fs::copy("/bin/sleep", &fake_claude).unwrap();
        let mut child = Command::new(&fake_claude).arg("30").spawn().unwrap();
        // Give the child time to exec so /proc/<pid>/cmdline reflects the new program.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let pid = child.id() as i32;
        write_record(tmp.path(), pid, "live", 1);
        let recs = records(tmp.path());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(recs[0].alive);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn non_claude_process_is_not_alive() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let alive =
            ProcessProbe::new(Env::Linux).is_running(PidDomain::Linux, &Env::Linux, pid, "claude");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!alive);
        assert!(!ProcessProbe::new(Env::Linux).is_running(
            PidDomain::Linux,
            &Env::Linux,
            0,
            "claude"
        ));
    }

    #[test]
    fn windows_pid_domain_is_checked_against_windows_processes() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("sessions")).unwrap();
        fs::write(
            tmp.path().join("sessions/58892.json"),
            r#"{"pid":58892,"sessionId":"s","startedAt":1,"pidDomain":"win32:host"}"#,
        )
        .unwrap();
        let wsl = Env::Wsl {
            distro: "Ubuntu".into(),
        };
        let probe = ProcessProbe::with_windows_snapshot(
            wsl.clone(),
            Ok(std::collections::HashMap::from([(
                58892,
                "claude.exe".to_string(),
            )])),
        );
        assert!(launch_records(tmp.path(), &Env::Windows, &probe)[0].alive);
        // Same record, but the host can't see Windows processes.
        assert!(
            !launch_records(tmp.path(), &Env::Windows, &ProcessProbe::new(Env::Linux))[0].alive
        );
    }
}
