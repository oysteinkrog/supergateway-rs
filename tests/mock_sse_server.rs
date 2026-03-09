//! Mock SSE server for compatibility testing (SSE→stdio client mode).
//!
//! Endpoints:
//!   GET /sse         → SSE stream: "endpoint" event with POST URL, then message events
//!   POST /message    → 202 Accepted, forwards body to all SSE clients
//!
//! Usage: mock_sse_server [--port PORT]
//!
//! The server prints "LISTENING <port>" on stderr once ready.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

type SessionMap = Arc<Mutex<HashMap<u64, Arc<Mutex<TcpStream>>>>>;

fn main() {
    let port = parse_port();
    let listener = TcpListener::bind(format!("127.0.0.1:{port}")).expect("bind");
    let actual_port = listener.local_addr().unwrap().port();
    eprintln!("LISTENING {actual_port}");

    let sessions: SessionMap = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(std::sync::atomic::AtomicU64::new(1));

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let sessions = sessions.clone();
        let next_id = next_id.clone();
        std::thread::spawn(move || {
            handle_connection(stream, sessions, &next_id);
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
    0 // OS-assigned
}

fn handle_connection(
    mut stream: TcpStream,
    sessions: SessionMap,
    next_id: &std::sync::atomic::AtomicU64,
) {
    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }

    // Read all headers.
    let mut headers = Vec::new();
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
        if let Some(val) = trimmed.strip_prefix("Content-Length:") {
            content_length = val.trim().parse().unwrap_or(0);
        }
        headers.push(trimmed);
    }

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    let path = parts[1];

    match (method, path) {
        ("GET", "/sse") => handle_sse(stream, sessions, next_id),
        ("POST", p) if p.starts_with("/message") => {
            // Read body.
            let mut body_buf = vec![0u8; content_length];
            if content_length > 0 {
                use std::io::Read;
                let _ = reader.read_exact(&mut body_buf);
            }
            let body = String::from_utf8_lossy(&body_buf).to_string();

            // Extract session id from query param.
            let session_id = extract_query_param(p, "sessionId");
            handle_message(&mut stream, &body, sessions, session_id);
        }
        _ => {
            let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        }
    }
}

fn extract_query_param(path: &str, key: &str) -> Option<u64> {
    let query = path.split('?').nth(1)?;
    for pair in query.split('&') {
        let mut kv = pair.splitn(2, '=');
        if kv.next()? == key {
            return kv.next()?.parse().ok();
        }
    }
    None
}

fn handle_sse(
    mut stream: TcpStream,
    sessions: SessionMap,
    next_id: &std::sync::atomic::AtomicU64,
) {
    let id = next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Send SSE headers.
    let header = "HTTP/1.1 200 OK\r\n\
                  Content-Type: text/event-stream\r\n\
                  Cache-Control: no-cache\r\n\
                  Connection: keep-alive\r\n\
                  \r\n";
    if stream.write_all(header.as_bytes()).is_err() {
        return;
    }

    // Send endpoint event.
    let endpoint_data = format!("/message?sessionId={id}");
    let endpoint_event = format!("event: endpoint\ndata: {endpoint_data}\n\n");
    if stream.write_all(endpoint_event.as_bytes()).is_err() {
        return;
    }
    let _ = stream.flush();

    // Register session.
    let stream_arc = Arc::new(Mutex::new(stream));
    sessions.lock().unwrap().insert(id, stream_arc.clone());

    // Keep connection alive — block until the client disconnects.
    // We detect disconnect by trying to write periodic keep-alives.
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

    sessions.lock().unwrap().remove(&id);
}

fn handle_message(
    stream: &mut TcpStream,
    body: &str,
    sessions: SessionMap,
    session_id: Option<u64>,
) {
    // Send 202 Accepted.
    let _ = write!(
        stream,
        "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n"
    );
    let _ = stream.flush();

    // Forward body as SSE message event to targeted session or all sessions.
    let sse_msg = format!("event: message\ndata: {body}\n\n");
    let sessions_guard = sessions.lock().unwrap();

    if let Some(sid) = session_id {
        if let Some(client) = sessions_guard.get(&sid) {
            let mut client = client.lock().unwrap();
            let _ = client.write_all(sse_msg.as_bytes());
            let _ = client.flush();
        }
    } else {
        for client in sessions_guard.values() {
            let mut client = client.lock().unwrap();
            let _ = client.write_all(sse_msg.as_bytes());
            let _ = client.flush();
        }
    }
}
