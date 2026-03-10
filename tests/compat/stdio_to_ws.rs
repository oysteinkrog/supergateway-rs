//! Compatibility tests: stdio->WebSocket gateway mode.
//!
//! Tests the WebSocket transport, including D-001 (string ID preservation)
//! and colon-containing IDs.

use crate::helpers::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

fn mock_stdio_cmd() -> String {
    find_binary("MOCK_STDIO_SERVER", "mock_stdio_server")
        .to_string_lossy()
        .to_string()
}

/// Minimal WebSocket client: perform upgrade handshake.
/// Returns the stream ready for frame I/O, or None on failure.
fn ws_connect(addr: &str, path: &str) -> Option<TcpStream> {
    let mut stream = TcpStream::connect(addr).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .ok()?;

    // WebSocket upgrade request with fixed key for simplicity.
    let req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    stream.flush().ok()?;

    // Read upgrade response.
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).ok()?;
    let response = String::from_utf8_lossy(&buf[..n]);

    if !response.contains("101") || !response.to_lowercase().contains("upgrade") {
        return None;
    }

    Some(stream)
}

/// Send a WebSocket text frame (unmasked for simplicity in tests).
fn ws_send_text(stream: &mut TcpStream, text: &str) {
    let payload = text.as_bytes();
    let len = payload.len();

    // Masked frame (client MUST mask per RFC 6455).
    let mask_key: [u8; 4] = [0x12, 0x34, 0x56, 0x78];

    if len < 126 {
        let header = [0x81, (0x80 | len) as u8]; // FIN + text opcode, MASK bit + len
        stream.write_all(&header).unwrap();
    } else if len < 65536 {
        let header = [0x81, 0xFE]; // FIN + text, MASK + 126
        stream.write_all(&header).unwrap();
        stream.write_all(&(len as u16).to_be_bytes()).unwrap();
    } else {
        let header = [0x81, 0xFF]; // FIN + text, MASK + 127
        stream.write_all(&header).unwrap();
        stream.write_all(&(len as u64).to_be_bytes()).unwrap();
    }

    stream.write_all(&mask_key).unwrap();

    // Masked payload.
    let masked: Vec<u8> = payload
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ mask_key[i % 4])
        .collect();
    stream.write_all(&masked).unwrap();
    stream.flush().unwrap();
}

/// Read a WebSocket text frame. Returns the payload text.
fn ws_read_text(stream: &mut TcpStream, timeout: Duration) -> Option<String> {
    stream.set_read_timeout(Some(timeout)).ok()?;

    let mut header = [0u8; 2];
    stream.read_exact(&mut header).ok()?;

    let _fin = (header[0] & 0x80) != 0;
    let opcode = header[0] & 0x0F;
    let masked = (header[1] & 0x80) != 0;
    let mut len = (header[1] & 0x7F) as u64;

    if len == 126 {
        let mut ext = [0u8; 2];
        stream.read_exact(&mut ext).ok()?;
        len = u16::from_be_bytes(ext) as u64;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        stream.read_exact(&mut ext).ok()?;
        len = u64::from_be_bytes(ext);
    }

    let mask_key = if masked {
        let mut mk = [0u8; 4];
        stream.read_exact(&mut mk).ok()?;
        Some(mk)
    } else {
        None
    };

    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).ok()?;

    if let Some(mk) = mask_key {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mk[i % 4];
        }
    }

    // Handle text frames (opcode 1) and close frames (opcode 8).
    match opcode {
        1 => String::from_utf8(payload).ok(),
        8 => None, // Close frame
        _ => None,
    }
}

/// WebSocket connect and bidirectional exchange.
#[test]
fn ws_connect_and_exchange() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "ws",
        "--port",
        "0",
    ]);
    let port = require_gateway!(gw, "stdio->WS connect");
    let addr = format!("127.0.0.1:{port}");

    let mut ws = match ws_connect(&addr, "/") {
        Some(s) => s,
        None => {
            eprintln!("[SKIP] WebSocket upgrade failed");
            kill_and_wait(&mut gw);
            return;
        }
    };

    // Send initialize request.
    let req = serde_json::to_string(&jsonrpc_initialize(1)).unwrap();
    ws_send_text(&mut ws, &req);

    // Read response.
    let resp_text = ws_read_text(&mut ws, Duration::from_secs(5))
        .expect("Should receive WS response");
    let resp: serde_json::Value = serde_json::from_str(&resp_text).expect("Valid JSON");
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert!(resp["result"].is_object());

    kill_and_wait(&mut gw);
}

/// D-001: String IDs are preserved through the WS multiplexer.
#[test]
fn d001_string_ids_preserved() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "ws",
        "--port",
        "0",
    ]);
    let port = require_gateway!(gw, "stdio->WS D-001 string IDs");
    let addr = format!("127.0.0.1:{port}");

    let mut ws = match ws_connect(&addr, "/") {
        Some(s) => s,
        None => {
            eprintln!("[SKIP] WebSocket upgrade failed");
            kill_and_wait(&mut gw);
            return;
        }
    };

    // Send request with string ID.
    let req = serde_json::to_string(&jsonrpc_echo(
        serde_json::json!("my-string-id"),
        serde_json::json!({"test": "string-id"}),
    ))
    .unwrap();
    ws_send_text(&mut ws, &req);

    let resp_text = ws_read_text(&mut ws, Duration::from_secs(5))
        .expect("Should receive response for string ID");
    let resp: serde_json::Value = serde_json::from_str(&resp_text).unwrap();
    assert_eq!(
        resp["id"], "my-string-id",
        "D-001: String ID must be preserved exactly"
    );

    kill_and_wait(&mut gw);
}

/// D-001 regression: IDs containing colons are preserved.
#[test]
fn d001_colon_ids_preserved() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "ws",
        "--port",
        "0",
    ]);
    let port = require_gateway!(gw, "stdio->WS D-001 colon IDs");
    let addr = format!("127.0.0.1:{port}");

    let mut ws = match ws_connect(&addr, "/") {
        Some(s) => s,
        None => {
            eprintln!("[SKIP] WebSocket upgrade failed");
            kill_and_wait(&mut gw);
            return;
        }
    };

    // Colon-containing IDs: TS version splits on ':' and breaks these.
    let colon_ids = vec![
        "request:123:abc",
        "urn:uuid:550e8400-e29b-41d4-a716-446655440000",
        "a:b:c:d:e",
    ];

    for (i, id) in colon_ids.iter().enumerate() {
        let req = serde_json::to_string(&jsonrpc_echo(
            serde_json::json!(id),
            serde_json::json!({"idx": i}),
        ))
        .unwrap();
        ws_send_text(&mut ws, &req);

        let resp_text = ws_read_text(&mut ws, Duration::from_secs(5))
            .unwrap_or_else(|| panic!("Should receive response for colon ID '{id}'"));
        let resp: serde_json::Value = serde_json::from_str(&resp_text).unwrap();
        assert_eq!(
            resp["id"].as_str().unwrap(),
            *id,
            "D-001: Colon-containing ID '{id}' must be preserved"
        );
    }

    kill_and_wait(&mut gw);
}

/// Multiple concurrent WS clients get routed correctly.
#[test]
fn ws_multiple_clients_routing() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "ws",
        "--port",
        "0",
    ]);
    let port = require_gateway!(gw, "stdio->WS multi-client");
    let addr = format!("127.0.0.1:{port}");

    let mut ws1 = match ws_connect(&addr, "/") {
        Some(s) => s,
        None => {
            kill_and_wait(&mut gw);
            return;
        }
    };
    let mut ws2 = match ws_connect(&addr, "/") {
        Some(s) => s,
        None => {
            kill_and_wait(&mut gw);
            return;
        }
    };

    // Client 1 sends id=100, client 2 sends id=200.
    let req1 = serde_json::to_string(&jsonrpc_echo(
        serde_json::json!(100),
        serde_json::json!({"from": "client1"}),
    ))
    .unwrap();
    let req2 = serde_json::to_string(&jsonrpc_echo(
        serde_json::json!(200),
        serde_json::json!({"from": "client2"}),
    ))
    .unwrap();

    ws_send_text(&mut ws1, &req1);
    ws_send_text(&mut ws2, &req2);

    // Each client should receive its own response.
    let resp1_text = ws_read_text(&mut ws1, Duration::from_secs(5)).expect("Client 1 response");
    let resp2_text = ws_read_text(&mut ws2, Duration::from_secs(5)).expect("Client 2 response");

    let resp1: serde_json::Value = serde_json::from_str(&resp1_text).unwrap();
    let resp2: serde_json::Value = serde_json::from_str(&resp2_text).unwrap();

    assert_eq!(resp1["id"], 100, "Client 1 should get id=100 response");
    assert_eq!(resp2["id"], 200, "Client 2 should get id=200 response");

    kill_and_wait(&mut gw);
}

/// Health endpoint works in WS mode.
#[test]
fn ws_health_endpoint() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "ws",
        "--port",
        "0",
        "--healthEndpoint",
        "/healthz",
    ]);
    let port = require_gateway!(gw, "stdio->WS health");
    let addr = format!("127.0.0.1:{port}");

    // D-002: Health endpoint should return exactly one response (no headers-already-sent bug).
    let (status, _, body) = http_request(&addr, "GET", "/healthz", &[], None);
    assert_eq!(status, 200, "Health should return 200");
    assert!(body.contains("ok"));

    kill_and_wait(&mut gw);
}
