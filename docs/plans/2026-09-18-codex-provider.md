# Codex provider implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** List, search, preview, resume, and detect-as-running Codex CLI sessions alongside Claude's, through the existing `Provider` trait.

**Architecture:** A new `src/providers/codex/` mirroring `src/providers/claude/`. The catalog comes from Codex's own SQLite index (`~/.codex/state_<n>.sqlite`), read-only; the rollout JSONL files are read for the preview and full-text search. Liveness is derived from the fact that a running `codex` process holds its rollout file open, via a new agent-neutral capability in `src/process.rs`.

**Tech Stack:** Rust 2024, `rusqlite` (bundled SQLite), existing `serde_json`, `memchr`, `rayon`.

**Spec:** `docs/specs/2026-09-18-codex-provider-design.md`

## Global Constraints

- **Run `mise check` before every commit**: no fmt diff, no clippy warnings (`-D warnings`), all tests green, no compiler warnings.
- After touching `src/process.rs` or any `cfg(windows)`/`cfg(unix)` code, also run `mise lint-windows` and `mise test-windows`.
- **Never read the developer's real `~/.codex` from a test.** Build fixture databases and fixture rollout files in the test itself, or under `tests/fixtures/codex/`.
- **Never write to Codex's data.** The database is opened read-only; rollout files are never modified. No test may spawn `codex` or focus a window.
- **Provider boundary:** nothing outside `src/providers/codex/` may know Codex file layouts, its SQLite schema, or its rollout format. Nothing inside it may know Claude's.
- **Process liveness lives in `src/process.rs`**, not in a provider (AGENTS.md). The provider maps paths to session ids; the probe finds the processes.
- Public repo: no personal paths, hostnames or usernames in code, tests or docs.
- Commit messages in the repo's style, ending with this trailer on its own line after a blank line:
  `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`

## Known limitation to carry, not solve

ccpick's metadata cache keys on the rollout file's mtime and size. A thread renamed in Codex without gaining any new messages will therefore keep its old title until the next message arrives or the user runs `--no-cache`. This is accepted for now; do not bend the cache framework to fix it. Note it in the README and file it as a follow-up issue in the final task.

---

### Task 1: Read Codex's thread index

**Files:**
- Create: `src/providers/codex/mod.rs`, `src/providers/codex/db.rs`
- Modify: `Cargo.toml`, `src/providers/mod.rs`

**Interfaces:**
- Produces:
  - `pub struct Thread { pub id: String, pub rollout_path: PathBuf, pub cwd: String, pub title: String, pub name: Option<String>, pub first_user_message: String, pub git_branch: Option<String>, pub created_at_ms: i64, pub recency_at_ms: i64 }`
  - `pub fn newest_state_db(config_dir: &Path) -> Option<PathBuf>`
  - `pub fn listable_threads(db: &Path) -> anyhow::Result<Vec<Thread>>`

- [ ] **Step 1: Add the dependency**

In `Cargo.toml`: `rusqlite = { version = "0.37", features = ["bundled"] }`. Bundled compiles SQLite from source, so the Windows cross-build needs no system library. Confirm `mise lint-windows` still passes after adding it — that is the point of checking early.

Create `src/providers/codex/mod.rs` with `pub mod db;` and add `pub mod codex;` to `src/providers/mod.rs`. Do **not** register it in `all()` yet; that happens in Task 4, once it works.

- [ ] **Step 2: Write the failing tests**

In `src/providers/codex/db.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture with the columns this reader uses. The real table has ~40 columns; these are
    /// the ones the provider depends on, so a schema change that drops one fails here.
    ///
    /// `pub(crate)` because Task 3's provider tests build on the same fixture: two fixtures that
    /// drift apart would let a provider test pass against a shape the reader never sees.
    pub(crate) fn tests_fixture(dir: &Path) -> PathBuf {
        let path = dir.join("state_5.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, cwd TEXT NOT NULL,
                title TEXT NOT NULL, name TEXT, first_user_message TEXT NOT NULL DEFAULT '',
                git_branch TEXT, created_at_ms INTEGER, recency_at_ms INTEGER NOT NULL DEFAULT 0,
                source TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0);
             INSERT INTO threads VALUES
               ('id-cli','/s/2026/09/18/rollout-a.jsonl','/home/me/proj','Fix the parser','Parser',
                'fix the parser please','main',1000,2000,'cli',0),
               ('id-named','/s/2026/09/18/rollout-b.jsonl','/home/me','Generated title','My Name',
                'hello',NULL,1100,2100,'vscode',0),
               ('id-sub','/s/2026/09/18/rollout-c.jsonl','/home/me','Subagent work',NULL,
                'go',NULL,1200,2200,'{\"subagent\":{\"other\":\"guardian\"}}',0),
               ('id-empty','/s/2026/09/18/rollout-d.jsonl','/home/me','',NULL,'',NULL,1300,2300,'cli',0);",
        )
        .unwrap();
        path
    }

    #[test]
    fn subagent_and_stillborn_threads_are_not_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tests_fixture(tmp.path());
        let ids: Vec<String> = listable_threads(&db).unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(ids, vec!["id-named", "id-cli"], "newest first, no subagent, no stillborn");
    }

    #[test]
    fn a_thread_carries_what_the_list_shows() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tests_fixture(tmp.path());
        let threads = listable_threads(&db).unwrap();
        let cli = threads.iter().find(|t| t.id == "id-cli").unwrap();
        assert_eq!(cli.rollout_path, PathBuf::from("/s/2026/09/18/rollout-a.jsonl"));
        assert_eq!(cli.cwd, "/home/me/proj");
        assert_eq!(cli.title, "Fix the parser");
        assert_eq!(cli.name.as_deref(), Some("Parser"));
        assert_eq!(cli.git_branch.as_deref(), Some("main"));
        assert_eq!(cli.created_at_ms, 1000);
        assert_eq!(cli.recency_at_ms, 2000);
    }

    #[test]
    fn the_highest_numbered_state_database_wins() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["state_2.sqlite", "state_10.sqlite", "state_9.sqlite", "notes.txt"] {
            std::fs::write(tmp.path().join(name), b"").unwrap();
        }
        assert_eq!(
            newest_state_db(tmp.path()).unwrap().file_name().unwrap(),
            "state_10.sqlite",
            "10 beats 9 numerically, not alphabetically"
        );
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(newest_state_db(empty.path()), None);
    }

    #[test]
    fn a_database_without_the_expected_columns_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state_1.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE threads (id TEXT)").unwrap();
        assert!(listable_threads(&path).is_err());
    }
}
```

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test --lib providers::codex::db`
Expected: FAIL, `cannot find function listable_threads`.

- [ ] **Step 4: Implement**

```rust
//! Codex's own index of its sessions: `~/.codex/state_<n>.sqlite`. Read-only — Codex holds this
//! database open in WAL mode while it runs, and ccpick never writes to another tool's data.
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Thread {
    pub id: String,
    pub rollout_path: PathBuf,
    pub cwd: String,
    pub title: String,
    pub name: Option<String>,
    pub first_user_message: String,
    pub git_branch: Option<String>,
    pub created_at_ms: i64,
    pub recency_at_ms: i64,
}

/// The state database, newest schema wins. The number is part of the filename (`state_5`), so a
/// future `state_6` is picked up without a code change. Compared numerically: `state_10` is
/// newer than `state_9`, which a string sort would get wrong.
pub fn newest_state_db(config_dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in std::fs::read_dir(config_dir).ok()?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(n) = name
            .strip_prefix("state_")
            .and_then(|rest| rest.strip_suffix(".sqlite"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(seen, _)| n > *seen) {
            best = Some((n, path));
        }
    }
    best.map(|(_, path)| path)
}

/// Threads a person would resume: started from the CLI or the editor, with something in them.
///
/// Two thirds of a real database is subagent threads — children spawned by a parent run, which
/// nobody resumes — so they are excluded by `source`, along with threads that never got a title
/// or a first message.
pub fn listable_threads(db: &Path) -> anyhow::Result<Vec<Thread>> {
    // `mode=ro` on a URI: opening read-only cannot create or migrate the file, and is safe
    // alongside Codex's own writer.
    let uri = format!("file:{}?mode=ro", db.display());
    let conn = rusqlite::Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut stmt = conn.prepare(
        "SELECT id, rollout_path, cwd, title, name, first_user_message, git_branch,
                COALESCE(created_at_ms, 0), recency_at_ms
         FROM threads
         WHERE source IN ('cli', 'vscode')
           AND (COALESCE(title, '') <> '' OR COALESCE(name, '') <> ''
                OR COALESCE(first_user_message, '') <> '')
         ORDER BY recency_at_ms DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Thread {
            id: row.get(0)?,
            rollout_path: PathBuf::from(row.get::<_, String>(1)?),
            cwd: row.get(2)?,
            title: row.get(3)?,
            name: row.get(4)?,
            first_user_message: row.get(5)?,
            git_branch: row.get(6)?,
            created_at_ms: row.get(7)?,
            recency_at_ms: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
```

`name` and `git_branch` are nullable in the real schema, so they are `Option<String>`; `title` and `first_user_message` carry `NOT NULL DEFAULT ''`.

- [ ] **Step 5: Verify**

Run: `cargo test --lib providers::codex::db` — expected PASS.
Run: `mise check`, then `mise lint-windows` — the bundled SQLite must cross-compile.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/providers/
git commit -m "codex: read the thread index from Codex's own state database"
```

---

### Task 2: Read a rollout file

**Files:**
- Create: `src/providers/codex/rollout.rs`, `tests/fixtures/codex/simple.jsonl`
- Modify: `src/providers/codex/mod.rs`

**Interfaces:**
- Consumes: `crate::model::{Message, Role}`.
- Produces: `pub fn messages(path: &Path) -> Vec<Message>`, `pub fn may_contain(path: &Path, needle_lower: &str) -> bool`

- [ ] **Step 1: Write the fixture**

`tests/fixtures/codex/simple.jsonl` — four lines, no personal data:

```
{"timestamp":"2026-09-18T09:25:27.000Z","type":"session_meta","payload":{"id":"019d-abc","timestamp":"2026-09-18T09:25:27.000Z","cwd":"/home/me/proj","originator":"codex-tui","cli_version":"0.155.0","source":"cli","git":{"branch":"main"}}}
{"timestamp":"2026-09-18T09:25:28.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>noise that must not appear</environment_context>"}]}}
{"timestamp":"2026-09-18T09:25:29.000Z","type":"event_msg","payload":{"type":"user_message","message":"fix the PINEAPPLE parser","images":[]}}
{"timestamp":"2026-09-18T09:25:31.000Z","type":"event_msg","payload":{"type":"agent_message","message":"Found it: the lexer drops the last token.","phase":"commentary"}}
```

- [ ] **Step 2: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex/simple.jsonl")
    }

    #[test]
    fn only_the_human_visible_conversation_is_read() {
        let msgs = messages(&fixture());
        assert_eq!(msgs.len(), 2, "the response_item turn is injected context, not conversation");
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].text, "fix the PINEAPPLE parser");
        assert_eq!(msgs[1].role, Role::Assistant);
        assert!(msgs[1].text.starts_with("Found it"));
        assert!(
            !msgs.iter().any(|m| m.text.contains("noise that must not appear")),
            "injected environment context must never reach the preview"
        );
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
}
```

- [ ] **Step 3: Run and watch them fail**

Run: `cargo test --lib providers::codex::rollout`
Expected: FAIL, `cannot find function messages`.

- [ ] **Step 4: Implement**

```rust
//! A Codex rollout file: one JSON object per line.
//!
//! `response_item / message` is the raw model conversation and carries injected context — the
//! environment block, AGENTS.md — so it reads as noise. `event_msg / user_message` and
//! `agent_message` are what the person typed and what Codex showed them. Only those are read.
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
}

pub fn messages(path: &Path) -> Vec<Message> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Line>(line).ok())
        .filter(|line| line.kind == "event_msg")
        .filter_map(|line| {
            let payload = line.payload?;
            let role = match payload.kind.as_deref()? {
                "user_message" => Role::User,
                "agent_message" => Role::Assistant,
                _ => return None,
            };
            Some(Message {
                role,
                text: payload.message?,
                ts: None,
            })
        })
        .collect()
}

/// A raw-byte, ASCII-case-insensitive scan, the same shortcut the Claude provider uses: cheap
/// enough to run over every file, and only ever used to rule a file *out*.
pub fn may_contain(path: &Path, needle_lower: &str) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let mut lower = bytes;
    lower.make_ascii_lowercase();
    memchr::memmem::find(&lower, needle_lower.as_bytes()).is_some()
}
```

Read `src/providers/claude/mod.rs`'s `may_contain` and `is_simple_needle` before finishing: if it guards against needles that JSON escapes, apply the same guard here rather than inventing a different rule.

- [ ] **Step 5: Verify**

Run: `cargo test --lib providers::codex::rollout` — expected PASS. Then `mise check`.

- [ ] **Step 6: Commit**

```bash
git add src/providers/codex/ tests/fixtures/codex/
git commit -m "codex: read the human-visible conversation from a rollout file"
```

---

### Task 3: The provider itself

**Files:**
- Create: `src/providers/codex/sources.rs`
- Modify: `src/providers/codex/mod.rs`

**Interfaces:**
- Consumes: Tasks 1 and 2; `crate::model::Provider`, `Source`, `SessionMeta`, `LaunchPlan`, `Discovery`; `crate::homes::Home`.
- Produces: `pub struct CodexProvider` implementing `Provider`, with an internal `RwLock<HashMap<PathBuf, Thread>>` filled by `list_session_files` and read by `scan_file`.

- [ ] **Step 1: Understand the call order before writing anything**

Read `src/catalog.rs`'s scan loop. `list_session_files(store)` is called once per store, then `scan_file(file)` runs in parallel over the misses. That is what makes the cache viable: load every thread during `list_session_files`, and `scan_file` becomes a map lookup with no further database access.

- [ ] **Step 2: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanning_uses_the_threads_loaded_by_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::providers::codex::db::tests_fixture(tmp.path());
        let provider = CodexProvider::default();
        let files = provider.list_session_files(&db);
        assert_eq!(files.len(), 2, "one path per listable thread");
        let meta = provider.scan_file(&files[0]).expect("listed files must scan");
        assert_eq!(meta.agent, "codex");
        assert!(!meta.id.is_empty());
    }

    #[test]
    fn the_title_prefers_the_name_the_user_set() {
        // name -> title -> first message, matching the Claude provider's own precedence.
        assert_eq!(title_for(&thread("Name", "Title", "first")), "Name");
        assert_eq!(title_for(&thread("", "Title", "first")), "Title");
        assert_eq!(title_for(&thread("", "", "first message here")), "first message here");
    }

    #[test]
    fn resuming_runs_codex_in_the_sessions_directory() {
        let source = source_at("/home/me/.codex");
        let meta = meta_with(/* id */ "abc-123", /* cwd */ "/home/me/proj");
        let plan = CodexProvider::default().launch_plan(&source, &meta);
        assert_eq!(plan.argv, vec!["codex", "resume", "abc-123"]);
        assert_eq!(plan.cwd, PathBuf::from("/home/me/proj"));
    }

    #[test]
    fn a_home_without_codex_yields_no_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = settings_with_home(tmp.path());
        let home = crate::homes::Home::native(&settings.host, tmp.path().to_path_buf());
        assert!(CodexProvider::default().discover_sources(&settings, &home).unwrap().sources.is_empty());
    }
}
```

Write whatever small helpers these need (`thread`, `source_at`, `meta_with`, `settings_with_home`) in the same test module, following the patterns in `src/providers/claude/sources.rs`'s tests. Export the Task 1 fixture builder as `pub(crate) fn tests_fixture` under `#[cfg(test)]` so both modules use one fixture rather than two that can drift.

- [ ] **Step 3: Run and watch them fail**

Run: `cargo test --lib providers::codex::sources`
Expected: FAIL, `cannot find type CodexProvider`.

- [ ] **Step 4: Implement**

The `Provider` impl:

- `id()` → `"codex"`.
- `home_markers()` → `&[".codex"]`, so the cross-environment home scan finds Codex homes in a WSL distro from Windows and under `/mnt/c/Users/<user>` from WSL.
- `discover_sources()` → `~/.codex` for the home, or `$CODEX_HOME` when set in `settings.env`, named `codex` (and `codex:<label>` when the home carries a label, matching how the Claude provider names `win:`/`wsl:` sources). Honour `[codex] home = false` in `settings.agents` the way the Claude provider honours `[claude]`. Read `src/providers/claude/sources.rs` and follow its shape.
- `store_for(source)` → `db::newest_state_db(&source.config_dir)`. The *database* is the store, not the sessions directory: it is what identifies this Codex installation and what the catalog dedupes on.
- `list_session_files(store)` → `db::listable_threads(store)`, stored into the provider's map keyed by `rollout_path`, returning those paths. On error, log nothing and return empty — a missing or unreadable database means no sessions, not a crash.
- `scan_file(path)` → look the path up in the map and build a `SessionMeta`: `agent: "codex"`, `id`, `path`, title by the precedence above, `cwd`, `branch`, `first_ts: created_at_ms`, `last_ts: recency_at_ms`, `first_prompt: first_user_message`, `msg_count: 0`.
- `messages(path)` / `may_contain(path, needle)` → Task 2.
- `launch_records(..)` → `Vec::new()` for now; Task 5 fills it in.
- `launch_plan(source, meta)` → argv `["codex", "resume", <id>]`, cwd from the session, no env changes.

`msg_count` is not in the index. Leave it 0 rather than opening every rollout to count — the list shows it, but paying a file read per session to populate it would throw away the reason for using the index.

- [ ] **Step 5: Verify**

Run: `cargo test --lib providers::codex` — expected PASS. Then `mise check`.

- [ ] **Step 6: Commit**

```bash
git add src/providers/codex/
git commit -m "codex: the provider, over Codex's index and rollout files"
```

---

### Task 4: Register it, and see it against real data

**Files:**
- Modify: `src/providers/mod.rs`, `src/config.rs`, `README.md`

- [ ] **Step 1: Register**

`providers::all()` returns `vec![Box::new(claude::ClaudeProvider), Box::new(codex::CodexProvider::default())]`. Claude stays first, so its sources keep priority in the default-source ordering.

- [ ] **Step 2: Write the failing test**

In `src/config.rs`'s tests, assert that a `[[source]]` with `agent = "codex"` is accepted and that `[codex] home = false` parses into the agents table. In `src/providers/mod.rs`, assert `all()` contains both ids exactly once:

```rust
#[test]
fn every_provider_is_registered_once() {
    let ids: Vec<&str> = all().iter().map(|p| p.id()).collect();
    assert_eq!(ids, vec!["claude", "codex"]);
}
```

- [ ] **Step 3: Implement, then look at real output**

Run against the developer's own machine — this is the first time the provider meets real data:

```bash
mise dev -- --sources
mise dev -- --list
```

Expected: a `codex` source appears with the state database as its store, and Codex sessions appear in the list beside Claude's with titles, cwds and dates. Compare the count against the database:

```bash
sqlite3 "file:$HOME/.codex/state_5.sqlite?mode=ro" \
  "select count(*) from threads where source in ('cli','vscode')"
```

Put both numbers in your report. If they disagree, say so rather than adjusting the filter to make them match — the difference is the finding.

- [ ] **Step 4: Document**

Add Codex to the README: what it lists, where it reads from, that subagent threads are excluded, and the rename-while-cached limitation from the top of this plan.

- [ ] **Step 5: Verify and commit**

```bash
mise check
git add src/providers/mod.rs src/config.rs README.md
git commit -m "codex: register the provider and document it"
```

---

### Task 5: Liveness

**Files:**
- Modify: `src/process.rs`, `src/providers/codex/mod.rs`
- Create: `src/providers/codex/live.rs`

**Interfaces:**
- Produces:
  - `ProcessProbe::open_files(&self, process_name: &str) -> Vec<(u32, PathBuf)>` — every (pid, open file) pair for processes whose command line contains `process_name`. Linux and WSL only; empty elsewhere.
  - `pub fn launch_records(threads: &[Thread], open: &[(u32, PathBuf)]) -> Vec<LaunchRecord>` in `codex/live.rs`.

- [ ] **Step 1: Write the failing tests**

For the probe, inject the enumeration so no test reads `/proc`:

```rust
#[test]
fn open_files_pairs_each_pid_with_what_it_has_open() {
    let probe = ProcessProbe::with_open_file_lister(Env::Linux, |_name| {
        vec![
            (42, PathBuf::from("/home/me/.codex/sessions/2026/09/18/rollout-a.jsonl")),
            (42, PathBuf::from("/home/me/.codex/state_5.sqlite")),
            (7, PathBuf::from("/home/me/.codex/sessions/2026/09/18/rollout-b.jsonl")),
        ]
    });
    let open = probe.open_files("codex");
    assert_eq!(open.len(), 3);
    assert!(open.iter().any(|(pid, p)| *pid == 7 && p.ends_with("rollout-b.jsonl")));
}

#[test]
fn a_host_with_no_proc_filesystem_reports_nothing() {
    let probe = ProcessProbe::new(Env::Windows);
    assert!(probe.open_files("codex").is_empty(), "no handle enumeration on Windows yet");
}
```

For the mapping:

```rust
#[test]
fn a_thread_whose_rollout_is_held_open_is_running() {
    let threads = vec![thread_at("id-a", "/s/rollout-a.jsonl"), thread_at("id-b", "/s/rollout-b.jsonl")];
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
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib process::tests::open_files providers::codex::live`
Expected: FAIL, `no method named open_files`.

- [ ] **Step 3: Implement the probe**

In `src/process.rs`, alongside the existing snapshot machinery. On Linux and WSL, walk `/proc`, keep pids whose `cmdline` contains the name (there is already a `linux_cmdline_contains` helper — reuse it rather than writing a second one), then read each `/proc/<pid>/fd/*` symlink. Skip anything unreadable: another user's processes are not ours to inspect and must not produce an error.

Follow the existing pattern in that file for injecting a test double (`with_windows_snapshotter` and friends), so the real enumeration is swapped out in tests.

- [ ] **Step 4: Implement the mapping and wire it up**

`codex/live.rs` matches open paths against `Thread::rollout_path` and emits a `LaunchRecord { pid, session_id, started_at_ms: 0, alive: true }` per match. The provider's `launch_records` calls `probe.open_files("codex")` and hands it the threads already loaded in Task 3's map.

- [ ] **Step 5: Verify against a real running session**

Ask the developer to leave a Codex session running, then:

```bash
mise dev -- --list
```

The running session must show a pid. Cross-check it:

```bash
pgrep -af codex
```

Put both in your report. Then `mise check`, `mise lint-windows`, `mise test-windows`.

- [ ] **Step 6: Commit**

```bash
git add src/process.rs src/providers/codex/
git commit -m "codex: detect running sessions from their open rollout files"
```

---

### Task 6: Focus, and the id-collision fix

**Files:**
- Modify: `src/catalog.rs`, `src/web/route.rs`, `src/ui/app.rs` (only if it needs it)

- [ ] **Step 1: Fix the lookup that a second provider makes reachable**

`Catalog::find_by_id` resolves a bare session id, while the catalog's own `launches`/`live` maps key on `(provider, id)`. With two providers registered that is now a real collision: a click in the web portal could act on the wrong provider's session. Change `find_by_id` to take the provider alongside the id, and update the three call sites in `src/web/route.rs`. The payload already carries what is needed, or add it — check `src/web/json.rs` first.

- [ ] **Step 2: Write the failing test**

```rust
#[test]
fn two_providers_sharing_a_session_id_resolve_separately() {
    // Build a catalog with one Claude and one Codex session that share an id, and assert
    // find_by_id returns the right one for each provider.
}
```

Build it with `providers::fake::FakeProvider` — see `catalog::build_fake`. If the fake provider cannot represent two providers, extend it; that is in scope.

- [ ] **Step 3: Confirm focusing needs no new code**

`focus_session(pid, domain, env, host, tab_title)` already takes everything needed. Codex sets its terminal title from the thread's `name`, so the tab title passed for a Codex session is that name. Check what `src/ui/app.rs` passes as the title today and make sure a Codex session passes its own title, not something Claude-shaped. Add a test at the `App` level asserting the Focus action carries the session's title.

- [ ] **Step 4: Verify**

Run `mise check`. Then, with a Codex session running, confirm in the TUI that it shows as running — do **not** press Enter on it as an agent; the developer verifies focusing by hand.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "catalog: resolve sessions per provider, now that there are two"
```

---

### Task 7: Documentation and a follow-up issue

**Files:**
- Modify: `README.md`, `AGENTS.md`

- [ ] **Step 1: README**

State plainly which agents are supported, that Codex sessions are read from Codex's own index, that subagent threads are not listed, and that running detection works on Linux and WSL but not on a Windows host. Do not oversell: someone reading this should be able to predict what they will see.

- [ ] **Step 2: AGENTS.md**

Extend the provider-boundary rule: it now has two implementations, so say that `src/providers/codex/` owns Codex's SQLite schema and rollout format and that nothing outside it may know either. Note that the database is opened read-only and that no code path may write to another agent's data.

- [ ] **Step 3: File the cache limitation**

Open a GitHub issue for the rename-while-cached behaviour described at the top of this plan: a Codex thread renamed without new messages keeps its old title until the next message or `--no-cache`. Describe the cause (the cache keys on the rollout file's stamp, but the title lives in the database) so whoever picks it up does not have to re-derive it.

- [ ] **Step 4: Verify and commit**

```bash
mise check
git add README.md AGENTS.md
git commit -m "docs: Codex support, and the provider boundary with two agents"
```
