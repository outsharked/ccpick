//! The only module that knows about the HTTP crate. Everything else works through `route()`.
use crate::web::route::{Req, route};
use crate::web::state::Portal;
use std::io::Write;
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

fn header<'a>(request: &'a tiny_http::Request, name: &'static str) -> Option<&'a str> {
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
    let host = header(&request, "Host").map(str::to_string);

    // SSE never completes, so it can't be served through `route()`'s request/response model:
    // it is handled here, before `route`, with its own token check.
    if path == "/api/events" && token.as_deref() == Some(portal.token()) {
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
            token: token.as_deref(),
            origin: origin.as_deref(),
            host: host.as_deref(),
            body: &body,
        },
        portal,
    );
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

/// Binds, serves until `stop` is set, and returns the port that was actually bound (relevant
/// when `port` was 0). Sets the portal's origin to what was just bound, so `route()` can compare
/// the browser's `Origin` against it.
pub fn serve(portal: Arc<Portal>, port: u16, stop: Arc<AtomicBool>) -> anyhow::Result<u16> {
    let server = tiny_http::Server::http(("127.0.0.1", port))
        .map_err(|e| anyhow::anyhow!("could not bind 127.0.0.1:{port}: {e}"))?;
    let bound = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .unwrap_or(port);
    portal.set_origin(format!("http://127.0.0.1:{bound}"));
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
    }
}
