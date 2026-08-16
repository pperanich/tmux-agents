//! A minimal, dependency-free HTTP/1.1 POST over `std::net::TcpStream`. The action broker's
//! API lane answers an OpenCode permission prompt with one small JSON POST to a localhost server, so
//! the whole client is: resolve `host:port` from the base URL, connect with a deadline, write one
//! request, read the status line. No TLS (localhost), no retry, no keep-alive, no body parsing beyond
//! the status code — the repo's dependency discipline (AD: a small static binary) rules out a
//! heavyweight HTTP crate for this.
//!
//! `timeout` bounds connect and the whole round trip: it is split across DNS/connect and the
//! read/write via a wall-clock deadline, so a hung server cannot wedge the broker (which holds the
//! pane lock while this runs).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// The outcome of one POST, in the broker's terms: a 2xx is the answer delivered, a 404 is
/// the target gone (the prompt was answered/withdrawn between gate and act), anything else — an
/// unreachable server, a non-2xx/404 status, a malformed base URL — is a broker error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpOutcome {
    /// 2xx.
    Ok,
    /// 404.
    NotFound,
    /// Transport failure or an unexpected status; carries a short reason for stderr.
    Error(String),
}

/// POST `body` (sent as `application/json`) to `base` + `path`, bounded by `timeout` (connect +
/// total). `base` is a plain `http://host:port[/prefix]` URL (no TLS); `path` starts with `/`.
pub(crate) fn post_json(base: &str, path: &str, body: &str, timeout: Duration) -> HttpOutcome {
    let deadline = Instant::now() + timeout;
    let target = match Target::parse(base, path) {
        Ok(t) => t,
        Err(e) => return HttpOutcome::Error(e),
    };

    let mut stream = match connect(&target, deadline) {
        Ok(s) => s,
        Err(e) => return HttpOutcome::Error(e),
    };

    let request = format!(
        "POST {p} HTTP/1.1\r\nHost: {h}\r\nContent-Type: application/json\r\n\
         Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        p = target.request_target,
        h = target.host_header,
        len = body.len(),
    );
    // Set the write/read timeout to the time left so a stalled socket cannot outlive the deadline.
    if let Err(e) = apply_remaining(&stream, deadline) {
        return HttpOutcome::Error(e);
    }
    if let Err(e) = stream.write_all(request.as_bytes()) {
        return HttpOutcome::Error(format!("write failed: {e}"));
    }
    if let Err(e) = apply_remaining(&stream, deadline) {
        return HttpOutcome::Error(e);
    }
    // The status line is all we need; read enough to cover it and stop (Connection: close means the
    // server closes when done, but we never need the body).
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 256];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'\n') || buf.len() >= 4096 {
                    break;
                }
            }
            Err(e) => return HttpOutcome::Error(format!("read failed: {e}")),
        }
    }
    classify_status(&buf)
}

/// Parse the status code from the first line (`HTTP/1.1 <code> <reason>`) into an outcome.
fn classify_status(response: &[u8]) -> HttpOutcome {
    let text = String::from_utf8_lossy(response);
    let Some(line) = text.lines().next() else {
        return HttpOutcome::Error("empty response".to_string());
    };
    let code = line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok());
    match code {
        Some(c) if (200..300).contains(&c) => HttpOutcome::Ok,
        Some(404) => HttpOutcome::NotFound,
        Some(c) => HttpOutcome::Error(format!("server returned HTTP {c}")),
        None => HttpOutcome::Error(format!("unparseable status line: {line:?}")),
    }
}

/// A resolved POST target: the socket address string, the `Host:` header value, and the request
/// target (path, including any base-URL prefix).
struct Target {
    authority: String,
    host_header: String,
    request_target: String,
}

impl Target {
    /// Parse `http://host:port[/prefix]` + `path` into a target. Rejects a non-`http` scheme (no TLS
    /// on the localhost lane), a missing host, and any whitespace/control byte in either input (the
    /// values land verbatim in the request start line, so this is the CRLF-injection backstop behind
    /// the broker's read-time validation). A missing port defaults to 80.
    fn parse(base: &str, path: &str) -> Result<Target, String> {
        if !base.bytes().all(|b| b.is_ascii_graphic())
            || !path.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err("endpoint or path carries whitespace/control bytes".to_string());
        }
        let rest = base.strip_prefix("http://").ok_or_else(|| {
            format!("api_base {base:?} must be an http:// URL (no TLS on localhost)")
        })?;
        let (authority, prefix) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].trim_end_matches('/')),
            None => (rest, ""),
        };
        if authority.is_empty() {
            return Err(format!("api_base {base:?} has no host"));
        }
        let host = authority.split(':').next().unwrap_or(authority);
        let authority = if authority.contains(':') {
            authority.to_string()
        } else {
            format!("{authority}:80")
        };
        Ok(Target {
            authority,
            host_header: host.to_string(),
            request_target: format!("{prefix}{path}"),
        })
    }
}

/// Connect to the target with the remaining time before `deadline`.
fn connect(target: &Target, deadline: Instant) -> Result<TcpStream, String> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| "timed out before connect".to_string())?;
    let addr = target
        .authority
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {}: {e}", target.authority))?
        .next()
        .ok_or_else(|| format!("no address for {}", target.authority))?;
    TcpStream::connect_timeout(&addr, remaining).map_err(|e| format!("connect failed: {e}"))
}

/// Set the stream's read+write timeout to the time left before `deadline`, so no single syscall can
/// outlive the total bound.
fn apply_remaining(stream: &TcpStream, deadline: Instant) -> Result<(), String> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| "request timed out".to_string())?;
    stream
        .set_write_timeout(Some(remaining))
        .and_then(|()| stream.set_read_timeout(Some(remaining)))
        .map_err(|e| format!("cannot set socket timeout: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// Spawn a one-shot server that reads the request and replies with `status_line` + an empty
    /// body, returning its `http://127.0.0.1:PORT` base. The reader drains the request first so the
    /// client's `write_all` never blocks on an unread socket.
    fn one_shot(status_line: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(
                    format!("{status_line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                );
            }
        });
        base
    }

    #[test]
    fn two_hundred_is_ok() {
        let base = one_shot("HTTP/1.1 200 OK");
        assert_eq!(
            post_json(
                &base,
                "/permission/req-1/reply",
                "{\"reply\":\"once\"}",
                Duration::from_secs(2)
            ),
            HttpOutcome::Ok
        );
    }

    #[test]
    fn four_oh_four_is_not_found() {
        let base = one_shot("HTTP/1.1 404 Not Found");
        assert_eq!(
            post_json(
                &base,
                "/permission/gone/reply",
                "{}",
                Duration::from_secs(2)
            ),
            HttpOutcome::NotFound
        );
    }

    #[test]
    fn five_hundred_is_error() {
        let base = one_shot("HTTP/1.1 500 Internal Server Error");
        assert!(matches!(
            post_json(&base, "/permission/x/reply", "{}", Duration::from_secs(2)),
            HttpOutcome::Error(_)
        ));
    }

    #[test]
    fn unreachable_server_is_error() {
        // Bind then drop, so the port is (almost certainly) refused.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        assert!(matches!(
            post_json(
                &base,
                "/permission/x/reply",
                "{}",
                Duration::from_millis(500)
            ),
            HttpOutcome::Error(_)
        ));
    }

    #[test]
    fn non_http_scheme_is_rejected() {
        assert!(matches!(
            post_json("https://example.com", "/x", "{}", Duration::from_secs(1)),
            HttpOutcome::Error(_)
        ));
    }

    #[test]
    fn crlf_in_base_or_path_is_rejected() {
        // The CRLF-injection backstop: smuggled header/request bytes never reach the wire.
        assert!(Target::parse("http://127.0.0.1:1/\r\nX: y", "/p").is_err());
        assert!(Target::parse("http://127.0.0.1:1", "/permission/a\r\nX: y/reply").is_err());
        assert!(Target::parse("http://127.0.0.1:1", "/permission/a b/reply").is_err());
    }

    #[test]
    fn base_prefix_and_default_port_parse() {
        let t = Target::parse("http://localhost/api", "/permission/1/reply").unwrap();
        assert_eq!(t.authority, "localhost:80");
        assert_eq!(t.host_header, "localhost");
        assert_eq!(t.request_target, "/api/permission/1/reply");

        let t2 = Target::parse("http://127.0.0.1:4096/", "/permission/1/reply").unwrap();
        assert_eq!(t2.authority, "127.0.0.1:4096");
        assert_eq!(t2.request_target, "/permission/1/reply");
    }
}
