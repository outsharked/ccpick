//! A Codex rollout file: one JSON object per line.
//!
//! `response_item / message` is the raw model conversation and carries injected context — the
//! environment block, AGENTS.md — so it reads as noise. What the person typed and what Codex
//! showed them is carried by `event_msg` lines, in one of two shapes depending on the Codex
//! version that wrote the file (never both in the files seen so far): older versions write
//! `user_message` / `agent_message`, newer ones write `item_completed` with a `UserMessage` /
//! `AgentMessage` item. Only those are read.
use crate::model::{Message, Role};
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    payload: Option<Payload>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: Option<String>,
    item: Option<Item>,
}

/// The `item` of an `item_completed` event. Its `type` is PascalCase (`UserMessage`,
/// `Reasoning`, `CommandExecution`, ...) and the content parts' own `type` casing differs between
/// user and agent items, so parts are read by whether they carry `text`, not by their type.
#[derive(Deserialize)]
struct Item {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    content: Vec<Part>,
}

#[derive(Deserialize)]
struct Part {
    text: Option<String>,
}

impl Item {
    fn text(self) -> Option<String> {
        let text = self
            .content
            .into_iter()
            .filter_map(|p| p.text)
            .collect::<Vec<_>>()
            .join("\n");
        (!text.is_empty()).then_some(text)
    }
}

/// The conversational message carried by a line, if any.
fn conversational(line: Line) -> Option<Message> {
    if line.kind != "event_msg" {
        return None;
    }
    let payload = line.payload?;
    let (role, text) = match payload.kind.as_deref()? {
        "user_message" => (Role::User, payload.message?),
        "agent_message" => (Role::Assistant, payload.message?),
        "item_completed" => {
            let item = payload.item?;
            let role = match item.kind.as_str() {
                "UserMessage" => Role::User,
                "AgentMessage" => Role::Assistant,
                _ => return None,
            };
            (role, item.text()?)
        }
        _ => return None,
    };
    Some(Message {
        role,
        text,
        ts: None,
    })
}

pub fn messages(path: &Path) -> Vec<Message> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Line>(line).ok())
        .filter_map(conversational)
        .collect()
}

/// Same rule as the Claude provider's `may_contain`: only a needle JSON could never escape
/// (plain alphanumerics, space, `-`, `_`, `.`) is safe to rule out with a raw-byte scan, since
/// the scan runs over the file's on-disk bytes rather than the JSON-decoded text. Anything else
/// — a quote, a backslash — could appear in the file only in escaped form, so a raw scan for it
/// would find nothing even when it's there; such needles fall back to "maybe" rather than
/// risking a false negative.
fn is_simple_needle(needle: &str) -> bool {
    !needle.is_empty()
        && needle
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'-' | b'_' | b'.'))
}

/// A raw-byte, ASCII-case-insensitive scan, the same shortcut the Claude provider uses: cheap
/// enough to run over every file, and only ever used to rule a file *out*.
pub fn may_contain(path: &Path, needle_lower: &str) -> bool {
    if !is_simple_needle(needle_lower) {
        return true;
    }
    match std::fs::read(path) {
        Ok(mut bytes) => {
            bytes.make_ascii_lowercase();
            memchr::memmem::find(&bytes, needle_lower.as_bytes()).is_some()
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture() -> PathBuf {
        crate::testutil::manifest_dir().join("tests/fixtures/codex/simple.jsonl")
    }

    #[test]
    fn only_the_human_visible_conversation_is_read() {
        let msgs = messages(&fixture());
        assert_eq!(
            msgs.len(),
            2,
            "the response_item turn is injected context, not conversation"
        );
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].text, "fix the PINEAPPLE parser");
        assert_eq!(msgs[1].role, Role::Assistant);
        assert!(msgs[1].text.starts_with("Found it"));
        assert!(
            !msgs
                .iter()
                .any(|m| m.text.contains("noise that must not appear")),
            "injected environment context must never reach the preview"
        );
    }

    #[test]
    fn item_completed_events_are_read_like_the_older_message_events() {
        // Newer Codex writes the conversation as `item_completed` events (`UserMessage` /
        // `AgentMessage` items) and no longer emits `user_message` / `agent_message` at all.
        let msgs = messages(
            &crate::testutil::manifest_dir().join("tests/fixtures/codex/item_completed.jsonl"),
        );
        assert_eq!(
            msgs.len(),
            2,
            "reasoning, commands and injected context are not conversation"
        );
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].text, "fix the PINEAPPLE parser");
        assert_eq!(msgs[1].role, Role::Assistant);
        assert!(msgs[1].text.starts_with("Found it"));
    }

    #[test]
    fn a_missing_or_malformed_file_is_empty_rather_than_a_panic() {
        assert!(messages(Path::new("/nonexistent/ccpick-test.jsonl")).is_empty());
        let tmp = tempfile::tempdir().unwrap();
        let junk = tmp.path().join("junk.jsonl");
        std::fs::write(&junk, b"not json\n{\"type\":\"event_msg\"}\n").unwrap();
        assert!(messages(&junk).is_empty());
    }

    #[test]
    fn may_contain_rules_out_a_needle_that_is_not_in_the_file() {
        assert!(may_contain(&fixture(), "pineapple"));
        assert!(!may_contain(&fixture(), "definitely-not-in-this-file"));
    }

    #[test]
    fn may_contain_falls_back_to_maybe_for_a_needle_json_could_escape() {
        // A literal `"` never appears unescaped in this fixture's bytes, so a raw scan for it
        // would wrongly say "no" if it ran at all; the guard means it never runs.
        assert!(may_contain(&fixture(), "\"pineapple"));
    }
}
