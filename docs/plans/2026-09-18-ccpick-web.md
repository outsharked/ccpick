# ccpick web Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve ccpick's session list as a page on localhost, so a browser can search sessions, focus a running session's terminal, and resume a stopped one in a new terminal.

**Architecture:** A `src/web/` module beside `src/ui/`, sharing `Catalog` directly. A `Portal` holds an `Arc<Catalog>` behind an `RwLock` and a background thread republishes it on an interval. All request handling is a pure `route()` over `&Portal`; `tiny_http` appears in exactly one file, so the HTTP crate can be swapped by rewriting `server.rs`. The page, CSS and JS are embedded with `include_str!`.

**Tech Stack:** Rust 2024, `tiny_http` (blocking, thread-per-connection), `getrandom` (token), `serde_json`, existing `nucleo-matcher` and `rayon` search paths. No async runtime.

**Spec:** `docs/specs/2026-09-18-ccpick-web-design.md`

## Global Constraints

- **Run `mise check` before every commit.** It must pass: no fmt diff, no clippy warnings (`-D warnings`), all tests green, no compiler warnings.
- **The whole server sits behind a default-on `web` cargo feature.** `cargo build --no-default-features` must build the TUI with no HTTP dependency.
- **Bind `127.0.0.1` only.** No flag, option or config may bind any other address.
- **Never run the TUI as an agent.** Smoke-test with `mise dev -- --sources` / `--list`. For the portal use `--no-open` and `curl`; never leave a server running after a task.
- **Provider boundary:** nothing in `src/web/` may know Claude file formats, `ccs`, or `CLAUDE_CONFIG_DIR`. Go through `Catalog`, `Source` and the `Provider` trait.
- **Tests must not depend on the real environment:** use `fake_catalog()`, `catalog::build_fake`, tempdirs, and a pinned "now". No test may require a browser, a terminal emulator, or the developer's own sessions.
- **Windows-only code behind `cfg(windows)`, Unix-only behind `cfg(unix)`.** After touching either, run `mise lint-windows` and `mise test-windows` as well.
- **Public repo:** no personal paths, hostnames or usernames in code, tests or docs.

---

### Task 1: Extract catalog building so the refresh loop can reuse it

The portal rebuilds the catalog every few seconds. `main.rs` currently inlines that sequence (providers → markers → homes → `Catalog::build` → fold in home warnings), so it has to move somewhere both callers can reach.

**Files:**
- Modify: `src/catalog.rs` (add `build_from_settings`)
- Modify: `src/main.rs:40-58` (call it)

**Interfaces:**
- Consumes: `Catalog::build`, `homes::discover_homes`, `homes::running_distros`, `providers::all`.
- Produces: `pub fn build_from_settings(settings: &Settings, cache: &mut Cache) -> anyhow::Result<Catalog>` — discovers homes, builds the catalog, folds home warnings into `catalog.warnings`.

- [ ] **Step 1: Write the failing test**

In `src/catalog.rs`, inside `mod tests`:

```rust
#[test]
fn build_from_settings_discovers_nothing_in_an_empty_home() {
    let tmp = tempfile::tempdir().unwrap();
    let settings = crate::testutil::settings_with_home(tmp.path());
    let mut cache = Cache::in_memory();
    let catalog = build_from_settings(&settings, &mut cache).unwrap();
    assert!(catalog.sessions.is_empty());
    assert!(catalog.sources.is_empty());
}
```

If `crate::testutil` has no `settings_with_home`, read `src/testutil.rs` and use whatever helper builds a `Settings` with an empty `env` map and a tempdir home; the constraint is that the test must not see the developer's real config.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --lib catalog::tests::build_from_settings`
Expected: FAIL, `cannot find function build_from_settings`.

- [ ] **Step 3: Implement**

In `src/catalog.rs`:

```rust
/// Discovers homes and builds the catalog from settings. Shared by the CLI entry point and
/// the web portal's refresh loop, which rebuilds on an interval.
pub fn build_from_settings(settings: &Settings, cache: &mut Cache) -> anyhow::Result<Catalog> {
    let providers = crate::providers::all();
    let markers: Vec<&str> = providers
        .iter()
        .flat_map(|p| p.home_markers().iter().copied())
        .collect();
    let (homes, home_warnings) =
        crate::homes::discover_homes(settings, &markers, crate::homes::running_distros);
    let mut catalog = Catalog::build(providers, settings, &homes, cache)?;
    catalog.warnings.extend(home_warnings);
    Ok(catalog)
}
```

- [ ] **Step 4: Rewrite `main.rs` to use it**

Replace the block in `run()` from `let providers = providers::all();` through `catalog.warnings.extend(home_warnings);` with:

```rust
    let catalog = catalog::build_from_settings(&settings, &mut cache)?;
```

Remove any `use` statements that become unused (`homes`, `providers`) — clippy runs with `-D warnings`.

- [ ] **Step 5: Verify**

Run: `mise check`
Expected: all green. Then `mise dev -- --sources` must print the same source table as before the change.

- [ ] **Step 6: Commit**

```bash
git add src/catalog.rs src/main.rs
git commit -m "catalog: extract build_from_settings for reuse by the web portal"
```

---

### Task 2: The `web` feature, dependencies, and the portal's shared state

No HTTP yet. This task delivers the state the server will serve: a catalog that can be republished, a generation counter that only advances when something visible changed, and subscribers to notify.

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs`
- Create: `src/web/mod.rs`
- Create: `src/web/state.rs`

**Interfaces:**
- Consumes: `catalog::build_from_settings` (Task 1), `Catalog`.
- Produces:
  - `pub fn fingerprint(catalog: &Catalog) -> u64`
  - `pub struct Portal` with `pub fn new(catalog: Catalog, token: String) -> Portal`, `pub fn catalog(&self) -> Arc<Catalog>`, `pub fn generation(&self) -> u64`, `pub fn token(&self) -> &str`, `pub fn subscribe(&self) -> std::sync::mpsc::Receiver<u64>`, `pub fn publish(&self, catalog: Catalog) -> bool`

- [ ] **Step 1: Add the feature and dependencies**

In `Cargo.toml`:

```toml
[features]
default = ["web"]
web = ["dep:tiny_http", "dep:getrandom"]
```

and in `[dependencies]`:

```toml
getrandom = { version = "0.3.4", optional = true }
tiny_http = { version = "0.12.0", optional = true }
```

In `src/lib.rs`, beside the other module declarations:

```rust
#[cfg(feature = "web")]
pub mod web;
```

Create `src/web/mod.rs`:

```rust
//! A local web portal over the same catalog the TUI uses. Agent-neutral: nothing here may
//! know Claude file formats, ccs, or CLAUDE_CONFIG_DIR.
pub mod state;
```

- [ ] **Step 2: Write the failing tests**

Create `src/web/state.rs` with only a `mod tests` block for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake_catalog;

    #[test]
    fn publishing_an_identical_catalog_does_not_advance_the_generation() {
        let portal = Portal::new(fake_catalog(), "token".into());
        let before = portal.generation();
        assert!(!portal.publish(fake_catalog()), "nothing changed");
        assert_eq!(portal.generation(), before);
    }

    #[test]
    fn publishing_a_changed_catalog_advances_the_generation_and_notifies() {
        let portal = Portal::new(fake_catalog(), "token".into());
        let events = portal.subscribe();
        let mut changed = fake_catalog();
        changed.sessions[0].meta.title = "Renamed".into();
        assert!(portal.publish(changed));
        assert_eq!(portal.generation(), 1);
        assert_eq!(events.recv().unwrap(), 1);
        assert_eq!(portal.catalog().sessions[0].meta.title, "Renamed");
    }

    #[test]
    fn a_dropped_subscriber_is_forgotten_rather_than_failing_a_publish() {
        let portal = Portal::new(fake_catalog(), "token".into());
        drop(portal.subscribe());
        let mut changed = fake_catalog();
        changed.sessions[0].meta.title = "Renamed".into();
        assert!(portal.publish(changed));
    }

    #[test]
    fn the_fingerprint_covers_what_a_viewer_can_see() {
        let base = fingerprint(&fake_catalog());
        let mut other = fake_catalog();
        other.sessions[0].meta.title = "Renamed".into();
        assert_ne!(base, fingerprint(&other));
        let mut live = fake_catalog();
        live.sessions[0].live = Some((4242, 0));
        assert_ne!(base, fingerprint(&live));
        assert_eq!(base, fingerprint(&fake_catalog()));
    }
}
```

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test --lib web::state`
Expected: FAIL, `cannot find type Portal`.

- [ ] **Step 4: Implement**

At the top of `src/web/state.rs`:

```rust
//! What the portal serves: a catalog that can be republished, a generation that advances only
//! when a viewer would see a difference, and the subscribers to notify when it does.
use crate::catalog::Catalog;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, RwLock};

/// Everything a viewer can see, hashed. Two catalogs with the same fingerprint would render
/// identically, so republishing one must not wake idle tabs.
pub fn fingerprint(catalog: &Catalog) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    catalog.warnings.hash(&mut hasher);
    for source in &catalog.sources {
        source.name.hash(&mut hasher);
        source.env.id().hash(&mut hasher);
    }
    for session in &catalog.sessions {
        session.meta.id.hash(&mut hasher);
        session.meta.title.hash(&mut hasher);
        session.meta.last_ts.hash(&mut hasher);
        session.meta.msg_count.hash(&mut hasher);
        session.meta.cwd.hash(&mut hasher);
        session.live.hash(&mut hasher);
        session.default_source.hash(&mut hasher);
    }
    hasher.finish()
}

pub struct Portal {
    catalog: RwLock<Arc<Catalog>>,
    generation: AtomicU64,
    fingerprint: AtomicU64,
    subscribers: Mutex<Vec<Sender<u64>>>,
    token: String,
}

impl Portal {
    pub fn new(catalog: Catalog, token: String) -> Portal {
        let fingerprint = fingerprint(&catalog);
        Portal {
            catalog: RwLock::new(Arc::new(catalog)),
            generation: AtomicU64::new(0),
            fingerprint: AtomicU64::new(fingerprint),
            subscribers: Mutex::new(Vec::new()),
            token,
        }
    }

    pub fn catalog(&self) -> Arc<Catalog> {
        self.catalog.read().unwrap().clone()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// A receiver that yields the new generation each time one is published.
    pub fn subscribe(&self) -> Receiver<u64> {
        let (tx, rx) = channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Swaps in a new catalog. Returns whether it differed from the last one; only then does
    /// the generation advance and subscribers hear about it.
    pub fn publish(&self, catalog: Catalog) -> bool {
        let next = fingerprint(&catalog);
        *self.catalog.write().unwrap() = Arc::new(catalog);
        if next == self.fingerprint.swap(next, Ordering::Relaxed) {
            return false;
        }
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        // A closed receiver means that tab is gone; drop it rather than failing the publish.
        self.subscribers
            .lock()
            .unwrap()
            .retain(|tx| tx.send(generation).is_ok());
        true
    }
}
```

`Session::live` is `Option<(i32, usize)>` and `Source::env` is an `Env`; `Env` derives `Hash` already, and `env.id()` is used above so the fingerprint does not depend on enum layout.

- [ ] **Step 5: Verify**

Run: `cargo test --lib web::state` — expected PASS.
Run: `cargo build --no-default-features` — expected: builds, with no `tiny_http` or `getrandom` in the tree.
Run: `mise check` — expected: all green.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/web/
git commit -m "web: portal state, behind a default-on web feature"
```

---

### Task 3: The refresh loop

**Files:**
- Modify: `src/web/state.rs`

**Interfaces:**
- Consumes: `Portal::publish` (Task 2).
- Produces: `pub fn refresh_loop(portal: Arc<Portal>, every: Duration, build: impl Fn() -> anyhow::Result<Catalog> + Send + 'static, stop: Arc<AtomicBool>)` — rebuilds on an interval until `stop` is set. Callers pass a closure that calls `catalog::build_from_settings`; tests pass a fake.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn the_refresh_loop_publishes_until_it_is_stopped() {
        let portal = Arc::new(Portal::new(fake_catalog(), "token".into()));
        let events = portal.subscribe();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let calls = Arc::new(AtomicU64::new(0));
        let counter = calls.clone();
        let handle = {
            let portal = portal.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                refresh_loop(
                    portal,
                    std::time::Duration::from_millis(5),
                    move || {
                        let n = counter.fetch_add(1, Ordering::Relaxed);
                        let mut catalog = fake_catalog();
                        catalog.sessions[0].meta.title = format!("Build {n}");
                        Ok(catalog)
                    },
                    stop,
                )
            })
        };
        assert_eq!(events.recv().unwrap(), 1);
        assert_eq!(events.recv().unwrap(), 2);
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
        assert!(calls.load(Ordering::Relaxed) >= 2);
    }

    #[test]
    fn a_failing_rebuild_leaves_the_last_good_catalog_in_place() {
        let portal = Arc::new(Portal::new(fake_catalog(), "token".into()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(true));
        refresh_loop(
            portal.clone(),
            std::time::Duration::from_millis(1),
            || anyhow::bail!("disk went away"),
            stop,
        );
        assert_eq!(portal.generation(), 0);
        assert!(!portal.catalog().sessions.is_empty());
    }
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test --lib web::state::tests::the_refresh_loop`
Expected: FAIL, `cannot find function refresh_loop`.

- [ ] **Step 3: Implement**

```rust
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Rebuilds the catalog every `every` until `stop` is set, publishing each result. Runs one
/// rebuild before checking `stop`, so a caller can drive exactly one pass in a test.
///
/// A rebuild that fails is reported and skipped: the last good catalog keeps serving, because
/// a transient failure (an unmounted drive, a distro shutting down) must not empty the page.
pub fn refresh_loop(
    portal: Arc<Portal>,
    every: Duration,
    build: impl Fn() -> anyhow::Result<Catalog>,
    stop: Arc<AtomicBool>,
) {
    loop {
        match build() {
            Ok(catalog) => {
                portal.publish(catalog);
            }
            Err(err) => eprintln!("ccpick: warning: could not refresh sessions: {err}"),
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(every);
        if stop.load(Ordering::Relaxed) {
            return;
        }
    }
}
```

A tick cannot overlap the previous one because the rebuild is synchronous inside the loop, which is what the spec means by "skipping a tick if the previous one is still running".

- [ ] **Step 4: Verify**

Run: `cargo test --lib web::state` — expected PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web/state.rs
git commit -m "web: refresh loop that keeps the last good catalog on failure"
```

---

### Task 4: JSON payloads

Pure functions from a catalog to `serde_json::Value`. No HTTP, no state.

**Files:**
- Create: `src/web/json.rs`
- Modify: `src/web/mod.rs` (add `pub mod json;`)

**Interfaces:**
- Consumes: `Catalog`, `catalog.host_cwd`, `catalog.is_launchable`, `format::friendly`, `format::exact`, `format::shorten_home_in`, `shell::resume_command`, `catalog.launch_plan`, `catalog.messages`.
- Produces:
  - `pub fn sessions_payload(catalog: &Catalog, generation: u64, now_ms: i64) -> serde_json::Value`
  - `pub fn messages_payload(catalog: &Catalog, idx: usize) -> serde_json::Value`

- [ ] **Step 1: Write the failing tests**

```rust
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
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib web::json`
Expected: FAIL, `cannot find function sessions_payload`.

- [ ] **Step 3: Implement**

```rust
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
            let source_idx = session.live.map(|(_, s)| s).unwrap_or(session.default_source);
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
```

Check `src/shell.rs:5` and `src/catalog.rs:200` for the exact argument order of `resume_command` and `launch_plan` before writing this; if they differ from the above, follow the source, not the plan.

- [ ] **Step 4: Verify**

Run: `cargo test --lib web::json` — expected PASS. Then `mise check`.

- [ ] **Step 5: Commit**

```bash
git add src/web/
git commit -m "web: JSON payloads for the session list and the preview"
```

---

### Task 5: Routing, auth and the read-only endpoints

The heart of the server, and all of it testable without a socket.

**Files:**
- Create: `src/web/route.rs`
- Modify: `src/web/mod.rs` (add `pub mod route;`)

**Interfaces:**
- Consumes: `Portal` (Task 2), `sessions_payload` / `messages_payload` (Task 4), `search::fuzzy`, `search::full_text`.
- Produces:
  - `pub struct Req<'a> { pub method: &'a str, pub path: &'a str, pub query: &'a str, pub token: Option<&'a str>, pub origin: Option<&'a str>, pub body: &'a [u8] }`
  - `pub struct Res { pub status: u16, pub content_type: String, pub body: Vec<u8> }`
  - `impl Res { pub fn json(status: u16, value: serde_json::Value) -> Res; pub fn error(status: u16, message: &str) -> Res; pub fn text(status: u16, content_type: &str, body: &str) -> Res }`
  - `pub fn route(req: &Req, portal: &Portal) -> Res`
  - `pub fn query_param(query: &str, key: &str) -> Option<String>`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake_catalog;

    fn portal() -> Portal {
        Portal::new(fake_catalog(), "secret".into())
    }

    fn get<'a>(path: &'a str, query: &'a str) -> Req<'a> {
        Req { method: "GET", path, query, token: Some("secret"), origin: None, body: b"" }
    }

    #[test]
    fn a_request_without_the_token_is_refused() {
        let mut req = get("/api/sessions", "");
        req.token = None;
        assert_eq!(route(&req, &portal()).status, 401);
        req.token = Some("wrong");
        assert_eq!(route(&req, &portal()).status, 401);
    }

    #[test]
    fn a_request_from_another_origin_is_refused_even_with_the_token() {
        let mut req = get("/api/sessions", "");
        req.origin = Some("https://evil.example");
        assert_eq!(route(&req, &portal()).status, 403);
    }

    #[test]
    fn the_page_is_served_without_a_token_because_the_token_arrives_in_its_url() {
        let req = Req { method: "GET", path: "/", query: "", token: None, origin: None, body: b"" };
        let res = route(&req, &portal());
        assert_eq!(res.status, 200);
        assert!(res.content_type.starts_with("text/html"));
    }

    #[test]
    fn the_session_list_is_served_as_json() {
        let res = route(&get("/api/sessions", ""), &portal());
        assert_eq!(res.status, 200);
        let value: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
        assert!(value["sessions"].as_array().unwrap().len() > 1);
    }

    #[test]
    fn search_ranks_with_the_same_matcher_the_tui_uses() {
        let res = route(&get("/api/search", "q=docker"), &portal());
        let value: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
        let matches = value["matches"].as_array().unwrap();
        assert!(!matches.is_empty());
        let catalog = fake_catalog();
        let first = matches[0].as_u64().unwrap() as usize;
        assert_eq!(catalog.sessions[first].meta.title, "Docker build cache");
    }

    #[test]
    fn search_also_returns_hits_from_inside_conversations() {
        let res = route(&get("/api/search", "q=PINEAPPLE"), &portal());
        let value: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
        let hits = value["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0]["snippet"].as_str().unwrap().contains("PINEAPPLE"));
    }

    #[test]
    fn messages_are_served_for_a_session_index() {
        let res = route(&get("/api/messages", "index=0"), &portal());
        assert_eq!(res.status, 200);
        let value: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
        assert!(value["messages"].is_array());
    }

    #[test]
    fn an_out_of_range_index_is_a_404_not_a_panic() {
        assert_eq!(route(&get("/api/messages", "index=999"), &portal()).status, 404);
        assert_eq!(route(&get("/api/messages", ""), &portal()).status, 400);
    }

    #[test]
    fn an_unknown_path_is_a_404() {
        assert_eq!(route(&get("/api/nope", ""), &portal()).status, 404);
    }

    #[test]
    fn query_params_are_decoded() {
        assert_eq!(query_param("q=a%20b&x=1", "q").as_deref(), Some("a b"));
        assert_eq!(query_param("q=a+b", "q").as_deref(), Some("a b"));
        assert_eq!(query_param("x=1", "q"), None);
        assert_eq!(query_param("", "q"), None);
    }
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib web::route`
Expected: FAIL, `cannot find type Req`.

- [ ] **Step 3: Implement**

```rust
//! Request handling as a pure function over the portal: no sockets, no HTTP crate. `server.rs`
//! adapts a real request into `Req` and writes `Res` back, so the HTTP library is swappable.
use crate::web::json::{messages_payload, sessions_payload};
use crate::web::state::Portal;
use serde_json::{Value, json};

pub struct Req<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub token: Option<&'a str>,
    pub origin: Option<&'a str>,
    pub body: &'a [u8],
}

pub struct Res {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Res {
    pub fn json(status: u16, value: Value) -> Res {
        Res {
            status,
            content_type: "application/json; charset=utf-8".into(),
            body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    pub fn error(status: u16, message: &str) -> Res {
        Res::json(status, json!({ "error": message }))
    }

    pub fn text(status: u16, content_type: &str, body: &str) -> Res {
        Res {
            status,
            content_type: content_type.into(),
            body: body.as_bytes().to_vec(),
        }
    }
}

/// `a=1&b=two%20words`, with `+` meaning a space as in a form-encoded query.
pub fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(&v.replace('+', " ")))
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn route(req: &Req, portal: &Portal) -> Res {
    // The page itself carries no token: the token arrives in its URL and the script it loads
    // sends it on every call after that.
    if req.path == "/" && req.method == "GET" {
        return Res::text(200, "text/html; charset=utf-8", crate::web::PAGE);
    }
    // A page on another origin must not be able to reach a server that can spawn processes,
    // even if it somehow learned the token.
    if let Some(origin) = req.origin
        && !origin.is_empty()
    {
        return Res::error(403, "cross-origin requests are refused");
    }
    if req.token != Some(portal.token()) {
        return Res::error(401, "missing or invalid token");
    }
    let catalog = portal.catalog();
    match (req.method, req.path) {
        ("GET", "/api/sessions") => Res::json(
            200,
            sessions_payload(&catalog, portal.generation(), crate::format::now_ms()),
        ),
        ("GET", "/api/search") => {
            let query = query_param(req.query, "q").unwrap_or_default();
            let all: Vec<usize> = (0..catalog.sessions.len()).collect();
            let matches = crate::search::fuzzy(&catalog, &all, &query);
            let hits: Vec<Value> = crate::search::full_text(&catalog, &all, &query, None)
                .unwrap_or_default()
                .into_iter()
                .map(|hit| json!({ "index": hit.session, "snippet": hit.snippet }))
                .collect();
            Res::json(200, json!({ "matches": matches, "hits": hits }))
        }
        ("GET", "/api/messages") => match query_param(req.query, "index") {
            None => Res::error(400, "index is required"),
            Some(raw) => match raw.parse::<usize>() {
                Ok(idx) if idx < catalog.sessions.len() => {
                    Res::json(200, messages_payload(&catalog, idx))
                }
                _ => Res::error(404, "no such session"),
            },
        },
        _ => Res::error(404, "not found"),
    }
}
```

Add to `src/web/mod.rs`:

```rust
pub mod json;
pub mod route;
pub mod state;

/// The page, embedded so the binary is self-contained.
pub const PAGE: &str = include_str!("assets/index.html");
```

and create a placeholder `src/web/assets/index.html` containing `<!doctype html><title>ccpick</title>` for now — Task 9 replaces it.

- [ ] **Step 4: Verify**

Run: `cargo test --lib web::route` — expected PASS. Then `mise check`.

- [ ] **Step 5: Commit**

```bash
git add src/web/
git commit -m "web: routing, token and origin checks, and the read-only endpoints"
```

---

### Task 6: Opening a session in a new terminal

**Files:**
- Modify: `src/launch.rs`

**Interfaces:**
- Consumes: `LaunchPlan`, `Env`, `HostContext`, `launch::find_in_path`, `shell::posix_quote`, `shell::powershell_quote`.
- Produces:
  - `pub fn terminal_command(plan: &LaunchPlan, target: &Env, host: &HostContext, terminal_env: Option<&str>, on_path: &dyn Fn(&str) -> bool) -> Option<Vec<String>>`
  - `pub fn spawn_in_new_terminal(plan: &LaunchPlan, target: &Env, host: &HostContext) -> Result<(), String>`

- [ ] **Step 1: Write the failing tests**

```rust
    fn plan() -> LaunchPlan {
        LaunchPlan {
            cwd: PathBuf::from("/home/me/proj"),
            argv: vec!["ccs".into(), "c2".into(), "--resume".into(), "abc".into()],
            env_set: vec![],
            env_remove: vec![],
        }
    }
    fn wsl_host() -> crate::env::HostContext {
        crate::env::HostContext {
            env: crate::env::Env::Wsl { distro: "Ubuntu".into() },
            wsl_mount_root: PathBuf::from("/mnt/"),
        }
    }
    fn nothing_on_path(_: &str) -> bool { false }

    #[test]
    fn a_windows_session_opens_in_windows_terminal() {
        let host = crate::env::HostContext { env: crate::env::Env::Windows, ..Default::default() };
        let argv = terminal_command(&plan(), &crate::env::Env::Windows, &host, None, &|name| name == "wt.exe").unwrap();
        assert_eq!(argv[0], "wt.exe");
        assert!(argv.iter().any(|a| a == "--resume"));
    }

    #[test]
    fn without_windows_terminal_it_falls_back_to_cmd_start() {
        let host = crate::env::HostContext { env: crate::env::Env::Windows, ..Default::default() };
        let argv = terminal_command(&plan(), &crate::env::Env::Windows, &host, None, &nothing_on_path).unwrap();
        assert_eq!(argv[0], "cmd.exe");
        assert_eq!(argv[1], "/c");
        assert_eq!(argv[2], "start");
    }

    #[test]
    fn a_wsl_session_from_windows_goes_through_wsl_exe() {
        let host = crate::env::HostContext { env: crate::env::Env::Windows, ..Default::default() };
        let target = crate::env::Env::Wsl { distro: "Ubuntu".into() };
        let argv = terminal_command(&plan(), &target, &host, None, &|name| name == "wt.exe").unwrap();
        assert_eq!(argv[0], "wt.exe");
        assert!(argv.windows(3).any(|w| w == ["wsl.exe", "-d", "Ubuntu"]));
    }

    #[test]
    fn the_terminal_env_var_wins_on_linux() {
        let host = crate::env::HostContext::default();
        let argv = terminal_command(&plan(), &crate::env::Env::Linux, &host, Some("kitty"), &nothing_on_path).unwrap();
        assert_eq!(argv[0], "kitty");
    }

    #[test]
    fn linux_falls_back_to_x_terminal_emulator_then_gives_up() {
        let host = crate::env::HostContext::default();
        let argv = terminal_command(&plan(), &crate::env::Env::Linux, &host, None, &|name| name == "x-terminal-emulator").unwrap();
        assert_eq!(argv[0], "x-terminal-emulator");
        // Nothing installed means no command: the caller shows the paste-ready line instead.
        assert_eq!(terminal_command(&plan(), &crate::env::Env::Linux, &host, None, &nothing_on_path), None);
    }

    #[test]
    fn a_wsl_session_from_wsl_has_no_terminal_to_open() {
        // ccpick can't open a Windows terminal window from inside the distro reliably;
        // the caller falls back to the paste command.
        assert_eq!(
            terminal_command(&plan(), &crate::env::Env::Wsl { distro: "Ubuntu".into() }, &wsl_host(), None, &nothing_on_path),
            None
        );
    }

    #[test]
    fn macos_opens_terminal_with_a_script() {
        let host = crate::env::HostContext { env: crate::env::Env::MacOs, ..Default::default() };
        let argv = terminal_command(&plan(), &crate::env::Env::MacOs, &host, None, &nothing_on_path).unwrap();
        assert_eq!(argv[0], "osascript");
        assert!(argv.last().unwrap().contains("--resume"));
    }
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib launch::tests::a_windows_session_opens`
Expected: FAIL, `cannot find function terminal_command`.

- [ ] **Step 3: Implement**

```rust
/// The command line that opens `plan` in a *new* terminal window, or None when this platform
/// has no terminal ccpick is willing to guess at — the caller then offers the paste-ready
/// command instead of opening something the user didn't ask for.
///
/// `terminal_env` is `$TERMINAL`; `on_path` reports whether a program is runnable, so every
/// platform's decision can be tested on every platform.
pub fn terminal_command(
    plan: &LaunchPlan,
    target: &Env,
    host: &HostContext,
    terminal_env: Option<&str>,
    on_path: &dyn Fn(&str) -> bool,
) -> Option<Vec<String>> {
    let quoted_posix = plan
        .argv
        .iter()
        .map(|a| crate::shell::posix_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    match (&host.env, target) {
        (Env::Windows, Env::Windows) => {
            let mut argv = windows_terminal_prefix(on_path);
            argv.extend(plan.argv.iter().cloned());
            Some(argv)
        }
        (Env::Windows, Env::Wsl { distro }) => {
            let mut argv = windows_terminal_prefix(on_path);
            argv.extend(["wsl.exe".to_string(), "-d".to_string(), distro.clone()]);
            argv.push("--".to_string());
            argv.extend(plan.argv.iter().cloned());
            Some(argv)
        }
        (Env::MacOs, _) => Some(vec![
            "osascript".to_string(),
            "-e".to_string(),
            format!(
                "tell application \"Terminal\" to do script {}",
                applescript_string(&format!(
                    "cd {} && {quoted_posix}",
                    crate::shell::posix_quote(&plan.cwd.display().to_string())
                ))
            ),
        ]),
        (Env::Linux, Env::Linux) => {
            let chosen = terminal_env
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    ["x-terminal-emulator", "gnome-terminal", "konsole", "alacritty", "kitty", "xterm"]
                        .into_iter()
                        .find(|name| on_path(name))
                        .map(str::to_string)
                })?;
            let mut argv = vec![chosen, "-e".to_string()];
            argv.extend(plan.argv.iter().cloned());
            Some(argv)
        }
        // From inside WSL there is no terminal ccpick can open for another environment.
        _ => None,
    }
}

fn windows_terminal_prefix(on_path: &dyn Fn(&str) -> bool) -> Vec<String> {
    if on_path("wt.exe") {
        vec!["wt.exe".to_string()]
    } else {
        vec!["cmd.exe".to_string(), "/c".to_string(), "start".to_string(), String::new()]
    }
}

/// An AppleScript string literal.
fn applescript_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Opens `plan` in a new terminal window. Err carries a message for the user.
pub fn spawn_in_new_terminal(
    plan: &LaunchPlan,
    target: &Env,
    host: &HostContext,
) -> Result<(), String> {
    let terminal_env = std::env::var("TERMINAL").ok();
    let argv = terminal_command(plan, target, host, terminal_env.as_deref(), &|name| {
        find_in_path(name).is_some()
    })
    .ok_or("no terminal to open on this system")?;
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).current_dir(&plan.cwd);
    for key in &plan.env_remove {
        command.env_remove(key);
    }
    for (key, value) in &plan.env_set {
        command.env(key, value);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open a terminal: {e}"))
}
```

Add `use crate::env::{Env, HostContext};` and `use crate::model::LaunchPlan;` at the top of `launch.rs` if not already present. `find_in_path` already exists in this file — check its exact signature and use it as-is.

The `cmd.exe /c start ""` form needs the empty title argument, which is why `windows_terminal_prefix` pushes `String::new()`.

- [ ] **Step 4: Verify**

Run: `cargo test --lib launch` — expected PASS.
Run: `mise check`, then `mise lint-windows` and `mise test-windows` (this touches platform-dependent code).

- [ ] **Step 5: Commit**

```bash
git add src/launch.rs
git commit -m "launch: open a session in a new terminal window"
```

---

### Task 7: The acting endpoints

**Files:**
- Modify: `src/web/route.rs`

**Interfaces:**
- Consumes: `terminal_command` / `spawn_in_new_terminal` (Task 6), `focus::focus_session`, `process::PidDomain`.
- Produces: `POST /api/launch` and `POST /api/focus` handling inside `route()`, plus `pub fn body_index(body: &[u8]) -> Option<usize>`.

- [ ] **Step 1: Write the failing tests**

```rust
    fn post<'a>(path: &'a str, body: &'a [u8]) -> Req<'a> {
        Req { method: "POST", path, query: "", token: Some("secret"), origin: None, body }
    }

    #[test]
    fn focusing_a_session_that_is_not_running_is_a_conflict_not_an_action() {
        let res = route(&post("/api/focus", br#"{"index":0}"#), &portal());
        assert_eq!(res.status, 409);
    }

    #[test]
    fn launching_a_session_ccpick_cannot_reach_returns_the_command_to_paste() {
        let portal = Portal::new(crate::catalog::fake_catalog_with_foreign(), "secret".into());
        let catalog = portal.catalog();
        let idx = catalog.sessions.iter().position(|s| s.meta.id == "w").unwrap();
        let body = format!("{{\"index\":{idx}}}");
        let res = route(&post("/api/launch", body.as_bytes()), &portal);
        assert_eq!(res.status, 409);
        let value: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
        assert!(value["command"].as_str().unwrap().contains("--resume"));
    }

    #[test]
    fn a_malformed_body_is_a_400() {
        assert_eq!(route(&post("/api/launch", b"not json"), &portal()).status, 400);
        assert_eq!(route(&post("/api/launch", br#"{"index":999}"#), &portal()).status, 404);
    }

    #[test]
    fn parses_the_index_out_of_a_body() {
        assert_eq!(body_index(br#"{"index":12}"#), Some(12));
        assert_eq!(body_index(br#"{"other":1}"#), None);
        assert_eq!(body_index(b""), None);
    }
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib web::route::tests::focusing`
Expected: FAIL, `cannot find function body_index`.

- [ ] **Step 3: Implement**

Add to `src/web/route.rs`:

```rust
pub fn body_index(body: &[u8]) -> Option<usize> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("index")?
        .as_u64()
        .map(|n| n as usize)
}
```

and two arms in the `match`, before the `_ => Res::error(404, ...)`:

```rust
        ("POST", "/api/focus") => {
            let Some(idx) = body_index(req.body) else {
                return Res::error(400, "index is required");
            };
            let Some(session) = catalog.sessions.get(idx) else {
                return Res::error(404, "no such session");
            };
            let Some((pid, source_idx)) = session.live else {
                return Res::error(409, "that session isn't running");
            };
            let env = catalog.sources[source_idx].env.clone();
            match crate::process::PidDomain::of(&env) {
                None => Res::error(409, "no process model for this environment"),
                Some(domain) => match crate::focus::focus_session(
                    pid as u32,
                    domain,
                    &env,
                    &catalog.host,
                    Some(&session.meta.title),
                ) {
                    Ok(_) => Res::json(200, json!({ "ok": true })),
                    Err(error) => Res::json(409, json!({ "error": error })),
                },
            }
        }
        ("POST", "/api/launch") => {
            let Some(idx) = body_index(req.body) else {
                return Res::error(400, "index is required");
            };
            let Some(session) = catalog.sessions.get(idx) else {
                return Res::error(404, "no such session");
            };
            let source_idx = session.default_source;
            let plan = catalog.launch_plan(idx, source_idx);
            let env = catalog.sources[source_idx].env.clone();
            // Anything ccpick can't open itself comes back with the line to paste, so the page
            // always has something to offer.
            let fallback = |error: String| {
                Res::json(
                    409,
                    json!({
                        "error": error,
                        "command": crate::shell::resume_command(&plan, &env),
                        "shell": env.shell_name(),
                    }),
                )
            };
            if !catalog.is_launchable(source_idx) {
                return fallback("ccpick can't start this session from here".into());
            }
            match crate::launch::spawn_in_new_terminal(&plan, &env, &catalog.host) {
                Ok(()) => Res::json(200, json!({ "ok": true })),
                Err(error) => fallback(error),
            }
        }
```

`focus_session` takes `(pid, domain, env, host, tab_title)` — confirm against `src/focus.rs` before writing, and follow the source if it has changed.

- [ ] **Step 4: Verify**

Run: `cargo test --lib web::route` — expected PASS. Then `mise check`.

Note for the implementer: no test may actually spawn a terminal. The tests above only exercise paths that stop before spawning; do not add a test that reaches `spawn_in_new_terminal`.

- [ ] **Step 5: Commit**

```bash
git add src/web/route.rs
git commit -m "web: focus and launch endpoints, with the paste command as the fallback"
```

---

### Task 8: The server, the CLI, and live updates

**Files:**
- Create: `src/web/server.rs`
- Modify: `src/web/mod.rs`, `src/main.rs`
- Create: `tests/web_server.rs`

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `pub fn sse_event(generation: u64) -> String`
  - `pub fn mint_token() -> String`
  - `pub struct WebOptions { pub port: u16, pub refresh: u64, pub open: bool }`
  - `pub fn run(settings: Settings, cache: Cache, options: WebOptions) -> anyhow::Result<()>`

- [ ] **Step 1: Write the failing tests**

In `src/web/server.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_is_encoded_as_one_sse_frame() {
        assert_eq!(sse_event(7), "event: generation\ndata: 7\n\n");
    }

    #[test]
    fn a_token_is_long_enough_to_be_unguessable_and_differs_each_time() {
        let token = mint_token();
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(token, mint_token());
    }
}
```

and an integration test in `tests/web_server.rs`:

```rust
//! Binds a real socket on an ephemeral port and checks the server answers.
#![cfg(feature = "web")]

use ccpick::catalog::fake_catalog;
use ccpick::web::server::serve_for_test;
use ccpick::web::state::Portal;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;

fn request(port: u16, line: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.write_all(line.as_bytes()).unwrap();
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    let code: u16 = status.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut rest = String::new();
    for line in reader.lines() {
        rest.push_str(&line.unwrap());
        rest.push('\n');
    }
    (code, rest)
}

#[test]
fn the_server_requires_the_token() {
    let portal = Arc::new(Portal::new(fake_catalog(), "secret".into()));
    let (port, stop) = serve_for_test(portal).unwrap();

    let (code, _) = request(port, "GET /api/sessions HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(code, 401);

    let (code, body) = request(
        port,
        "GET /api/sessions HTTP/1.1\r\nHost: localhost\r\nX-CCPick-Token: secret\r\n\r\n",
    );
    assert_eq!(code, 200);
    assert!(body.contains("sessions"));

    stop();
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --test web_server`
Expected: FAIL, `unresolved import ccpick::web::server`.

- [ ] **Step 3: Implement the server**

```rust
//! The only module that knows about the HTTP crate. Everything else works through `route()`.
use crate::web::route::{Req, Res, route};
use crate::web::state::Portal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub fn sse_event(generation: u64) -> String {
    format!("event: generation\ndata: {generation}\n\n")
}

/// 128 bits of randomness, hex encoded.
pub fn mint_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the OS must provide randomness for the portal's token");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn header<'a>(request: &'a tiny_http::Request, name: &str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str())
}

/// Serves one request. SSE is handled separately because it never completes.
fn handle(mut request: tiny_http::Request, portal: &Portal) {
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let method = request.method().as_str().to_string();
    let mut body = Vec::new();
    let _ = std::io::Read::read_to_end(request.as_reader(), &mut body);
    let token = header(&request, "X-CCPick-Token")
        .map(str::to_string)
        .or_else(|| crate::web::route::query_param(query, "t"));
    let origin = header(&request, "Origin").map(str::to_string);

    let res = route(
        &Req {
            method: &method,
            path,
            query,
            token: token.as_deref(),
            origin: origin.as_deref(),
            body: &body,
        },
        portal,
    );
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], res.content_type.as_bytes())
        .expect("content types are valid header values");
    let response = tiny_http::Response::from_data(res.body)
        .with_status_code(res.status)
        .with_header(header);
    let _ = request.respond(response);
}

/// Binds, serves until `stop` is set, and returns. `port` 0 asks the OS for a free one.
pub fn serve(portal: Arc<Portal>, port: u16, stop: Arc<AtomicBool>) -> anyhow::Result<u16> {
    let server = tiny_http::Server::http(("127.0.0.1", port))
        .map_err(|e| anyhow::anyhow!("could not bind 127.0.0.1:{port}: {e}"))?;
    let bound = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(port);
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let portal = portal.clone();
            std::thread::spawn(move || handle(request, &portal));
        }
    });
    Ok(bound)
}

/// Binds on an ephemeral port for tests and hands back the port and a stopper.
#[doc(hidden)]
pub fn serve_for_test(portal: Arc<Portal>) -> anyhow::Result<(u16, impl FnOnce())> {
    let stop = Arc::new(AtomicBool::new(false));
    let port = serve(portal, 0, stop.clone())?;
    Ok((port, move || stop.store(true, Ordering::Relaxed)))
}
```

SSE: add an arm in `handle` before calling `route` — when `path == "/api/events"` and the token matches, subscribe and stream:

```rust
    if path == "/api/events" && token.as_deref() == Some(portal.token()) {
        let events = portal.subscribe();
        let mut writer = request.into_writer();
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                    Cache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
        if writer.write_all(head.as_bytes()).is_err() {
            return;
        }
        // Ends when the browser closes the connection, which shows up as a write error.
        while let Ok(generation) = events.recv() {
            if writer.write_all(sse_event(generation).as_bytes()).is_err() {
                return;
            }
            let _ = writer.flush();
        }
        return;
    }
```

with `use std::io::Write;` at the top.

**Verify `into_writer` against the installed `tiny_http` before relying on it.** Run
`cargo doc --open -p tiny_http` or read the source. If the method is absent or shaped
differently in the pinned version, serve the stream with
`tiny_http::Response::empty(200).with_header(...)` over a reader that blocks on the subscriber
channel instead. The requirement is the behaviour — one SSE frame per published generation,
ending when the browser disconnects — not this particular call.

- [ ] **Step 4: Implement `run` and the CLI**

In `src/web/mod.rs`:

```rust
pub mod json;
pub mod route;
pub mod server;
pub mod state;

use crate::cache::Cache;
use crate::config::Settings;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub const PAGE: &str = include_str!("assets/index.html");

pub struct WebOptions {
    pub port: u16,
    pub refresh: u64,
    pub open: bool,
}

/// Runs the portal until interrupted.
pub fn run(settings: Settings, mut cache: Cache, options: WebOptions) -> anyhow::Result<()> {
    let catalog = crate::catalog::build_from_settings(&settings, &mut cache)?;
    let token = server::mint_token();
    let portal = Arc::new(state::Portal::new(catalog, token.clone()));
    let stop = Arc::new(AtomicBool::new(false));
    let port = server::serve(portal.clone(), options.port, stop.clone())?;
    let url = format!("http://127.0.0.1:{port}/?t={token}");
    println!("ccpick: serving at {url}");
    if options.open {
        open_browser(&url);
    }
    // The cache is shared across refreshes on purpose: it is what makes a quiet tick cheap.
    // Rebuilding with a fresh in-memory cache every 10 seconds would rescan every transcript.
    let cache = Arc::new(std::sync::Mutex::new(cache));
    let refresh_settings = settings.clone();
    let refresh_cache = cache.clone();
    state::refresh_loop(
        portal,
        Duration::from_secs(options.refresh.max(1)),
        move || {
            let mut cache = refresh_cache.lock().unwrap();
            crate::catalog::build_from_settings(&refresh_settings, &mut cache)
        },
        stop,
    );
    let _ = cache.lock().unwrap().save();
    Ok(())
}

fn open_browser(url: &str) {
    let argv: Vec<&str> = if cfg!(target_os = "macos") {
        vec!["open", url]
    } else if cfg!(windows) {
        vec!["cmd.exe", "/c", "start", "", url]
    } else {
        vec!["xdg-open", url]
    };
    let _ = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
```

`Settings` must be `Clone` for the refresh closure; if it isn't, derive `Clone` on it and on its fields in `src/config.rs`.

In `src/main.rs`, add the subcommand to `Cli`:

```rust
    #[command(subcommand)]
    command: Option<Command>,
```

```rust
#[derive(clap::Subcommand)]
enum Command {
    /// Serve the session list as a local web page
    Web {
        /// Port to listen on (default: chosen by the OS)
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Seconds between refreshes
        #[arg(long, default_value_t = 10)]
        refresh: u64,
        /// Print the URL instead of opening a browser
        #[arg(long)]
        no_open: bool,
    },
}
```

and in `run()`, after settings and cache are built but before the catalog:

```rust
    #[cfg(feature = "web")]
    if let Some(Command::Web { port, refresh, no_open }) = cli.command {
        ccpick::web::run(
            settings,
            cache,
            ccpick::web::WebOptions { port, refresh, open: !no_open },
        )?;
        return Ok(0);
    }
```

- [ ] **Step 5: Verify**

Run: `cargo test --lib web::server && cargo test --test web_server` — expected PASS.
Run: `cargo build --no-default-features` — expected: builds.
Run a real smoke test, and stop it:

```bash
cargo run -- web --no-open --port 8731 &
sleep 2
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8731/api/sessions   # expect 401
kill %1
```

Run: `mise check`.

- [ ] **Step 6: Commit**

```bash
git add src/web/ src/main.rs tests/web_server.rs
git commit -m "web: tiny_http server, SSE, and the ccpick web subcommand"
```

---

### Task 9: The page

**Files:**
- Modify: `src/web/assets/index.html`
- Create: `src/web/assets/app.css`, `src/web/assets/app.js`
- Modify: `src/web/mod.rs`, `src/web/route.rs`

**Interfaces:**
- Consumes: every endpoint above.
- Produces: `GET /app.css` and `GET /app.js` routes; `pub const STYLE: &str` and `pub const SCRIPT: &str` in `src/web/mod.rs`.

- [ ] **Step 1: Write the failing tests**

In `src/web/route.rs` tests:

```rust
    #[test]
    fn the_page_and_its_assets_are_served() {
        for (path, kind) in [("/", "text/html"), ("/app.css", "text/css"), ("/app.js", "text/javascript")] {
            let req = Req { method: "GET", path, query: "", token: None, origin: None, body: b"" };
            let res = route(&req, &portal());
            assert_eq!(res.status, 200, "{path}");
            assert!(res.content_type.starts_with(kind), "{path}");
            assert!(!res.body.is_empty(), "{path}");
        }
    }

    #[test]
    fn the_page_asks_for_the_token_to_be_stripped_from_the_address_bar() {
        let req = Req { method: "GET", path: "/app.js", query: "", token: None, origin: None, body: b"" };
        let body = String::from_utf8(route(&req, &portal()).body).unwrap();
        assert!(body.contains("replaceState"));
        assert!(body.contains("X-CCPick-Token"));
    }
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib web::route::tests::the_page_and_its_assets`
Expected: FAIL, 404 for `/app.css`.

- [ ] **Step 3: Add the routes**

In `src/web/mod.rs`:

```rust
pub const STYLE: &str = include_str!("assets/app.css");
pub const SCRIPT: &str = include_str!("assets/app.js");
```

In `route()`, beside the `/` arm (all three before the token check, since the browser fetches them without one):

```rust
    if req.method == "GET" {
        match req.path {
            "/" => return Res::text(200, "text/html; charset=utf-8", crate::web::PAGE),
            "/app.css" => return Res::text(200, "text/css; charset=utf-8", crate::web::STYLE),
            "/app.js" => {
                return Res::text(200, "text/javascript; charset=utf-8", crate::web::SCRIPT);
            }
            _ => {}
        }
    }
```

- [ ] **Step 4: Write the page**

`index.html`: a search input, a two-pane layout (session list, conversation preview), a status line for warnings, and a dialog for the paste-ready command. Load `/app.css` and `/app.js`. No frameworks, no CDN — the binary must be self-contained and the page must work offline.

`app.js` must:
- Read the token from `location.search`, keep it in a variable, then `history.replaceState({}, "", "/")` so it leaves the address bar.
- Send `X-CCPick-Token` on every fetch.
- Load `/api/sessions` on start; render the list with title, path, source, relative time and a running badge.
- Debounce the search box by 150ms (matching the TUI's `DEBOUNCE`), call `/api/search`, reorder by `matches` and show `hits` snippets below a divider.
- Fetch `/api/messages?index=` for the selected row and render the preview.
- Subscribe to `/api/events` with `EventSource` and refetch `/api/sessions` when the generation changes.
- On activating a row: POST `/api/focus` when it is running, otherwise POST `/api/launch`. On a 409 carrying a `command`, show the dialog with a copy button.
- Support keyboard: `/` focuses the search box, arrows move the selection, Enter activates.

`app.css` must use `prefers-color-scheme` for light and dark, with no toggle. Keep the visual language close to the TUI: monospace for paths and times, a green dot for running sessions, a solid selection band.

- [ ] **Step 5: Verify by hand**

```bash
cargo run -- web --port 8731
```

Check in the browser: the list renders, search filters and shows conversation hits, the preview loads, the token is gone from the address bar, and a second tab updates when a session starts or stops elsewhere. Stop the server when finished.

Run: `mise check`.

- [ ] **Step 6: Commit**

```bash
git add src/web/
git commit -m "web: the portal page"
```

---

### Task 10: Documentation and CI

**Files:**
- Modify: `README.md`, `AGENTS.md`, `.github/workflows/ci.yml`

- [ ] **Step 1: Add a CI job proving the feature is optional**

In `.github/workflows/ci.yml`, add to the existing job matrix or as a separate job:

```yaml
  no-default-features:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: jdx/mise-action@v2
      - run: cargo build --no-default-features
      - run: cargo test --no-default-features
```

- [ ] **Step 2: Document it in the README**

Add a `## Web portal` section after `## Use`:

- `ccpick web` serves the session list at `http://127.0.0.1:<port>` and opens a browser.
- It refreshes every 10 seconds (`--refresh`), listens on localhost only, and requires a token that is minted per run and delivered in the opened URL.
- Running sessions can be focused; stopped ones open in a new terminal, or hand you the command to paste when ccpick has no terminal to open.
- `--no-open` prints the URL instead; `--port` pins the port.
- `cargo build --no-default-features` builds the TUI without any HTTP dependency.
- Note the one CLI wrinkle: `ccpick web` is the subcommand, so searching for the literal word "web" needs `ccpick --list web`.

- [ ] **Step 3: Document it in AGENTS.md**

Add to the architecture rules:

- The web portal lives behind the default-on `web` feature. `src/web/server.rs` is the only file that may mention the HTTP crate; everything else goes through `route()`, which is pure over a `&Portal` and therefore testable without a socket.
- Never leave a portal running after a task. Smoke-test with `--no-open` and `curl`, then stop it.

- [ ] **Step 4: Verify**

Run: `mise check`, `cargo build --no-default-features`.

- [ ] **Step 5: Commit**

```bash
git add README.md AGENTS.md .github/workflows/ci.yml
git commit -m "docs: the web portal, and a CI job proving the feature is optional"
```
