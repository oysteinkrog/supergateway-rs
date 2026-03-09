//! Compatibility test harness for supergateway-rs.
//!
//! Runs the same tests against either the TypeScript or Rust binary.
//!
//! Usage:
//!   cargo test --test compat_harness -- --nocapture
//!
//! Environment variables:
//!   SUPERGATEWAY_BINARY — path to the binary under test (default: cargo-built binary)
//!   MOCK_STDIO_SERVER   — path to mock stdio server binary (default: cargo-built)
//!   MOCK_SSE_SERVER     — path to mock SSE server binary (default: cargo-built)
//!   MOCK_HTTP_SERVER    — path to mock HTTP server binary (default: cargo-built)
//!
//! Failure classification:
//!   - parity: behavior matches TS reference (PASS)
//!   - intentional-divergence: documented in DIVERGENCES.md (PASS with note)
//!   - unspecified: unexpected difference (FAIL)

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// TypeScript supergateway reference commit for baseline testing.
pub const TS_REFERENCE_COMMIT: &str = "a453931ba5ffba4e5e3af7e0c49fcf28a90e8233";
pub const TS_REFERENCE_REPO: &str = "https://github.com/nicepkg/supergateway";

/// Failure classification for compatibility tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// Behavior matches TypeScript reference — test passes.
    Parity,
    /// Documented intentional divergence — test passes with note.
    IntentionalDivergence(&'static str),
    /// Unexpected behavioral difference — test fails.
    Unspecified,
}

impl std::fmt::Display for FailureClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parity => write!(f, "parity"),
            Self::IntentionalDivergence(id) => write!(f, "intentional-divergence({id})"),
            Self::Unspecified => write!(f, "unspecified"),
        }
    }
}

/// Result of a single compatibility test.
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    pub classification: FailureClass,
    pub detail: String,
}

impl TestResult {
    pub fn pass(name: &str) -> Self {
        Self {
            name: name.to_string(),
            passed: true,
            classification: FailureClass::Parity,
            detail: String::new(),
        }
    }

    pub fn pass_divergence(name: &str, divergence_id: &'static str, detail: &str) -> Self {
        Self {
            name: name.to_string(),
            passed: true,
            classification: FailureClass::IntentionalDivergence(divergence_id),
            detail: detail.to_string(),
        }
    }

    pub fn fail(name: &str, detail: &str) -> Self {
        Self {
            name: name.to_string(),
            passed: false,
            classification: FailureClass::Unspecified,
            detail: detail.to_string(),
        }
    }
}

/// Resolve path to a test binary (mock server or supergateway).
fn find_test_binary(env_var: &str, binary_name: &str) -> PathBuf {
    if let Ok(path) = std::env::var(env_var) {
        return PathBuf::from(path);
    }
    // Look in target/debug (test build) first, then target/release.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let debug = Path::new(manifest_dir)
        .join("target")
        .join("debug")
        .join(binary_name);
    if debug.exists() {
        return debug;
    }
    let release = Path::new(manifest_dir)
        .join("target")
        .join("release")
        .join(binary_name);
    if release.exists() {
        return release;
    }
    panic!(
        "Cannot find binary '{binary_name}'. Set {env_var} or run `cargo build --tests` first."
    );
}

/// Helper: spawn a child process with piped stdin/stdout/stderr.
pub fn spawn_piped(cmd: &Path, args: &[&str]) -> Child {
    Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to spawn {}: {e}", cmd.display()))
}

/// Helper: send a JSON-RPC request to a child via stdin, read response from stdout.
pub fn stdio_roundtrip(child: &mut Child, request: &serde_json::Value) -> serde_json::Value {
    let stdin = child.stdin.as_mut().expect("stdin");
    let msg = serde_json::to_string(request).unwrap();
    writeln!(stdin, "{msg}").expect("write to stdin");
    stdin.flush().expect("flush stdin");

    let stdout = child.stdout.as_mut().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();

    // Read with timeout via non-blocking loop.
    let start = std::time::Instant::now();
    loop {
        match reader.read_line(&mut line) {
            Ok(0) => panic!("EOF on stdout"),
            Ok(_) => break,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if start.elapsed() > Duration::from_secs(5) {
                    panic!("Timeout reading from stdout");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("Error reading stdout: {e}"),
        }
    }

    serde_json::from_str(line.trim()).unwrap_or_else(|e| {
        panic!("Invalid JSON response: {e}\nRaw: {line}");
    })
}

/// Wait for "LISTENING <port>" on stderr, return the port.
pub fn wait_for_listening(child: &mut Child) -> u16 {
    let stderr = child.stderr.as_mut().expect("stderr");
    let reader = BufReader::new(stderr);
    for line in reader.lines() {
        let line = line.expect("read stderr");
        if let Some(rest) = line.strip_prefix("LISTENING ") {
            return rest.trim().parse().expect("parse port");
        }
    }
    panic!("Server exited without printing LISTENING");
}

/// Helper: make an HTTP request and return (status_code, headers, body).
pub fn http_request(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> (u16, Vec<(String, String)>, String) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let body_bytes = body.unwrap_or("");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body_bytes.is_empty() {
        req.push_str(&format!("Content-Length: {}\r\n", body_bytes.len()));
    }
    req.push_str("Connection: close\r\n\r\n");
    if !body_bytes.is_empty() {
        req.push_str(body_bytes);
    }

    stream.write_all(req.as_bytes()).expect("send request");
    stream.flush().unwrap();

    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);

    parse_http_response(&response)
}

fn parse_http_response(raw: &str) -> (u16, Vec<(String, String)>, String) {
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let status_code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_lowercase(), v.trim().to_string()));
        }
    }

    (status_code, headers, body.to_string())
}

// ─── Smoke tests for mock servers ───

#[test]
fn smoke_mock_stdio_server_initialize() {
    let bin = find_test_binary("MOCK_STDIO_SERVER", "mock_stdio_server");
    let mut child = spawn_piped(&bin, &[]);

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        },
        "id": 1
    });

    let resp = stdio_roundtrip(&mut child, &req);
    assert_eq!(resp["jsonrpc"], "2.0");
    assert!(resp["result"]["capabilities"].is_object());
    assert_eq!(resp["id"], 1);
    assert_eq!(
        resp["result"]["serverInfo"]["name"],
        "mock-mcp-server"
    );

    // Clean shutdown.
    drop(child.stdin.take());
    let status = child.wait().expect("wait");
    // Exit 0 or killed is fine.
    let _ = status;
    println!("[SMOKE] mock_stdio_server: initialize ✓");
}

#[test]
fn smoke_mock_stdio_server_tools_list() {
    let bin = find_test_binary("MOCK_STDIO_SERVER", "mock_stdio_server");
    let mut child = spawn_piped(&bin, &[]);

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "id": 2
    });

    let resp = stdio_roundtrip(&mut child, &req);
    assert_eq!(resp["result"]["tools"][0]["name"], "echo");

    drop(child.stdin.take());
    let _ = child.wait();
    println!("[SMOKE] mock_stdio_server: tools/list ✓");
}

#[test]
fn smoke_mock_stdio_server_echo() {
    let bin = find_test_binary("MOCK_STDIO_SERVER", "mock_stdio_server");
    let mut child = spawn_piped(&bin, &[]);

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "echo",
        "params": {"hello": "world"},
        "id": 3
    });

    let resp = stdio_roundtrip(&mut child, &req);
    assert_eq!(resp["result"]["hello"], "world");

    drop(child.stdin.take());
    let _ = child.wait();
    println!("[SMOKE] mock_stdio_server: echo ✓");
}

#[test]
fn smoke_mock_stdio_server_notification_ignored() {
    let bin = find_test_binary("MOCK_STDIO_SERVER", "mock_stdio_server");
    let mut child = spawn_piped(&bin, &[]);

    // Send a notification (no id) — should not get a response.
    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "{}", serde_json::to_string(&notification).unwrap()).unwrap();
    stdin.flush().unwrap();

    // Send a real request to confirm the server is still alive.
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "echo",
        "params": {"after": "notification"},
        "id": 4
    });
    let resp = stdio_roundtrip(&mut child, &req);
    assert_eq!(resp["id"], 4);

    drop(child.stdin.take());
    let _ = child.wait();
    println!("[SMOKE] mock_stdio_server: notification ignored ✓");
}

#[test]
fn smoke_mock_stdio_server_notify_flag() {
    let bin = find_test_binary("MOCK_STDIO_SERVER", "mock_stdio_server");
    let mut child = spawn_piped(&bin, &["--notify"]);

    // Wait for a notification on stdout (should arrive within 1 second).
    let stdout = child.stdout.as_mut().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();

    let start = std::time::Instant::now();
    loop {
        match reader.read_line(&mut line) {
            Ok(0) => panic!("EOF before notification"),
            Ok(_) => break,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if start.elapsed() > Duration::from_secs(3) {
                    panic!("Timeout waiting for notification");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("Error: {e}"),
        }
    }

    let notif: serde_json::Value = serde_json::from_str(line.trim()).expect("parse notification");
    assert_eq!(notif["method"], "notifications/progress");
    assert!(notif.get("id").is_none());

    drop(child.stdin.take());
    let _ = child.kill();
    let _ = child.wait();
    println!("[SMOKE] mock_stdio_server: --notify ✓");
}

#[test]
fn smoke_mock_sse_server() {
    let bin = find_test_binary("MOCK_SSE_SERVER", "mock_sse_server");
    let mut child = Command::new(&bin)
        .args(["--port", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mock_sse_server");

    let port = wait_for_listening(&mut child);
    let addr = format!("127.0.0.1:{port}");

    // Connect to SSE endpoint.
    let mut stream = TcpStream::connect(&addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let req = format!("GET /sse HTTP/1.1\r\nHost: {addr}\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();

    let mut reader = BufReader::new(stream);
    let mut response = String::new();

    // Read until we see the endpoint event.
    let start = std::time::Instant::now();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                response.push_str(&line);
                if response.contains("event: endpoint") && response.contains("data: /message") {
                    break;
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if start.elapsed() > Duration::from_secs(5) {
                    panic!("Timeout waiting for SSE endpoint event");
                }
            }
            Err(e) => panic!("Error: {e}"),
        }
    }

    assert!(
        response.contains("text/event-stream"),
        "Expected SSE content type"
    );
    assert!(
        response.contains("event: endpoint"),
        "Expected endpoint event"
    );

    let _ = child.kill();
    let _ = child.wait();
    println!("[SMOKE] mock_sse_server: endpoint event ✓");
}

#[test]
fn smoke_mock_http_server_lifecycle() {
    let bin = find_test_binary("MOCK_HTTP_SERVER", "mock_http_server");
    let mut child = Command::new(&bin)
        .args(["--port", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mock_http_server");

    let port = wait_for_listening(&mut child);
    let addr = format!("127.0.0.1:{port}");

    // 1. POST /mcp initialize → should create session.
    let init_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        },
        "id": 1
    });

    let (status, headers, body) = http_request(
        &addr,
        "POST",
        "/mcp",
        &[("Content-Type", "application/json")],
        Some(&serde_json::to_string(&init_body).unwrap()),
    );

    assert_eq!(status, 200, "initialize should return 200");
    let resp: serde_json::Value = serde_json::from_str(&body).expect("parse init response");
    assert!(resp["result"]["capabilities"].is_object());

    // Extract session ID from response headers.
    let session_id = headers
        .iter()
        .find(|(k, _)| k == "mcp-session-id")
        .map(|(_, v)| v.clone())
        .expect("Mcp-Session-Id header");

    // 2. POST /mcp tools/list with session ID.
    let tools_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "id": 2
    });
    let (status2, _, body2) = http_request(
        &addr,
        "POST",
        "/mcp",
        &[
            ("Content-Type", "application/json"),
            ("Mcp-Session-Id", &session_id),
        ],
        Some(&serde_json::to_string(&tools_body).unwrap()),
    );
    assert_eq!(status2, 200);
    let resp2: serde_json::Value = serde_json::from_str(&body2).unwrap();
    assert_eq!(resp2["result"]["tools"][0]["name"], "echo");

    // 3. DELETE /mcp with session ID.
    let (status3, _, _) = http_request(
        &addr,
        "DELETE",
        "/mcp",
        &[("Mcp-Session-Id", &session_id)],
        None,
    );
    assert_eq!(status3, 200, "DELETE should return 200");

    // 4. POST after delete should 404.
    let (status4, _, _) = http_request(
        &addr,
        "POST",
        "/mcp",
        &[
            ("Content-Type", "application/json"),
            ("Mcp-Session-Id", &session_id),
        ],
        Some(&serde_json::to_string(&tools_body).unwrap()),
    );
    assert_eq!(status4, 404, "Post-delete request should 404");

    let _ = child.kill();
    let _ = child.wait();
    println!("[SMOKE] mock_http_server: full session lifecycle ✓");
}

#[test]
fn smoke_mock_http_server_missing_session() {
    let bin = find_test_binary("MOCK_HTTP_SERVER", "mock_http_server");
    let mut child = Command::new(&bin)
        .args(["--port", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mock_http_server");

    let port = wait_for_listening(&mut child);
    let addr = format!("127.0.0.1:{port}");

    // Non-init request without session ID → 400.
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "id": 1
    });
    let (status, _, _) = http_request(
        &addr,
        "POST",
        "/mcp",
        &[("Content-Type", "application/json")],
        Some(&serde_json::to_string(&body).unwrap()),
    );
    assert_eq!(status, 400, "Missing session ID should return 400");

    let _ = child.kill();
    let _ = child.wait();
    println!("[SMOKE] mock_http_server: missing session → 400 ✓");
}

/// Harness metadata.
#[test]
fn harness_metadata() {
    println!("=== Compatibility Test Harness ===");
    println!("TS reference commit: {TS_REFERENCE_COMMIT}");
    println!("TS reference repo:   {TS_REFERENCE_REPO}");
    println!("Failure classes: parity | intentional-divergence | unspecified");
    println!();

    // Verify binary resolution works.
    let mock_stdio = find_test_binary("MOCK_STDIO_SERVER", "mock_stdio_server");
    println!("Mock stdio server: {}", mock_stdio.display());

    let mock_sse = find_test_binary("MOCK_SSE_SERVER", "mock_sse_server");
    println!("Mock SSE server:   {}", mock_sse.display());

    let mock_http = find_test_binary("MOCK_HTTP_SERVER", "mock_http_server");
    println!("Mock HTTP server:  {}", mock_http.display());

    println!("All mock binaries found ✓");
}
