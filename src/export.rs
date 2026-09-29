//! `ccpick export`: recent sessions as JSON for an LLM to read.
use crate::catalog::Catalog;
use crate::format::shorten_home_in;
use crate::model::{Message, Role};
use crate::search;
use chrono::TimeZone;
use serde::Serialize;

/// What to include per session.
#[derive(Debug, Clone, Copy)]
pub struct ExportOptions {
    /// Only sessions active at or after this instant (Unix epoch ms).
    pub since_ms: i64,
    /// Cap on `opening_prompt`, in chars. 0 omits the field.
    pub head_chars: usize,
    /// Cap on `tail`, in chars. 0 omits the field.
    pub tail_chars: usize,
}

#[derive(Debug, Serialize)]
struct Record {
    agent: String,
    id: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_active: Option<String>,
    messages: u32,
    /// Config directory the session lives in.
    source: String,
    /// Paste-ready command that resumes the session (cd, config dir and agent invocation).
    resume_command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    opening_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tail: Option<String>,
    truncated: bool,
}

/// Parses `--since`: a relative age (`36h`, `14d`, `2w`) or a local date (`2026-09-01`).
pub fn parse_since(input: &str, now_ms: i64) -> Result<i64, String> {
    let input = input.trim();
    let bad = || {
        format!(
            "invalid --since {input:?}: expected an age like 14d, 36h, 2w or a date like 2026-09-01"
        )
    };
    if let Some(unit) = input.chars().last().filter(char::is_ascii_alphabetic) {
        let digits = &input[..input.len() - 1];
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        let hour_ms: i64 = 3_600_000;
        let unit_ms = match unit {
            'h' => hour_ms,
            'd' => 24 * hour_ms,
            'w' => 7 * 24 * hour_ms,
            _ => return Err(bad()),
        };
        let n: i64 = digits.parse().map_err(|_| bad())?;
        return Ok(now_ms.saturating_sub(n.saturating_mul(unit_ms)));
    }
    let date = chrono::NaiveDate::parse_from_str(input, "%Y-%m-%d").map_err(|_| bad())?;
    let midnight = date.and_hms_opt(0, 0, 0).ok_or_else(bad)?;
    chrono::Local
        .from_local_datetime(&midnight)
        .earliest()
        .map(|t| t.timestamp_millis())
        .ok_or_else(bad)
}

/// The first `n` chars of `text`, and whether anything was cut (marked with a trailing `…`).
fn head(text: &str, n: usize) -> (String, bool) {
    match text.char_indices().nth(n) {
        Some((byte, _)) => (format!("{}…", &text[..byte]), true),
        None => (text.to_string(), false),
    }
}

/// The last `n` chars of `text`, and whether anything was cut (marked with a leading `…`).
fn tail(text: &str, n: usize) -> (String, bool) {
    let total = text.chars().count();
    match text.char_indices().nth(total.saturating_sub(n)) {
        Some((byte, _)) if total > n => (format!("…{}", &text[byte..]), true),
        _ => (text.to_string(), false),
    }
}

/// The opening prompt and the tail of a conversation, each capped, plus whether anything was cut.
/// The tail is the messages after the opening prompt, labelled by role. A cap of 0 omits that part.
fn excerpts(
    messages: &[Message],
    head_chars: usize,
    tail_chars: usize,
) -> (Option<String>, Option<String>, bool) {
    let opening = messages.iter().position(|m| m.role == Role::User);
    let mut cut = false;
    let opening_prompt = (head_chars > 0).then(|| {
        let text = opening.map_or("", |i| messages[i].text.as_str());
        let (text, was_cut) = head(text, head_chars);
        cut |= was_cut;
        text
    });
    let tail_text = (tail_chars > 0).then(|| {
        let rest = &messages[opening.map_or(0, |i| i + 1)..];
        let joined = rest
            .iter()
            .map(|m| {
                let who = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                format!("{who}: {}", m.text)
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let (text, was_cut) = tail(&joined, tail_chars);
        cut |= was_cut;
        text
    });
    (opening_prompt, tail_text, cut)
}

fn iso(ts_ms: Option<i64>) -> Option<String> {
    ts_ms
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// A JSON array, one session per line, most recently active first.
pub fn export_json(catalog: &Catalog, query: &str, opts: ExportOptions) -> String {
    let mut order: Vec<usize> = search::matching(catalog, query)
        .into_iter()
        .filter(|&i| {
            catalog.sessions[i]
                .meta
                .last_ts
                .is_some_and(|t| t >= opts.since_ms)
        })
        .collect();
    order.sort_by_key(|&i| std::cmp::Reverse(catalog.sessions[i].meta.last_ts));
    let lines: Vec<String> = order
        .into_iter()
        .map(|i| {
            let s = &catalog.sessions[i];
            let source = &catalog.sources[s.default_source];
            let (opening_prompt, tail, truncated) =
                excerpts(&catalog.messages(i), opts.head_chars, opts.tail_chars);
            let record = Record {
                agent: s.meta.agent.clone(),
                id: s.meta.id.clone(),
                title: s.meta.title.clone(),
                project: s
                    .meta
                    .cwd
                    .as_ref()
                    .map(|p| shorten_home_in(p, &source.env_home, &source.env)),
                branch: s.meta.branch.clone(),
                started: iso(s.meta.first_ts),
                last_active: iso(s.meta.last_ts),
                messages: s.meta.msg_count,
                source: source.name.clone(),
                resume_command: crate::shell::resume_command(
                    &catalog.launch_plan(i, s.default_source),
                    &source.env,
                ),
                opening_prompt,
                tail,
                truncated,
            };
            serde_json::to_string(&record).expect("record serializes")
        })
        .collect();
    if lines.is_empty() {
        "[]\n".into()
    } else {
        format!("[\n{}\n]\n", lines.join(",\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{build_fake, fake_catalog};
    use crate::providers::fake::FakeProvider;

    fn msg(role: Role, text: &str) -> Message {
        Message {
            role,
            text: text.into(),
            ts: None,
        }
    }

    fn opts(since_ms: i64, head_chars: usize, tail_chars: usize) -> ExportOptions {
        ExportOptions {
            since_ms,
            head_chars,
            tail_chars,
        }
    }

    fn parse(out: &str) -> Vec<serde_json::Value> {
        serde_json::from_str(out).unwrap()
    }

    const DAY: i64 = 86_400_000;

    #[test]
    fn since_relative_ages() {
        let now = 100 * DAY;
        assert_eq!(parse_since("14d", now), Ok(86 * DAY));
        assert_eq!(parse_since("2w", now), Ok(86 * DAY));
        assert_eq!(parse_since("36h", now), Ok(now - 36 * 3_600_000));
    }

    #[test]
    fn since_local_date_is_local_midnight() {
        use chrono::TimeZone;
        let want = chrono::Local
            .with_ymd_and_hms(2026, 9, 1, 0, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(parse_since("2026-09-01", 0), Ok(want));
    }

    #[test]
    fn since_rejects_garbage() {
        for bad in ["", "d", "14", "14x", "-3d", "2026-13-40", "yesterday"] {
            assert!(parse_since(bad, 0).is_err(), "{bad}");
        }
    }

    #[test]
    fn short_conversation_is_not_truncated() {
        let m = [msg(Role::User, "fix docker"), msg(Role::Assistant, "done")];
        let (head, tail, cut) = excerpts(&m, 300, 700);
        assert_eq!(head.as_deref(), Some("fix docker"));
        assert_eq!(tail.as_deref(), Some("assistant: done"));
        assert!(!cut);
    }

    #[test]
    fn tail_excludes_the_opening_message() {
        let m = [msg(Role::User, "only prompt")];
        let (head, tail, cut) = excerpts(&m, 300, 700);
        assert_eq!(head.as_deref(), Some("only prompt"));
        assert_eq!(tail.as_deref(), Some(""));
        assert!(!cut);
    }

    #[test]
    fn long_opening_prompt_is_cut_to_head_chars() {
        let m = [msg(Role::User, "abcdefghij")];
        let (head, _, cut) = excerpts(&m, 4, 700);
        assert_eq!(head.as_deref(), Some("abcd…"));
        assert!(cut);
    }

    #[test]
    fn tail_keeps_the_end_and_labels_roles() {
        let m = [
            msg(Role::User, "start"),
            msg(Role::Assistant, "0123456789"),
            msg(Role::User, "thanks"),
            msg(Role::Assistant, "welcome"),
        ];
        // "user: thanks\n\nassistant: welcome" is 32 chars; keep the last 26.
        let (_, tail, cut) = excerpts(&m, 300, 26);
        assert_eq!(tail.as_deref(), Some("…thanks\n\nassistant: welcome"));
        assert!(cut);
    }

    #[test]
    fn multibyte_text_is_cut_on_char_boundaries() {
        let m = [msg(Role::User, "héllo wörld")];
        let (head, _, _) = excerpts(&m, 2, 700);
        assert_eq!(head.as_deref(), Some("hé…"));
    }

    #[test]
    fn zero_chars_omits_the_field() {
        let m = [msg(Role::User, "a"), msg(Role::Assistant, "b")];
        let (head, tail, cut) = excerpts(&m, 0, 0);
        assert_eq!(head, None);
        assert_eq!(tail, None);
        assert!(!cut);
    }

    #[test]
    fn export_lists_recent_sessions_newest_first() {
        let out = export_json(&fake_catalog(), "", opts(1000, 300, 700));
        let recs = parse(&out);
        let ids: Vec<&str> = recs.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(recs[0]["title"], "Docker build cache");
        assert_eq!(recs[0]["agent"], "fake");
        assert_eq!(recs[0]["messages"], 2);
        assert_eq!(recs[0]["opening_prompt"], "fix docker");
        assert_eq!(recs[0]["tail"], "assistant: done");
        assert_eq!(recs[0]["truncated"], false);
        assert_eq!(recs[0]["source"], "one");
        assert!(recs[0]["resume_command"].as_str().unwrap().contains("a"));
    }

    #[test]
    fn export_filters_on_last_active_not_start() {
        // "d" started at 9000 but was last active at 500.
        let out = export_json(&fake_catalog(), "", opts(2000, 300, 700));
        let ids: Vec<String> = parse(&out)
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn export_applies_the_query() {
        let out = export_json(&fake_catalog(), "pineapple", opts(0, 300, 700));
        let recs = parse(&out);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0]["id"], "b");
    }

    #[test]
    fn export_formats_times_and_project() {
        let mut p = FakeProvider::default();
        p.add_source("one", "/s");
        p.add_session("/s", "x", "T", 0, 86_400_000, &[(Role::User, "hi")]);
        p.set_cwd("/s", "x", Some("/fake/proj"));
        let recs = parse(&export_json(&build_fake(p), "", opts(0, 300, 700)));
        assert_eq!(recs[0]["started"], "1970-01-01T00:00:00Z");
        assert_eq!(recs[0]["last_active"], "1970-01-02T00:00:00Z");
        assert_eq!(recs[0]["project"], "~/proj");
        assert!(recs[0].get("branch").is_none());
    }

    #[test]
    fn export_of_nothing_is_an_empty_array() {
        let out = export_json(&fake_catalog(), "", opts(i64::MAX, 300, 700));
        assert!(parse(&out).is_empty());
    }

    #[test]
    fn export_is_one_record_per_line() {
        let out = export_json(&fake_catalog(), "", opts(0, 300, 700));
        assert_eq!(out.lines().count(), 4 + 2);
    }
}
