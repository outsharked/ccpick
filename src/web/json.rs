//! The catalog as JSON. Pure: no state, no I/O beyond reading transcripts for the preview.
use crate::catalog::Catalog;
use crate::format::{exact, friendly, shorten_home_in};
use crate::model::Role;
use serde_json::{Value, json};

/// Everything the list needs, including the ready-to-paste command for sessions ccpick can't
/// launch itself.
pub fn sessions_payload(catalog: &Catalog, generation: u64, now_ms: i64) -> Value {
    let sessions: Vec<Value> = catalog
        .sessions
        .iter()
        .enumerate()
        .map(|(index, session)| {
            // Every row describes the launch source (override, else default), never the source
            // a running session happens to be live under: a resume command for a session that's
            // already resumed is not something anyone can usefully paste. `running_source` below
            // carries the live account instead, so it isn't lost.
            let source_idx = session.default_source;
            let source = &catalog.sources[source_idx];
            let launchable = catalog.is_launchable(source_idx);
            let cwd = session
                .meta
                .cwd
                .as_deref()
                .map(|p| shorten_home_in(p, &source.env_home, &source.env))
                .unwrap_or_default();
            let running_source = session
                .live
                .map(|(_, live_idx)| catalog.sources[live_idx].name.clone());
            json!({
                "id": session.meta.id,
                "title": session.meta.title,
                "cwd": cwd,
                "branch": session.meta.branch,
                "source": source.name,
                "env": source.env.id(),
                "when": friendly(now_ms, session.meta.last_ts),
                "exact": exact(session.meta.last_ts),
                "messages": session.meta.msg_count,
                "running": session.live.is_some(),
                "running_source": running_source,
                "pid": session.live.map(|(pid, _)| pid),
                "launchable": launchable,
                "shell": source.env.shell_name(),
                "command": crate::shell::resume_command(
                    &catalog.launch_plan(index, source_idx),
                    &source.env,
                ),
            })
        })
        .collect();
    json!({
        "generation": generation,
        "warnings": catalog.warnings,
        "sessions": sessions,
    })
}

/// The transcript for the preview pane.
pub fn messages_payload(catalog: &Catalog, idx: usize) -> Value {
    let messages: Vec<Value> = catalog
        .messages(idx)
        .into_iter()
        .map(|m| {
            json!({
                "role": match m.role { Role::User => "user", Role::Assistant => "assistant" },
                "text": m.text,
                "ts": m.ts,
            })
        })
        .collect();
    json!({ "messages": messages })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{build_fake, fake_catalog, fake_catalog_with_foreign};
    use crate::model::LaunchRecord;
    use crate::providers::fake::FakeProvider;

    const NOW: i64 = 10_000;

    #[test]
    fn a_session_carries_what_the_list_shows() {
        let catalog = fake_catalog();
        let payload = sessions_payload(&catalog, 7, NOW);
        assert_eq!(payload["generation"], 7);
        let sessions = payload["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), catalog.sessions.len());
        let running = sessions
            .iter()
            .find(|s| s["title"] == "Running thing")
            .unwrap();
        assert_eq!(running["pid"], 4242);
        assert_eq!(running["running"], true);
        assert_eq!(running["source"], "two");
        assert!(running["when"].is_string());
        assert!(running["exact"].is_string());
    }

    #[test]
    fn a_session_ccpick_cannot_launch_carries_the_command_to_paste() {
        let catalog = fake_catalog_with_foreign();
        let payload = sessions_payload(&catalog, 0, NOW);
        let foreign = payload["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["title"] == "Windows session")
            .unwrap();
        assert_eq!(foreign["launchable"], false);
        assert!(foreign["command"].as_str().unwrap().contains("--resume"));
        assert_eq!(foreign["shell"], "PowerShell");
    }

    #[test]
    fn warnings_ride_along_with_the_list() {
        let mut catalog = fake_catalog();
        catalog.warnings.push("something to say".into());
        let payload = sessions_payload(&catalog, 0, NOW);
        assert_eq!(payload["warnings"][0], "something to say");
    }

    #[test]
    fn a_row_describes_the_default_source_even_while_running_elsewhere() {
        // Two sources share a store. "one" holds the newer (but not alive) launch record, so
        // it wins as the default source; "two" holds the alive one, so it's the live source.
        // A row must describe "one" throughout, and only name "two" via `running_source`.
        let mut p = FakeProvider::default();
        p.add_source("one", "/s");
        p.add_source("two", "/s");
        p.add_session("/s", "x", "Shared session", 1000, 1000, &[]);
        p.records.insert(
            "one".into(),
            vec![LaunchRecord {
                pid: 1,
                session_id: "x".into(),
                started_at_ms: 100,
                alive: false,
            }],
        );
        p.records.insert(
            "two".into(),
            vec![LaunchRecord {
                pid: 999,
                session_id: "x".into(),
                started_at_ms: 50,
                alive: true,
            }],
        );
        let catalog = build_fake(p);
        // Sanity check the fixture actually exercises the interesting case.
        let session = catalog
            .sessions
            .iter()
            .find(|s| s.meta.title == "Shared session")
            .unwrap();
        assert_eq!(
            session.default_source, 0,
            "fixture: default_source is \"one\""
        );
        assert_eq!(session.live, Some((999, 1)), "fixture: live is \"two\"");

        let payload = sessions_payload(&catalog, 0, NOW);
        let row = payload["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["title"] == "Shared session")
            .unwrap();
        assert_eq!(row["source"], "one");
        assert_eq!(row["shell"], catalog.sources[0].env.shell_name());
        assert!(row["command"].as_str().unwrap().contains("one"));
        assert!(!row["command"].as_str().unwrap().contains("two"));
        assert_eq!(row["running"], true);
        assert_eq!(row["pid"], 999);
        assert_eq!(row["running_source"], "two");
    }

    #[test]
    fn messages_carry_role_and_text() {
        let catalog = fake_catalog();
        let idx = catalog
            .sessions
            .iter()
            .position(|s| s.meta.title == "Docker build cache")
            .unwrap();
        let payload = messages_payload(&catalog, idx);
        let messages = payload["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["text"], "fix docker");
        assert_eq!(messages[1]["role"], "assistant");
    }
}
