//! HTTP MCP transport — no ONNX, no ffmpeg.

use reelforge_host::{MCP_PROTOCOL_VERSION, serve_http_listener};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

fn spawn(token: Option<&str>) -> (std::net::SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let token = token.map(str::to_owned);
    let handle = thread::spawn(move || {
        serve_http_listener(listener, token).unwrap();
    });
    wait_up(addr);
    (addr, handle)
}

fn wait_up(addr: std::net::SocketAddr) {
    for _ in 0..50 {
        if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(50)) {
            let req = format!("GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
            if stream.write_all(req.as_bytes()).is_ok() {
                let mut buf = Vec::new();
                let _ = stream.read_to_end(&mut buf);
                if String::from_utf8_lossy(&buf).contains("\"ok\"") {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("http server did not accept on {addr}");
}

fn exchange(addr: std::net::SocketAddr, raw: &str) -> (u16, String) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.write_all(raw.as_bytes()).unwrap();
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, body.to_owned())
}

fn post(addr: std::net::SocketAddr, path: &str, token: Option<&str>, body: &str) -> (u16, Value) {
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{auth}Connection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, raw) = exchange(addr, &req);
    let value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({ "raw": raw }));
    (status, value)
}

fn shutdown(addr: std::net::SocketAddr, token: Option<&str>, handle: thread::JoinHandle<()>) {
    let _ = post(
        addr,
        "/mcp",
        token,
        r#"{"jsonrpc":"2.0","id":99,"method":"shutdown"}"#,
    );
    handle.join().unwrap();
}

#[test]
fn health_and_tools_list() {
    let (addr, handle) = spawn(None);
    let (status, raw) = exchange(
        addr,
        &format!("GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(status, 200, "{raw}");
    let health: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(health["ok"], true);
    assert_eq!(health["protocolVersion"], MCP_PROTOCOL_VERSION);

    let (status, init) = post(
        addr,
        "/mcp",
        None,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    assert_eq!(status, 200);
    assert_eq!(init["result"]["serverInfo"]["name"], "reelforge-host");

    let (status, listed) = post(
        addr,
        "/mcp",
        None,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    assert_eq!(status, 200);
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(names.contains(&"privacy_except"));
    assert!(names.contains(&"rewrite_plan"));

    shutdown(addr, None, handle);
}

#[test]
fn rewrite_plan_over_http() {
    let (addr, handle) = spawn(None);
    let body = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "rewrite_plan",
            "arguments": {
                "plan": {
                    "version": 2,
                    "media": "scene.mp4",
                    "edits": [{
                        "type": "blur_everyone_except",
                        "allowed": {
                            "kind": "frame_pick",
                            "media": "alice.jpg",
                            "frame_index": 0,
                            "box_xyxy": [0.0, 0.0, 10.0, 10.0]
                        }
                    }]
                },
                "bindings": [{
                    "media": "alice.jpg",
                    "frame_index": 0,
                    "box_xyxy": [0.0, 0.0, 10.0, 10.0],
                    "ids": [1]
                }]
            }
        }
    })
    .to_string();
    let (status, out) = post(addr, "/mcp", None, &body);
    assert_eq!(status, 200, "{out}");
    let text = out["result"]["structuredContent"]["edits"][0]["allowed"]["kind"].as_str();
    assert_eq!(text, Some("subject_ids"), "{out}");
    shutdown(addr, None, handle);
}

#[test]
fn token_required() {
    let (addr, handle) = spawn(Some("secret"));
    let (status, body) = post(
        addr,
        "/mcp",
        None,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    assert_eq!(status, 401, "{body}");

    let (status, init) = post(
        addr,
        "/mcp",
        Some("secret"),
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    assert_eq!(status, 200, "{init}");
    assert_eq!(init["result"]["serverInfo"]["name"], "reelforge-host");
    shutdown(addr, Some("secret"), handle);
}

#[test]
fn cors_preflight_allows_browser() {
    let (addr, handle) = spawn(None);
    let (status, _) = exchange(
        addr,
        &format!(
            "OPTIONS /mcp HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://127.0.0.1:5173\r\nAccess-Control-Request-Method: POST\r\nConnection: close\r\n\r\n"
        ),
    );
    assert_eq!(status, 204);
    shutdown(addr, None, handle);
}

#[test]
fn unknown_path_is_404() {
    let (addr, handle) = spawn(None);
    let (status, _) = post(addr, "/nope", None, "{}");
    assert_eq!(status, 404);
    shutdown(addr, None, handle);
}
