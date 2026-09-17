//! Claude Code JSONL transcript parsing.
use crate::model::{Message, Role, SessionMeta};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub const AGENT: &str = "claude";
const TITLE_MAX: usize = 80;

#[derive(Deserialize)]
struct Line<'a> {
    #[serde(rename = "type")]
    kind: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "gitBranch")]
    git_branch: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "isMeta")]
    is_meta: Option<bool>,
    #[serde(borrow)]
    message: Option<MessageBody<'a>>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    summary: Option<String>,
}

#[derive(Deserialize)]
struct MessageBody<'a> {
    #[serde(borrow)]
    content: Option<&'a RawValue>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Block>),
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

pub fn parse_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp_millis())
}

pub fn truncate(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let mut t: String = line.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn content_text(raw: &RawValue) -> String {
    match serde_json::from_str::<Content>(raw.get()) {
        Ok(Content::Text(s)) => s,
        Ok(Content::Blocks(blocks)) => blocks
            .into_iter()
            .filter(|b| b.kind == "text")
            .filter_map(|b| b.text)
            .collect::<Vec<_>>()
            .join("\n"),
        Err(_) => String::new(),
    }
}

/// The conversational message carried by a line, if any.
fn conversational(line: &Line) -> Option<Message> {
    let role = match line.kind.as_deref()? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    if line.is_meta == Some(true) {
        return None;
    }
    let text = content_text(line.message.as_ref()?.content?);
    let text = text.trim();
    // Slash commands, command output and injected reminders are XML-tagged user lines.
    if text.is_empty() || (role == Role::User && text.starts_with('<')) {
        return None;
    }
    Some(Message {
        role,
        text: text.to_string(),
        ts: line.timestamp.as_deref().and_then(parse_ts),
    })
}

/// Calls `f` for each parseable line; returns the number parsed, or None if unreadable.
fn for_each_line(path: &Path, mut f: impl FnMut(Line<'_>)) -> Option<usize> {
    let file = File::open(path).ok()?;
    let mut parsed = 0;
    for raw in BufReader::new(file).split(b'\n') {
        let Ok(raw) = raw else { break };
        let Ok(line) = serde_json::from_slice::<Line>(&raw) else {
            continue;
        };
        parsed += 1;
        f(line);
    }
    Some(parsed)
}

pub fn scan_file(path: &Path) -> Option<SessionMeta> {
    let id = path.file_stem()?.to_str()?.to_string();
    let mut cwd: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut first_ts: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    let mut ai_title: Option<String> = None;
    let mut custom_title: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut msg_count = 0u32;

    let parsed = for_each_line(path, |line| {
        if let Some(ts) = line.timestamp.as_deref().and_then(parse_ts) {
            first_ts = Some(first_ts.map_or(ts, |f| f.min(ts)));
            last_ts = Some(last_ts.map_or(ts, |l| l.max(ts)));
        }
        if cwd.is_none() {
            cwd = line.cwd.clone();
        }
        if let Some(b) = line.git_branch.as_ref().filter(|b| !b.is_empty()) {
            branch = Some(b.clone());
        }
        match line.kind.as_deref() {
            Some("ai-title") if line.ai_title.is_some() => ai_title = line.ai_title.clone(),
            Some("custom-title") if line.custom_title.is_some() => {
                custom_title = line.custom_title.clone()
            }
            Some("summary") if line.summary.is_some() => summary = line.summary.clone(),
            _ => {}
        }
        if let Some(msg) = conversational(&line) {
            msg_count += 1;
            if first_prompt.is_none() && msg.role == Role::User {
                first_prompt = Some(msg.text);
            }
        }
    })?;
    if parsed == 0 {
        return None;
    }

    let first_prompt = first_prompt.unwrap_or_default();
    let title = [custom_title, ai_title, summary]
        .into_iter()
        .flatten()
        .find(|t| !t.trim().is_empty())
        .unwrap_or_else(|| {
            if first_prompt.is_empty() {
                "(untitled)".to_string()
            } else {
                truncate(&first_prompt, TITLE_MAX)
            }
        });

    Some(SessionMeta {
        agent: AGENT.to_string(),
        id,
        path: path.to_path_buf(),
        title,
        cwd: cwd.map(PathBuf::from),
        branch,
        first_ts,
        last_ts,
        first_prompt,
        msg_count,
    })
}

pub fn messages(path: &Path) -> Vec<Message> {
    let mut out = Vec::new();
    for_each_line(path, |line| {
        if let Some(m) = conversational(&line) {
            out.push(m);
        }
    });
    out
}

/// Path of a test fixture under `tests/fixtures/claude/`.
#[cfg(test)]
pub fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude")
        .join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        fixture_path(name)
    }

    #[test]
    fn scans_basic_metadata() {
        let m = scan_file(&fixture("basic.jsonl")).unwrap();
        assert_eq!(m.agent, "claude");
        assert_eq!(m.id, "basic");
        assert_eq!(m.title, "Docker build cache fix");
        assert_eq!(m.cwd, Some(PathBuf::from("/work/proj")));
        assert_eq!(m.branch.as_deref(), Some("feature-x"));
        assert_eq!(m.first_ts, parse_ts("2026-09-01T10:00:00.000Z"));
        assert_eq!(m.last_ts, parse_ts("2026-09-01T10:05:00.000Z"));
        assert_eq!(m.first_prompt, "Fix the Docker build cache\nsecond line");
        assert_eq!(m.msg_count, 3);
    }

    #[test]
    fn messages_exclude_meta_commands_and_tools() {
        let msgs = messages(&fixture("basic.jsonl"));
        let texts: Vec<&str> = msgs.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "Fix the Docker build cache\nsecond line",
                "Looking at the Dockerfile.",
                "Cache fixed."
            ]
        );
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].role, Role::Assistant);
        assert!(
            !texts
                .iter()
                .any(|t| t.contains("SECRETTOOLOUTPUT") || t.contains("docker build ."))
        );
    }

    #[test]
    fn custom_title_wins() {
        assert_eq!(
            scan_file(&fixture("custom-title.jsonl")).unwrap().title,
            "My renamed session"
        );
    }

    #[test]
    fn falls_back_to_truncated_first_prompt() {
        let t = scan_file(&fixture("untitled.jsonl")).unwrap().title;
        assert_eq!(t.chars().count(), 80);
        assert!(t.ends_with('…'));
        assert!(t.starts_with("This prompt is intentionally"));
    }

    #[test]
    fn untitled_without_cwd() {
        let m = scan_file(&fixture("no-cwd.jsonl")).unwrap();
        assert_eq!(m.title, "(untitled)");
        assert_eq!(m.cwd, None);
        assert_eq!(m.msg_count, 1);
    }

    #[test]
    fn garbage_file_is_none() {
        assert!(scan_file(&fixture("garbage.jsonl")).is_none());
        assert!(scan_file(&fixture("does-not-exist.jsonl")).is_none());
    }

    #[test]
    fn truncate_uses_first_line() {
        assert_eq!(truncate("short\nmore", 80), "short");
        assert_eq!(truncate("abcdef", 4), "abc…");
    }
}
