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
    /// The request's `Host` header, if any. Checked against a loopback allowlist: a standard
    /// defence against DNS rebinding, where an attacker's page (loaded from a public hostname
    /// that later resolves to 127.0.0.1) is same-origin as far as the browser is concerned and
    /// so sends no `Origin` header at all — the origin check alone does nothing against that.
    pub host: Option<&'a str>,
    pub body: &'a [u8],
}

pub struct Res {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

impl Res {
    pub fn json(status: u16, value: Value) -> Res {
        Res {
            status,
            content_type: "application/json; charset=utf-8".into(),
            body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
            headers: Vec::new(),
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
            headers: Vec::new(),
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

/// Decodes `%XX` escapes. Byte offsets `i + 1..i + 3` are looked up with `str::get`, which
/// returns `None` instead of panicking when the input isn't valid hex or the escape doesn't
/// land on a char boundary (a malformed or adversarial query must never crash the handler
/// thread) — the raw bytes are copied through unchanged in that case.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The hostname part of a `Host` header, with any `:<port>` suffix stripped. Kept deliberately
/// simple (this server never binds anything but IPv4 loopback, so no IPv6 literal form to
/// parse): a trailing `:<digits>` is a port, anything else is left as-is.
fn hostname(host: &str) -> &str {
    match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    }
}

/// Whether `actual` is the page's own origin, allowing either loopback hostname on whatever
/// port `expected` (as recorded by `Portal::set_origin`) was bound to.
fn origin_matches(expected: &str, actual: &str) -> bool {
    match expected.rsplit_once(':') {
        Some((_, port)) => {
            actual.eq_ignore_ascii_case(&format!("http://127.0.0.1:{port}"))
                || actual.eq_ignore_ascii_case(&format!("http://localhost:{port}"))
        }
        None => false,
    }
}

pub fn route(req: &Req, portal: &Portal) -> Res {
    // Refused before anything else, regardless of path: a DNS-rebinding attacker's page can
    // reach this server carrying a `Host` header for its own public hostname, and the browser
    // treats that as same-origin (no `Origin` header at all), so this is the only check that
    // catches it.
    if let Some(host) = req.host {
        let name = hostname(host);
        if !name.eq_ignore_ascii_case("127.0.0.1") && !name.eq_ignore_ascii_case("localhost") {
            return Res::error(403, "unrecognized Host header");
        }
    }
    // The page itself carries no token: the token arrives in its URL and the script it loads
    // sends it on every call after that.
    if req.path == "/" && req.method == "GET" {
        let mut res = Res::text(200, "text/html; charset=utf-8", crate::web::PAGE);
        // The token lives in this page's own URL. Without this, any external subresource or
        // outbound link a later task adds would leak it via the Referer header.
        res.headers
            .push(("Referrer-Policy".into(), "no-referrer".into()));
        return res;
    }
    // A page on another origin must not be able to reach a server that can spawn processes,
    // even if it somehow learned the token. Checked before the token so a foreign origin is
    // refused outright rather than treated as merely unauthenticated. Compared against the
    // page's own recorded origin rather than blanket-rejected, since a browser sends `Origin`
    // on the page's own same-origin POSTs too, not only on genuinely cross-origin requests; if
    // no origin has been recorded yet, fail closed and refuse any non-empty `Origin`.
    if let Some(origin) = req.origin
        && !origin.is_empty()
        && !portal
            .origin()
            .is_some_and(|expected| origin_matches(expected, origin))
    {
        return Res::error(403, "cross-origin requests are refused");
    }
    if req.token != Some(portal.token()) {
        return Res::error(401, "missing or invalid token");
    }
    // Bound once here and worked from as an `Arc`, together with the generation it was
    // published under: reading them as one pair (rather than two separate lock acquisitions)
    // means a publish landing in between can never pair a catalog with the wrong generation.
    let (catalog, generation) = portal.published();
    match (req.method, req.path) {
        ("GET", "/api/sessions") => Res::json(
            200,
            sessions_payload(&catalog, generation, crate::format::now_ms()),
        ),
        ("GET", "/api/search") => {
            let query = query_param(req.query, "q").unwrap_or_default();
            let all: Vec<usize> = (0..catalog.sessions.len()).collect();
            // Ranked by position (fuzzy/full_text work in terms of indices into `catalog.sessions`
            // for the scan itself), but handed to the client as ids: the same
            // `Reverse(last_ts)`-sorted, periodically republished list that `/api/focus`,
            // `/api/launch` and `/api/messages` stopped trusting positions from. A result
            // rendered from a stale position could name a different session by the time a click
            // on it reaches those endpoints.
            let matches: Vec<&str> = crate::search::fuzzy(&catalog, &all, &query)
                .into_iter()
                .map(|idx| catalog.sessions[idx].meta.id.as_str())
                .collect();
            // A newer search supersedes an older one still scanning: full_text checks this
            // ticket against the shared counter as it goes, so a search box wired per keystroke
            // can't pile up uncancellable scans across rayon's pool.
            let mine = portal.next_search();
            let hits: Vec<Value> = crate::search::full_text(
                &catalog,
                &all,
                &query,
                Some((portal.search_generation(), mine)),
            )
            .unwrap_or_default()
            .into_iter()
            .map(|hit| json!({ "id": catalog.sessions[hit.session].meta.id, "snippet": hit.snippet }))
            .collect();
            Res::json(200, json!({ "matches": matches, "hits": hits }))
        }
        ("GET", "/api/messages") => match query_param(req.query, "id") {
            None => Res::error(400, "id is required"),
            Some(id) => match catalog.sessions.iter().position(|s| s.meta.id == id) {
                Some(idx) => Res::json(200, messages_payload(&catalog, idx)),
                None => Res::error(404, "no such session"),
            },
        },
        ("POST", "/api/focus") => {
            let Some(id) = body_session_id(req.body) else {
                return Res::error(400, "id is required");
            };
            let Some(session) = catalog.sessions.iter().find(|s| s.meta.id == id) else {
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
            let Some(id) = body_session_id(req.body) else {
                return Res::error(400, "id is required");
            };
            // Addressed by id, not by list position: `catalog.sessions` is sorted by
            // `Reverse(last_ts)` and the refresh loop republishes every few seconds, so a
            // position a page rendered a moment ago can already name a different session by the
            // time a click arrives here — exactly when a live session producing output would
            // reorder the list. The id is stable across that reorder.
            let Some(idx) = catalog.sessions.iter().position(|s| s.meta.id == id) else {
                return Res::error(404, "no such session");
            };
            let session = &catalog.sessions[idx];
            let source_idx = session.default_source;
            let plan = catalog.launch_plan(idx, source_idx);
            let env = catalog.sources[source_idx].env.clone();
            // A stale tab's launch button can race a session that has since started running
            // elsewhere; resuming it again would run a second agent against the same
            // transcript. The TUI forbids this at the UI layer (`ui/app.rs` routes Enter to
            // focus instead of launch for a running session), but the page can only know what
            // it last fetched, while the server's own catalog is the fresher view — so this
            // guard has to live here too, carrying the pid back so the page can offer focusing
            // instead.
            if let Some((pid, _)) = session.live {
                return Res::json(
                    409,
                    json!({
                        "error": "that session is already running",
                        "pid": pid,
                        "command": crate::shell::resume_command(&plan, &env),
                        "shell": env.shell_name(),
                    }),
                );
            }
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
        _ => Res::error(404, "not found"),
    }
}

pub fn body_session_id(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("id")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake_catalog;

    fn portal() -> Portal {
        Portal::new(fake_catalog(), "secret".into())
    }

    fn get<'a>(path: &'a str, query: &'a str) -> Req<'a> {
        Req {
            method: "GET",
            path,
            query,
            token: Some("secret"),
            origin: None,
            host: None,
            body: b"",
        }
    }

    fn post<'a>(path: &'a str, body: &'a [u8]) -> Req<'a> {
        Req {
            method: "POST",
            path,
            query: "",
            token: Some("secret"),
            origin: None,
            host: None,
            body,
        }
    }

    #[test]
    fn focusing_a_session_that_is_not_running_is_a_conflict_not_an_action() {
        let res = route(&post("/api/focus", br#"{"id":"a"}"#), &portal());
        assert_eq!(res.status, 409);
    }

    #[test]
    fn focusing_an_unknown_id_is_a_404() {
        let res = route(
            &post("/api/focus", br#"{"id":"does-not-exist"}"#),
            &portal(),
        );
        assert_eq!(res.status, 404);
    }

    #[test]
    fn launching_a_session_ccpick_cannot_reach_returns_the_command_to_paste() {
        let portal = Portal::new(crate::catalog::fake_catalog_with_foreign(), "secret".into());
        let res = route(&post("/api/launch", br#"{"id":"w"}"#), &portal);
        assert_eq!(res.status, 409);
        let value: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
        assert!(value["command"].as_str().unwrap().contains("--resume"));
    }

    #[test]
    fn launching_a_session_the_catalog_shows_as_live_is_refused_with_the_pid_to_focus_instead() {
        // "c" ("Running thing") is live under source "two", pid 4242 -- see
        // `catalog::fake_catalog`. This guard must fire before the launch would otherwise
        // proceed, and must never reach `spawn_in_new_terminal`.
        let res = route(&post("/api/launch", br#"{"id":"c"}"#), &portal());
        assert_eq!(res.status, 409);
        let value: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
        assert_eq!(value["pid"], 4242);
        assert!(value["command"].as_str().unwrap().contains("--resume"));
        assert!(value["shell"].is_string());
    }

    #[test]
    fn a_malformed_body_is_a_400() {
        assert_eq!(
            route(&post("/api/launch", b"not json"), &portal()).status,
            400
        );
        assert_eq!(
            route(
                &post("/api/launch", br#"{"id":"does-not-exist"}"#),
                &portal()
            )
            .status,
            404
        );
    }

    #[test]
    fn parses_the_session_id_out_of_a_body() {
        assert_eq!(body_session_id(br#"{"id":"abc"}"#), Some("abc".to_string()));
        assert_eq!(body_session_id(br#"{"other":1}"#), None);
        assert_eq!(body_session_id(b""), None);
    }

    #[test]
    fn a_request_without_the_token_is_refused() {
        let mut req = get("/api/sessions", "");
        req.token = None;
        let res = route(&req, &portal());
        assert_eq!(res.status, 401);
        // Proves a refusal, not merely a status code stamped on a payload that was built
        // anyway: no catalog data rides along with the error.
        let value: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
        assert!(value["error"].is_string());
        assert!(value["sessions"].is_null());

        req.token = Some("wrong");
        assert_eq!(route(&req, &portal()).status, 401);
    }

    #[test]
    fn a_request_from_another_origin_is_refused_even_with_the_token() {
        let mut req = get("/api/sessions", "");
        req.origin = Some("https://evil.example");
        let res = route(&req, &portal());
        assert_eq!(res.status, 403);
        let value: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
        assert!(value["error"].is_string());
        assert!(value["sessions"].is_null());
    }

    #[test]
    fn a_request_matching_the_recorded_origin_is_accepted() {
        let portal = portal();
        portal.set_origin("http://127.0.0.1:4242".into());
        let mut req = get("/api/sessions", "");
        req.origin = Some("http://127.0.0.1:4242");
        assert_eq!(route(&req, &portal).status, 200);
        // The other loopback hostname, same port, is also the page's own origin.
        req.origin = Some("http://localhost:4242");
        assert_eq!(route(&req, &portal).status, 200);
    }

    #[test]
    fn the_origin_comparison_is_case_insensitive() {
        let portal = portal();
        portal.set_origin("http://127.0.0.1:4242".into());
        let mut req = get("/api/sessions", "");
        req.origin = Some("HTTP://127.0.0.1:4242");
        assert_eq!(route(&req, &portal).status, 200);
    }

    #[test]
    fn a_foreign_origin_is_refused_even_once_one_is_recorded() {
        let portal = portal();
        portal.set_origin("http://127.0.0.1:4242".into());
        let mut req = get("/api/sessions", "");
        req.origin = Some("https://evil.example");
        assert_eq!(route(&req, &portal).status, 403);
    }

    #[test]
    fn an_origin_is_refused_when_none_has_been_recorded_yet() {
        let mut req = get("/api/sessions", "");
        req.origin = Some("http://127.0.0.1:4242");
        assert_eq!(route(&req, &portal()).status, 403);
    }

    #[test]
    fn a_foreign_host_header_is_refused() {
        let mut req = get("/api/sessions", "");
        req.host = Some("evil.example");
        assert_eq!(route(&req, &portal()).status, 403);
    }

    #[test]
    fn a_loopback_host_header_with_a_port_is_accepted() {
        let mut req = get("/api/sessions", "");
        req.host = Some("127.0.0.1:4242");
        assert_eq!(route(&req, &portal()).status, 200);
        req.host = Some("localhost:4242");
        assert_eq!(route(&req, &portal()).status, 200);
    }

    #[test]
    fn the_host_comparison_is_case_insensitive() {
        let mut req = get("/api/sessions", "");
        req.host = Some("LOCALHOST:4242");
        assert_eq!(route(&req, &portal()).status, 200);
    }

    #[test]
    fn a_post_to_a_get_only_path_is_not_found() {
        let mut req = get("/api/sessions", "");
        req.method = "POST";
        assert_eq!(route(&req, &portal()).status, 404);
    }

    #[test]
    fn the_page_is_served_without_a_token_because_the_token_arrives_in_its_url() {
        let req = Req {
            method: "GET",
            path: "/",
            query: "",
            token: None,
            origin: None,
            host: None,
            body: b"",
        };
        let res = route(&req, &portal());
        assert_eq!(res.status, 200);
        assert!(res.content_type.starts_with("text/html"));
    }

    #[test]
    fn the_page_response_tells_the_browser_never_to_leak_the_token_via_referer() {
        let req = Req {
            method: "GET",
            path: "/",
            query: "",
            token: None,
            origin: None,
            host: None,
            body: b"",
        };
        let res = route(&req, &portal());
        assert!(
            res.headers
                .iter()
                .any(|(k, v)| k == "Referrer-Policy" && v == "no-referrer")
        );
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
        // Matches are session ids, not list positions: `catalog.sessions` is sorted by
        // `Reverse(last_ts)` and periodically republished, so a position handed back here would
        // go stale exactly like the one `/api/focus`/`/api/launch` stopped trusting.
        let res = route(&get("/api/search", "q=docker"), &portal());
        let value: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
        let matches = value["matches"].as_array().unwrap();
        assert!(!matches.is_empty());
        let catalog = fake_catalog();
        let first = matches[0].as_str().unwrap();
        let session = catalog
            .sessions
            .iter()
            .find(|s| s.meta.id == first)
            .unwrap();
        assert_eq!(session.meta.title, "Docker build cache");
    }

    #[test]
    fn search_also_returns_hits_from_inside_conversations() {
        let res = route(&get("/api/search", "q=PINEAPPLE"), &portal());
        let value: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
        let hits = value["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0]["snippet"].as_str().unwrap().contains("PINEAPPLE"));
        let catalog = fake_catalog();
        let id = hits[0]["id"].as_str().unwrap();
        let session = catalog.sessions.iter().find(|s| s.meta.id == id).unwrap();
        assert_eq!(session.meta.title, "Kubernetes ingress");
    }

    #[test]
    fn the_search_arm_mints_exactly_one_ticket_and_wires_it_into_full_text() {
        // The cancellation semantics themselves -- that a stale ticket makes `full_text` return
        // no hits -- are already covered deterministically by
        // `search::tests::cancelled_search_returns_none`. This test only proves the wiring: that
        // `/api/search` actually mints a ticket via `next_search()` and passes it through, not
        // that a superseded scan behaves correctly (that would need real concurrency to observe
        // here, which is both unnecessary -- the other test already proves it -- and a source of
        // CI flakiness on a starved machine).
        let portal = portal();
        let before = portal
            .search_generation()
            .load(std::sync::atomic::Ordering::SeqCst);
        let res = route(&get("/api/search", "q=docker"), &portal);
        let after = portal
            .search_generation()
            .load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            after,
            before + 1,
            "the /api/search arm must mint exactly one ticket per call"
        );
        let value: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
        assert!(!value["matches"].as_array().unwrap().is_empty());
    }

    #[test]
    fn messages_are_served_for_a_session_id() {
        // "a" is "Docker build cache", whose fixture messages are a known user/assistant pair —
        // pin their content, not just that `messages` happens to be an array, which
        // `{"messages":[]}` would also satisfy.
        let res = route(&get("/api/messages", "id=a"), &portal());
        assert_eq!(res.status, 200);
        let value: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
        let messages = value["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["text"], "fix docker");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["text"], "done");
    }

    #[test]
    fn an_unknown_id_is_a_404_not_a_panic() {
        assert_eq!(
            route(&get("/api/messages", "id=does-not-exist"), &portal()).status,
            404
        );
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

    #[test]
    fn a_percent_escape_that_lands_mid_character_does_not_panic() {
        // "%" followed by a 3-byte UTF-8 character (€): the naive byte-offset slice
        // `i + 1..i + 3` would land inside that character, not on a char boundary.
        let query = "q=%\u{20ac}";
        let decoded = query_param(query, "q").unwrap();
        assert!(decoded.contains('\u{20ac}'));
    }
}
