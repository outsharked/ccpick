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

pub fn route(req: &Req, portal: &Portal) -> Res {
    // The page itself carries no token: the token arrives in its URL and the script it loads
    // sends it on every call after that.
    if req.path == "/" && req.method == "GET" {
        return Res::text(200, "text/html; charset=utf-8", crate::web::PAGE);
    }
    // A page on another origin must not be able to reach a server that can spawn processes,
    // even if it somehow learned the token. Checked before the token so a foreign origin is
    // refused outright rather than treated as merely unauthenticated.
    if let Some(origin) = req.origin
        && !origin.is_empty()
    {
        return Res::error(403, "cross-origin requests are refused");
    }
    if req.token != Some(portal.token()) {
        return Res::error(401, "missing or invalid token");
    }
    // Bound once here and worked from as an `Arc`: `Portal::catalog()` releases its lock guard
    // immediately, so nothing below ever holds one across handler work.
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
            body: b"",
        }
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
        let req = Req {
            method: "GET",
            path: "/",
            query: "",
            token: None,
            origin: None,
            body: b"",
        };
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
        assert_eq!(
            route(&get("/api/messages", "index=999"), &portal()).status,
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
