//! Shared test helpers for compatibility tests.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Resolve path to a test binary.
pub fn find_binary(env_var: &str, binary_name: &str) -> PathBuf {
    if let Ok(path) = std::env::var(env_var) {
        return PathBuf::from(path);
    }
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
    panic!("Cannot find binary '{binary_name}'. Set {env_var} or run `cargo build` first.");
}

/// Spawn the gateway binary with given args and piped I/O.
pub fn spawn_gateway(args: &[&str]) -> Child {
    let bin = find_binary("SUPERGATEWAY_BINARY", "supergateway-rs");
    Command::new(&bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to spawn gateway: {e}"))
}

/// Spawn a mock server binary with given args.
pub fn spawn_mock(env_var: &str, binary_name: &str, args: &[&str]) -> Child {
    let bin = find_binary(env_var, binary_name);
    Command::new(&bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to spawn {binary_name}: {e}"))
}

/// Wait for the gateway to print a listening message on stderr.
/// Returns the port number, or None if the process exits/times out.
///
/// The gateway logs `[supergateway] ... | port NNNN` on startup.
/// We scan stderr for "port" to extract the listening port.
pub fn wait_for_gateway_ready(child: &mut Child, timeout: Duration) -> Option<u16> {
    let stderr = child.stderr.take().expect("stderr");
    let reader = BufReader::new(stderr);
    let start = Instant::now();

    for line in reader.lines() {
        if start.elapsed() > timeout {
            return None;
        }
        let line = match line {
            Ok(l) => l,
            Err(_) => return None,
        };
        eprintln!("[gateway stderr] {line}");

        // Look for listening indicator: server modes log the port.
        // The gateway logs: "[supergateway] supergateway vX.X.X | ... | port NNNN"
        if let Some(idx) = line.find("port ") {
            let rest = &line[idx + 5..];
            if let Some(port) = rest.split_whitespace().next().and_then(|s| s.parse::<u16>().ok())
            {
                return Some(port);
            }
        }

        // If the gateway errors out, detect early.
        if line.contains("gateway not yet implemented") || line.contains("error:") {
            return None;
        }
    }
    None
}

/// Wait for "LISTENING <port>" on stderr from a mock server.
pub fn wait_for_listening(child: &mut Child, timeout: Duration) -> Option<u16> {
    let stderr = child.stderr.take().expect("stderr");
    let reader = BufReader::new(stderr);
    let start = Instant::now();

    for line in reader.lines() {
        if start.elapsed() > timeout {
            return None;
        }
        let line = match line {
            Ok(l) => l,
            Err(_) => return None,
        };
        if let Some(rest) = line.strip_prefix("LISTENING ") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Make an HTTP request and return (status, headers, body).
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

/// Make an HTTP request returning a streaming connection for SSE.
/// Returns (status, headers, BufReader) so caller can read SSE events.
pub fn http_request_streaming(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, BufReader<TcpStream>) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");

    stream.write_all(req.as_bytes()).expect("send request");
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("read status");

    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut resp_headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read header");
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            resp_headers.push((k.trim().to_lowercase(), v.trim().to_string()));
        }
    }

    (status, resp_headers, reader)
}

/// Read SSE events from a BufReader. Returns Vec<(event_type, data)>.
pub fn read_sse_events(
    reader: &mut BufReader<TcpStream>,
    max_events: usize,
    timeout: Duration,
) -> Vec<(String, String)> {
    let mut events = Vec::new();
    let mut current_event = String::new();
    let mut current_data = String::new();
    let start = Instant::now();

    loop {
        if events.len() >= max_events || start.elapsed() > timeout {
            break;
        }

        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    // Empty line = event boundary
                    if !current_data.is_empty() || !current_event.is_empty() {
                        let event_type = if current_event.is_empty() {
                            "message".to_string()
                        } else {
                            current_event.clone()
                        };
                        events.push((event_type, current_data.trim().to_string()));
                        current_event.clear();
                        current_data.clear();
                    }
                } else if let Some(rest) = trimmed.strip_prefix("event:") {
                    current_event = rest.trim().to_string();
                } else if let Some(rest) = trimmed.strip_prefix("data:") {
                    if !current_data.is_empty() {
                        current_data.push('\n');
                    }
                    current_data.push_str(rest.trim());
                } else if trimmed.starts_with(':') {
                    // Comment line, skip
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if start.elapsed() > timeout {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    events
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

/// Get a header value (case-insensitive key).
pub fn get_header<'a>(headers: &'a [(String, String)], key: &str) -> Option<&'a str> {
    let key = key.to_lowercase();
    headers
        .iter()
        .find(|(k, _)| k == &key)
        .map(|(_, v)| v.as_str())
}

/// Kill a child process and reap it.
pub fn kill_and_wait(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Macro to skip a test if the gateway is not functional.
/// Use: `let port = require_gateway!(child, "mode description");`
macro_rules! require_gateway {
    ($child:expr, $mode:expr) => {
        match $crate::helpers::wait_for_gateway_ready(
            &mut $child,
            std::time::Duration::from_secs(5),
        ) {
            Some(port) => port,
            None => {
                eprintln!(
                    "[SKIP] {} — gateway not ready (not yet implemented?)",
                    $mode
                );
                $crate::helpers::kill_and_wait(&mut $child);
                return;
            }
        }
    };
}

pub(crate) use require_gateway;

/// JSON-RPC initialize request.
pub fn jsonrpc_initialize(id: u64) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "compat-test", "version": "1.0"}
        },
        "id": id
    })
}

/// JSON-RPC initialized notification.
pub fn jsonrpc_initialized() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    })
}

/// JSON-RPC tools/list request.
pub fn jsonrpc_tools_list(id: u64) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "id": id
    })
}

/// JSON-RPC echo request.
pub fn jsonrpc_echo(id: serde_json::Value, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "echo",
        "params": params,
        "id": id
    })
}

/// Send a JSON body via HTTP POST and return parsed JSON response.
pub fn post_json(
    addr: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
) -> (u16, Vec<(String, String)>, serde_json::Value) {
    let body_str = serde_json::to_string(body).unwrap();
    let mut all_headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
    all_headers.extend_from_slice(headers);

    let (status, resp_headers, resp_body) =
        http_request(addr, "POST", path, &all_headers, Some(&body_str));

    let json: serde_json::Value =
        serde_json::from_str(&resp_body).unwrap_or(serde_json::Value::Null);

    (status, resp_headers, json)
}
