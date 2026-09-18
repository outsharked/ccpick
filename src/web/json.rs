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
            let source_idx = session
                .live
                .map(|(_, s)| s)
                .unwrap_or(session.default_source);
            let source = &catalog.sources[source_idx];
            let launchable = catalog.is_launchable(source_idx);
            let cwd = session
                .meta
                .cwd
                .as_deref()
                .map(|p| shorten_home_in(p, &source.env_home, &source.env))
                .unwrap_or_default();
            json!({
                "index": index,
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
    use crate::catalog::{fake_catalog, fake_catalog_with_foreign};

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
        assert!(running["index"].is_number());
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
