//! Mock Streamable HTTP server for compatibility testing (HTTP→stdio client mode).
//!
//! Endpoints:
//!   POST /mcp     → JSON response or SSE stream for MCP requests; manages sessions
//!   GET /mcp      → SSE notification stream for an existing session
//!   DELETE /mcp   → Session termination (returns 200)
//!
//! Usage: mock_http_server [--port PORT]
//!
//! The server prints "LISTENING <port>" on stderr once ready.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

#[allow(dead_code)]
struct Session {
    id: String,
    initialized: bool,
    notification_streams: Vec<Arc<Mutex<TcpStream>>>,
}

type Sessions = Arc<Mutex<HashMap<String, Session>>>;

fn main() {
    let port = parse_port();
    let listener = TcpListener::bind(format!("127.0.0.1:{port}")).expect("bind");
    let actual_port = listener.local_addr().unwrap().port();
    eprintln!("LISTENING {actual_port}");

    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let sessions = sessions.clone();
        std::thread::spawn(move || {
            handle_connection(stream, sessions);
        });
    }
}

fn parse_port() -> u16 {
    let args: Vec<String> = std::env::args().collect();
    for i in 0..args.len() {
        if args[i] == "--port" {
            if let Some(p) = args.get(i + 1) {
                return p.parse().expect("invalid port");
            }
        }
    }
    0
}

fn handle_connection(mut stream: TcpStream, sessions: Sessions) {
    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }

    let mut headers_map: HashMap<String, String> = HashMap::new();
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            break;
        }
        if let Some((key, val)) = trimmed.split_once(':') {
            let key = key.trim().to_lowercase();
            let val = val.trim().to_string();
            if key == "content-length" {
                content_length = val.parse().unwrap_or(0);
            }
            headers_map.insert(key, val);
        }
    }

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    let path = parts[1];

    if !path.starts_with("/mcp") {
        let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        return;
    }

    let session_id = headers_map.get("mcp-session-id").cloned();

    match method {
        "POST" => {
            let mut body_buf = vec![0u8; content_length];
            if content_length > 0 {
                let _ = reader.read_exact(&mut body_buf);
            }
            let body = String::from_utf8_lossy(&body_buf).to_string();
            handle_post(&mut stream, &body, session_id, &sessions);
        }
        "GET" => {
            handle_get(stream, session_id, &sessions);
        }
        "DELETE" => {
            handle_delete(&mut stream, session_id, &sessions);
        }
        _ => {
            let _ = write!(
                stream,
                "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n"
            );
        }
    }
}

fn handle_post(
    stream: &mut TcpStream,
    body: &str,
    session_id: Option<String>,
    sessions: &Sessions,
) {
    let req: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            let err = json!({
                "jsonrpc": "2.0",
                "error": {"code": -32700, "message": format!("Parse error: {e}")},
                "id": null
            });
            send_json_response(stream, 400, &err, None);
            return;
        }
    };

    let method = req.get("method").and_then(|m: &Value| m.as_str()).unwrap_or("");
    let id = req.get("id").cloned();

    // Initialize creates a new session.
    if method == "initialize" {
        let new_id = uuid_v4();
        let session = Session {
            id: new_id.clone(),
            initialized: true,
            notification_streams: Vec::new(),
        };
        sessions.lock().unwrap().insert(new_id.clone(), session);

        let result = json!({
            "jsonrpc": "2.0",
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": { "listChanged": true }
                },
                "serverInfo": {
                    "name": "mock-http-mcp-server",
                    "version": "1.0.0"
                }
            },
            "id": id
        });
        send_json_response(stream, 200, &result, Some(&new_id));
        return;
    }

    // Non-init requests require a valid session.
    let sid = match session_id {
        Some(sid) => sid,
        None => {
            let err = json!({
                "jsonrpc": "2.0",
                "error": {"code": -32000, "message": "Bad Request: No valid session ID provided"},
                "id": id
            });
            send_json_response(stream, 400, &err, None);
            return;
        }
    };

    let exists = sessions.lock().unwrap().contains_key(&sid);
    if !exists {
        let err = json!({
            "jsonrpc": "2.0",
            "error": {"code": -32000, "message": "Session not found"},
            "id": id
        });
        send_json_response(stream, 404, &err, None);
        return;
    }

    // Notifications (no id) → 202.
    if id.is_none() || id.as_ref().map(|v: &Value| v.is_null()).unwrap_or(false) {
        let _ = write!(
            stream,
            "HTTP/1.1 202 Accepted\r\nMcp-Session-Id: {sid}\r\nContent-Length: 0\r\n\r\n"
        );
        let _ = stream.flush();
        return;
    }

    // Handle known methods.
    let result = match method {
        "tools/list" => {
            json!({
                "jsonrpc": "2.0",
                "result": {
                    "tools": [{
                        "name": "echo",
                        "description": "Echoes input back",
                        "inputSchema": {
                            "type": "object",
                            "properties": { "message": { "type": "string" } }
                        }
                    }]
                },
                "id": id
            })
        }
        "tools/call" => {
            let params = req.get("params").cloned().unwrap_or(json!({}));
            let tool_name = params.get("name").and_then(|n: &Value| n.as_str()).unwrap_or("");
            if tool_name == "echo" {
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                json!({
                    "jsonrpc": "2.0",
                    "result": {
                        "content": [{"type": "text", "text": serde_json::to_string(&arguments).unwrap()}]
                    },
                    "id": id
                })
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "error": {"code": -32601, "message": format!("Unknown tool: {tool_name}")},
                    "id": id
                })
            }
        }
        "echo" => {
            let params = req.get("params").cloned().unwrap_or(json!(null));
            json!({"jsonrpc": "2.0", "result": params, "id": id})
        }
        _ => {
            json!({
                "jsonrpc": "2.0",
                "error": {"code": -32601, "message": format!("Method not found: {method}")},
                "id": id
            })
        }
    };

    send_json_response(stream, 200, &result, Some(&sid));
}

fn handle_get(mut stream: TcpStream, session_id: Option<String>, sessions: &Sessions) {
    let sid = match session_id {
        Some(sid) => sid,
        None => {
            let _ = write!(
                stream,
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n"
            );
            return;
        }
    };

    let exists = sessions.lock().unwrap().contains_key(&sid);
    if !exists {
        let _ = write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"
        );
        return;
    }

    // SSE notification stream.
    let header = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         Connection: keep-alive\r\n\
         Mcp-Session-Id: {sid}\r\n\
         \r\n"
    );
    if stream.write_all(header.as_bytes()).is_err() {
        return;
    }
    let _ = stream.flush();

    // Register this stream for notifications.
    let stream_arc = Arc::new(Mutex::new(stream));
    if let Some(session) = sessions.lock().unwrap().get_mut(&sid) {
        session.notification_streams.push(stream_arc.clone());
    }

    // Keep alive.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(15));
        let mut guard = stream_arc.lock().unwrap();
        if guard.write_all(b": keepalive\n\n").is_err() {
            break;
        }
        if guard.flush().is_err() {
            break;
        }
    }
}

fn handle_delete(stream: &mut TcpStream, session_id: Option<String>, sessions: &Sessions) {
    let sid = match session_id {
        Some(sid) => sid,
        None => {
            let _ = write!(
                stream,
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n"
            );
            return;
        }
    };

    let removed = sessions.lock().unwrap().remove(&sid);
    if removed.is_some() {
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    } else {
        let _ = write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"
        );
    }
    let _ = stream.flush();
}

fn send_json_response(stream: &mut TcpStream, status: u16, body: &Value, session_id: Option<&str>) {
    let body_str = serde_json::to_string(body).unwrap();
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Unknown",
    };
    let session_header = match session_id {
        Some(sid) => format!("Mcp-Session-Id: {sid}\r\n"),
        None => String::new(),
    };
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: application/json\r\n\
         {session_header}\
         Content-Length: {}\r\n\
         \r\n\
         {body_str}",
        body_str.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Simple UUID v4 generation without external deps.
fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // Not cryptographically random, but sufficient for mock testing.
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (seed & 0xFFFFFFFF) as u32,
        ((seed >> 32) & 0xFFFF) as u16,
        ((seed >> 48) & 0x0FFF) as u16,
        (0x8000 | ((seed >> 60) & 0x3FFF)) as u16,
        ((seed >> 74) ^ seed) & 0xFFFFFFFFFFFF,
    )
}
