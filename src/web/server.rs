//! The only module that knows about the HTTP crate. Everything else works through `route()`.
use crate::web::route::{
    Req, Res, host_is_allowed, origin_matches, query_param, route, tokens_match,
};
use crate::web::state::Portal;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// No real endpoint needs more than a few hundred bytes (`{"id":"..."}`); 64 KiB is generous
/// headroom. Enforced by capping the read itself (`Read::take`), never by trusting the
/// client-declared `Content-Length` — tiny_http places no upper bound of its own on that value,
/// so without this a `Content-Length: 2000000000` request would grow `body` to match, and it
/// would do so before the token, Origin or Host check ever ran (any page the user has open can
/// reach this far: a cross-origin `fetch` with a `text/plain` body is CORS-simple, so no
/// preflight stops it from hitting the handler even though the request will fail auth).
const MAX_BODY_BYTES: u64 = 64 * 1024;

pub fn sse_event(generation: u64) -> String {
    format!("event: generation\ndata: {generation}\n\n")
}

/// 128 bits of randomness, hex encoded.
pub fn mint_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the OS must provide randomness for the portal's token");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn header<'a>(request: &'a tiny_http::Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str())
}

fn write_response(request: tiny_http::Request, res: Res) {
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], res.content_type.as_bytes())
        .expect("content types are valid header values");
    let mut response = tiny_http::Response::from_data(res.body)
        .with_status_code(res.status)
        .with_header(header);
    for (name, value) in &res.headers {
        if let Ok(header) = tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            response.add_header(header);
        }
    }
    let _ = request.respond(response);
}

/// Serves one request. SSE is handled separately because it never completes.
fn handle(mut request: tiny_http::Request, portal: &Portal) {
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let method = request.method().as_str().to_string();
    let host = header(&request, "Host").map(str::to_string);

    // Read at most `MAX_BODY_BYTES + 1`: enough to tell an oversized body apart from a normal
    // one without ever buffering past the cap, regardless of what `Content-Length` claimed.
    let mut body = Vec::new();
    let _ = request
        .as_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut body);
    if body.len() as u64 > MAX_BODY_BYTES {
        write_response(request, Res::error(413, "request body too large"));
        return;
    }

    // The header token is what every endpoint accepts. A query-string token needs no custom
    // header to attach, and a GET carrying one sends no `Origin` either, so together they would
    // let a cross-origin page's preflight-free request through unnoticed — exactly what
    // `X-CCPick-Token` (a non-simple header, so it forces a preflight) exists to prevent. Only
    // `/api/events` gets the query fallback, because `EventSource` cannot set custom headers.
    let header_token = header(&request, "X-CCPick-Token").map(str::to_string);
    let origin = header(&request, "Origin").map(str::to_string);

    // SSE never completes, so it can't be served through `route()`'s request/response model: it
    // is handled here, before `route`, with its own Host, Origin and token checks — the same
    // checks `route()` applies to everything else, via the shared `host_is_allowed`,
    // `origin_matches` and `tokens_match`, so there is one rule rather than two (or three).
    if path == "/api/events" {
        if !host_is_allowed(host.as_deref()) {
            write_response(request, Res::error(403, "unrecognized Host header"));
            return;
        }
        if let Some(o) = origin.as_deref()
            && !o.is_empty()
            && !portal
                .origin()
                .is_some_and(|expected| origin_matches(expected, o))
        {
            write_response(
                request,
                Res::error(403, "cross-origin requests are refused"),
            );
            return;
        }
        let event_token = header_token.clone().or_else(|| query_param(query, "t"));
        if !tokens_match(event_token.as_deref(), portal.token()) {
            write_response(request, Res::error(401, "missing or invalid token"));
            return;
        }
        let events = portal.subscribe();
        let mut writer = request.into_writer();
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                    Cache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
        // tiny_http buffers this writer (a 1KB `BufWriter` under the hood), so without an
        // explicit flush here the head sits unsent until the buffer fills or the first event
        // flushes it — leaving the browser's EventSource waiting for up to a whole refresh
        // interval before it even sees a 200.
        if writer.write_all(head.as_bytes()).is_err() || writer.flush().is_err() {
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

    let res = route(
        &Req {
            method: &method,
            path,
            query,
            token: header_token.as_deref(),
            origin: origin.as_deref(),
            host: host.as_deref(),
            body: &body,
        },
        portal,
    );
    write_response(request, res);
}

/// The threads backing one `serve()` call: the accept loop, and the small watcher that turns
/// `stop` into a call to `tiny_http::Server::unblock()`.
struct Handles {
    accept: std::thread::JoinHandle<()>,
    watcher: std::thread::JoinHandle<()>,
}

fn bind(portal: Arc<Portal>, port: u16, stop: Arc<AtomicBool>) -> anyhow::Result<(u16, Handles)> {
    let server = Arc::new(
        tiny_http::Server::http(("127.0.0.1", port))
            .map_err(|e| anyhow::anyhow!("could not bind 127.0.0.1:{port}: {e}"))?,
    );
    let bound = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .unwrap_or(port);
    portal.set_origin(format!("http://127.0.0.1:{bound}"));

    let accept_server = server.clone();
    let accept = std::thread::spawn(move || {
        for request in accept_server.incoming_requests() {
            let portal = portal.clone();
            std::thread::spawn(move || handle(request, &portal));
        }
    });

    // `incoming_requests()` blocks in `accept()`, and nothing wakes it just because `stop` flips
    // — so a thread has to poll the flag and call `unblock()` itself once it does. That is a
    // graceful shutdown by tiny_http's own design (its docs recommend exactly this pairing), and
    // it lets `serve_for_test`'s stop closure join both threads and know the listening socket is
    // genuinely gone, rather than leaving a live server for the rest of the test binary's run.
    let watcher = std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
        }
        server.unblock();
    });

    Ok((bound, Handles { accept, watcher }))
}

/// Binds and serves in the background, returning the port that was actually bound (relevant
/// when `port` was 0). Sets the portal's origin to what was just bound, so `route()` can compare
/// the browser's `Origin` against it. Keeps serving until `stop` is set.
pub fn serve(portal: Arc<Portal>, port: u16, stop: Arc<AtomicBool>) -> anyhow::Result<u16> {
    let (bound, _handles) = bind(portal, port, stop)?;
    Ok(bound)
}

/// Binds on an ephemeral port for tests and hands back the port and a stopper. The stopper
/// blocks until the server has actually shut down (both its threads have exited and the
/// listening socket is closed), so a test can call it and then assert the port refuses new
/// connections, rather than merely asking for a shutdown and hoping.
#[doc(hidden)]
pub fn serve_for_test(portal: Arc<Portal>) -> anyhow::Result<(u16, impl FnOnce())> {
    let stop = Arc::new(AtomicBool::new(false));
    let (port, handles) = bind(portal, 0, stop.clone())?;
    Ok((port, move || {
        stop.store(true, Ordering::Relaxed);
        let _ = handles.watcher.join();
        let _ = handles.accept.join();
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake_catalog;
    use crate::web::state::Portal;
    use std::io::{BufRead, BufReader};
    use std::net::TcpStream;

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

    /// Binds a real socket on an ephemeral port and checks the server answers. Lives here
    /// (rather than in `tests/`) because `fake_catalog()` is `#[cfg(test)]`-only and a separate
    /// integration-test crate can't reach it.
    ///
    /// `line` must end with `Connection: close\r\n\r\n`: HTTP/1.1 defaults to keep-alive, and
    /// without that header the server leaves the socket open after responding, so reading to
    /// EOF below would hang forever instead of seeing the response end.
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

    /// A `stop()` that doesn't actually make the port stop accepting connections would leave a
    /// live server behind for the rest of the test binary's run.
    ///
    /// Joining `stop()`'s two threads only proves *our* accept-consumer and watcher threads have
    /// exited; the listening socket itself is closed by tiny_http's own internal accept thread,
    /// woken asynchronously (by `Server::drop`'s self-connect) once the last `Arc<Server>` drops
    /// on one of ours. That handoff is genuine cross-thread scheduling, not something a caller
    /// outside the crate can wait on directly — so this polls with a short bound instead of
    /// asserting instantly.
    fn assert_port_closed(port: u16) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_err() {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("expected port {port} to no longer accept connections after stop()");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_server_requires_the_token() {
        let portal = Arc::new(Portal::new(fake_catalog(), "secret".into()));
        let (port, stop) = serve_for_test(portal).unwrap();

        let (code, _) = request(
            port,
            "GET /api/sessions HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(code, 401);

        let (code, body) = request(
            port,
            "GET /api/sessions HTTP/1.1\r\nHost: localhost\r\nX-CCPick-Token: secret\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(code, 200);
        assert!(body.contains("sessions"));

        stop();
        assert_port_closed(port);
    }

    #[test]
    fn a_request_with_the_wrong_host_header_is_refused_even_with_the_token() {
        let portal = Arc::new(Portal::new(fake_catalog(), "secret".into()));
        let (port, stop) = serve_for_test(portal).unwrap();

        let (code, _) = request(
            port,
            "GET /api/sessions HTTP/1.1\r\nHost: evil.example\r\nX-CCPick-Token: secret\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(code, 403);

        stop();
        assert_port_closed(port);
    }

    #[test]
    fn a_query_string_token_is_refused_on_an_ordinary_endpoint() {
        let portal = Arc::new(Portal::new(fake_catalog(), "secret".into()));
        let (port, stop) = serve_for_test(portal).unwrap();

        // No `X-CCPick-Token` header: only `/api/events` may fall back to `?t=`.
        let (code, _) = request(
            port,
            "GET /api/sessions?t=secret HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(code, 401);

        stop();
        assert_port_closed(port);
    }

    #[test]
    fn the_events_endpoint_still_accepts_a_query_string_token() {
        // `EventSource` cannot set custom headers, so this is the one endpoint that must accept
        // the token from `?t=`.
        let portal = Arc::new(Portal::new(fake_catalog(), "secret".into()));
        let (port, stop) = serve_for_test(portal).unwrap();

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(b"GET /api/events?t=secret HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert!(status.contains("200"));
        drop(reader);

        stop();
        assert_port_closed(port);
    }

    #[test]
    fn the_events_endpoint_rejects_a_foreign_origin_even_with_the_token() {
        // Before this fix, `/api/events` checked Host and the token but not Origin -- the one
        // check `route()` applies to every other endpoint. `bind()` records the page's own
        // origin as soon as it binds, so a request claiming a different one must still be
        // refused here.
        let portal = Arc::new(Portal::new(fake_catalog(), "secret".into()));
        let (port, stop) = serve_for_test(portal).unwrap();

        let (code, _) = request(
            port,
            "GET /api/events?t=secret HTTP/1.1\r\nHost: localhost\r\nOrigin: https://evil.example\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(code, 403);

        stop();
        assert_port_closed(port);
    }

    #[test]
    fn an_oversized_body_is_refused_before_it_reaches_a_handler() {
        let portal = Arc::new(Portal::new(fake_catalog(), "secret".into()));
        let (port, stop) = serve_for_test(portal).unwrap();

        let body = "a".repeat(MAX_BODY_BYTES as usize + 1);
        let request_line = format!(
            "POST /api/launch HTTP/1.1\r\nHost: localhost\r\nX-CCPick-Token: secret\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let (code, _) = request(port, &request_line);
        assert_eq!(code, 413);

        stop();
        assert_port_closed(port);
    }

    #[test]
    fn a_normal_sized_body_is_still_read_in_full() {
        let portal = Arc::new(Portal::new(fake_catalog(), "secret".into()));
        let (port, stop) = serve_for_test(portal).unwrap();

        let body = r#"{"id":"does-not-exist","agent":"fake"}"#;
        let request_line = format!(
            "POST /api/launch HTTP/1.1\r\nHost: localhost\r\nX-CCPick-Token: secret\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        // 404 (rather than 400 "id is required") proves the whole body made it through intact.
        let (code, _) = request(port, &request_line);
        assert_eq!(code, 404);

        stop();
        assert_port_closed(port);
    }

    #[test]
    fn the_sse_stream_ends_when_the_client_disconnects() {
        let portal = Arc::new(Portal::new(fake_catalog(), "secret".into()));
        let (port, stop) = serve_for_test(portal.clone()).unwrap();

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(b"GET /api/events?t=secret HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert!(status.contains("200"));

        // Publishing after the client has gone away must not hang the handler thread forever:
        // the next write attempt fails and the thread returns. Proven indirectly here by the
        // publish and a subsequent fresh request both completing promptly.
        drop(stream);
        assert!(portal.publish({
            let mut catalog = fake_catalog();
            catalog.sessions[0].meta.title = "Renamed".into();
            catalog
        }));

        let (code, body) = request(
            port,
            "GET /api/sessions HTTP/1.1\r\nHost: localhost\r\nX-CCPick-Token: secret\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(code, 200);
        assert!(body.contains("Renamed"));

        stop();
        assert_port_closed(port);
    }
}
