//! JSON-RPC 2.0 MCP over HTTP/1.1. Same methods as stdio [`crate::handle_jsonrpc`].
//!
//! Local-first: default bind is loopback. Non-loopback requires a bearer token.

use crate::error::{HostError, Result};
use crate::mcp::{CancelScope, HostService, MCP_PROTOCOL_VERSION, handle_jsonrpc};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

/// Default `--http` bind when the flag has no value.
pub const DEFAULT_HTTP_BIND: &str = "127.0.0.1:8787";

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// HTTP MCP listen options.
#[derive(Debug, Clone)]
pub struct HttpServeOpts {
    /// `host:port` (`127.0.0.1:8787`).
    pub bind: String,
    /// Optional bearer token (`Authorization: Bearer …` or `X-ReelForge-Token`).
    pub token: Option<String>,
}

/// True when `bind` is loopback (`127.0.0.1`, `localhost`, `::1`).
#[must_use]
pub fn is_loopback_bind(spec: &str) -> bool {
    matches!(bind_host(spec), "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

/// Refuse non-loopback binds without a token.
///
/// # Errors
///
/// Empty token on a public bind.
pub fn require_token_for_bind(bind: &str, token: Option<&str>) -> Result<()> {
    let token = token.map(str::trim).filter(|t| !t.is_empty());
    if !is_loopback_bind(bind) && token.is_none() {
        return Err(HostError::message(format!(
            "refusing to bind `{bind}` without --token / REELFORGE_HOST_TOKEN"
        )));
    }
    Ok(())
}

/// Bind and serve until JSON-RPC `shutdown`.
///
/// # Errors
///
/// Bind, HTTP, or I/O.
pub fn serve_http(opts: HttpServeOpts) -> Result<()> {
    require_token_for_bind(&opts.bind, opts.token.as_deref())?;
    let listener = TcpListener::bind(&opts.bind)
        .map_err(|e| HostError::message(format!("http bind {}: {e}", opts.bind)))?;
    serve_http_listener(listener, opts.token)
}

/// Serve on an existing listener (tests bind `:0`).
///
/// # Errors
///
/// HTTP or I/O.
pub fn serve_http_listener(listener: TcpListener, token: Option<String>) -> Result<()> {
    let addr = listener
        .local_addr()
        .map_err(|e| HostError::message(format!("http local_addr: {e}")))?;
    eprintln!("reelforge-host MCP http://{addr}/mcp");
    let mut svc = HostService::new();
    let scope = svc.cancel_scope();
    let token = token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    let (jobs, rx) = mpsc::channel::<Job>();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_flag = Arc::clone(&shutdown);
    let worker = thread::spawn(move || {
        while let Ok(job) = rx.recv() {
            let response = handle_jsonrpc(&mut svc, &job.body);
            let stop = job.stop;
            let _ = job.reply.send(response);
            if stop {
                shutdown_flag.store(true, Ordering::Relaxed);
                let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(200));
                break;
            }
        }
    });
    for incoming in listener.incoming() {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let mut stream = match incoming {
            Ok(stream) => stream,
            Err(err) => {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                return Err(HostError::message(format!("http accept: {err}")));
            }
        };
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let jobs = jobs.clone();
        let scope = scope.clone();
        let token = token.clone();
        thread::spawn(move || {
            if let Err(err) = serve_one(&scope, &jobs, &mut stream, token.as_deref()) {
                let _ = write_http(
                    &mut stream,
                    400,
                    "application/json",
                    &json!({ "error": err.to_string() }).to_string(),
                );
            }
        });
    }
    drop(jobs);
    let _ = worker.join();
    Ok(())
}

struct Job {
    body: String,
    stop: bool,
    reply: Sender<Option<Value>>,
}

fn serve_one(
    scope: &CancelScope,
    jobs: &Sender<Job>,
    stream: &mut TcpStream,
    token: Option<&str>,
) -> Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let req = read_request(stream)?;
    if req.method == "OPTIONS" {
        write_http(stream, 204, "text/plain", "")?;
        return Ok(());
    }
    if req.method == "GET" && (req.path == "/" || req.path == "/health") {
        let body = json!({
            "ok": true,
            "name": "reelforge-host",
            "protocolVersion": MCP_PROTOCOL_VERSION,
        })
        .to_string();
        write_http(stream, 200, "application/json", &body)?;
        return Ok(());
    }
    if req.method != "POST" || (req.path != "/" && req.path != "/mcp") {
        write_http(
            stream,
            if req.method == "POST" { 404 } else { 405 },
            "application/json",
            &json!({ "error": "POST /mcp" }).to_string(),
        )?;
        return Ok(());
    }
    if let Some(want) = token
        && !token_matches(&req, want)
    {
        write_http(
            stream,
            401,
            "application/json",
            &json!({ "error": "unauthorized" }).to_string(),
        )?;
        return Ok(());
    }
    if scope.take_cancel_line(&req.body) {
        write_http(stream, 202, "application/json", "")?;
        return Ok(());
    }
    let (reply_tx, reply_rx) = mpsc::channel();
    jobs.send(Job {
        stop: method_is_shutdown(&req.body),
        body: req.body,
        reply: reply_tx,
    })
    .map_err(|_| HostError::message("http worker stopped"))?;
    match reply_rx.recv() {
        Ok(None) => write_http(stream, 202, "application/json", ""),
        Ok(Some(resp)) => write_http(stream, 200, "application/json", &resp.to_string()),
        Err(_) => Err(HostError::message("http worker stopped")),
    }
}

fn bind_host(spec: &str) -> &str {
    let spec = spec.trim();
    if let Some(rest) = spec.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        return &rest[..end];
    }
    spec.rsplit_once(':').map_or(spec, |(host, _)| host)
}

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

fn token_matches(req: &HttpRequest, want: &str) -> bool {
    for (name, value) in &req.headers {
        if name == "authorization" {
            let v = value.trim();
            if let Some(got) = v
                .strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
                && got == want
            {
                return true;
            }
        }
        if name == "x-reelforge-token" && value.trim() == want {
            return true;
        }
    }
    false
}

fn method_is_shutdown(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("method")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|m| m == "shutdown")
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 4096];
    let header_end = loop {
        let n = stream
            .read(&mut tmp)
            .map_err(|e| HostError::message(format!("http read: {e}")))?;
        if n == 0 {
            return Err(HostError::message("http: client closed"));
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_HEADER_BYTES {
            return Err(HostError::message("http: headers too large"));
        }
        if let Some(end) = find_header_end(&buf) {
            break end;
        }
    };
    let header_text = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = header_text.split('\n');
    let request_line = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| HostError::message("http: empty request"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| HostError::message("http: no method"))?
        .to_ascii_uppercase();
    let path = parts
        .next()
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_owned();

    let mut headers = Vec::new();
    let mut content_length = 0_usize;
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        if name == "content-length" {
            content_length = value.parse().unwrap_or(0);
        }
        headers.push((name, value));
    }
    if content_length > MAX_BODY_BYTES {
        return Err(HostError::message("http: body too large"));
    }

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream
            .read(&mut tmp)
            .map_err(|e| HostError::message(format!("http read body: {e}")))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
        if body.len() > MAX_BODY_BYTES {
            return Err(HostError::message("http: body too large"));
        }
    }
    body.truncate(content_length);
    Ok(HttpRequest {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .or_else(|| buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2))
}

fn write_http(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) -> Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         MCP-Protocol-Version: {MCP_PROTOCOL_VERSION}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type, Authorization, X-ReelForge-Token, MCP-Protocol-Version\r\n\
         \r\n",
        body.len()
    );
    stream
        .write_all(header.as_bytes())
        .and_then(|()| stream.write_all(body.as_bytes()))
        .and_then(|()| stream.flush())
        .map_err(|e| HostError::message(format!("http write: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn loopback_binds_do_not_need_token() {
        assert!(is_loopback_bind("127.0.0.1:8787"));
        assert!(is_loopback_bind("localhost:9"));
        assert!(is_loopback_bind("[::1]:8787"));
        assert!(!is_loopback_bind("0.0.0.0:8787"));
        assert!(!is_loopback_bind("192.168.1.5:80"));
        assert!(require_token_for_bind("127.0.0.1:1", None).is_ok());
        assert!(require_token_for_bind("0.0.0.0:8787", None).is_err());
        assert!(require_token_for_bind("0.0.0.0:8787", Some("secret")).is_ok());
    }

    #[test]
    fn cancel_notification_stops_the_inflight_call() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            serve_http_listener(listener, Some("secret".to_owned())).unwrap();
        });
        let mut ready = false;
        for _ in 0..50 {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_ok() {
                ready = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(ready, "http server did not accept");
        let (status, _) = post(
            addr,
            "/mcp",
            None,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        );
        assert_eq!(status, 401, "missing token");
        let done = Arc::new(AtomicBool::new(false));
        let finished = Arc::clone(&done);
        let waiter = thread::spawn(move || {
            let result = post(
                addr,
                "/mcp",
                Some("secret"),
                r#"{"jsonrpc":"2.0","id":5,"method":"wait_for_cancel"}"#,
            );
            finished.store(true, Ordering::Relaxed);
            result
        });
        thread::sleep(Duration::from_millis(80));
        let (status, _) = post(
            addr,
            "/mcp",
            None,
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":5}}"#,
        );
        assert_eq!(status, 401, "cancel without token");
        thread::sleep(Duration::from_millis(40));
        assert!(
            !done.load(Ordering::Relaxed),
            "unauthenticated cancel stopped the call"
        );
        let (status, _) = post(
            addr,
            "/mcp",
            Some("secret"),
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":5}}"#,
        );
        assert_eq!(status, 202, "cancel notification");
        let (status, body) = waiter.join().unwrap();
        assert_eq!(status, 200, "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("extract cancelled"),
            "{body}"
        );
        let _ = post(
            addr,
            "/mcp",
            Some("secret"),
            r#"{"jsonrpc":"2.0","id":99,"method":"shutdown"}"#,
        );
        server.join().unwrap();
    }

    fn post(
        addr: std::net::SocketAddr,
        path: &str,
        token: Option<&str>,
        body: &str,
    ) -> (u16, Value) {
        let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{auth}Connection: close\r\n\r\n{body}",
            body.len()
        );
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(req.as_bytes()).unwrap();
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf);
        let (head, raw) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        let value = serde_json::from_str(raw).unwrap_or_else(|_| json!({ "raw": raw }));
        (status, value)
    }
}
