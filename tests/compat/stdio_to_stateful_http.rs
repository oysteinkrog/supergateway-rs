//! Compatibility tests: stdio->Stateful Streamable HTTP gateway mode.
//!
//! Tests session lifecycle: initialize (creates session), POST with session ID,
//! GET SSE notification stream, DELETE session.
//!
//! Key behaviors:
//! - D-015: Session not found returns 404 (not 400)
//! - B-003: Session access counter
//! - D-102: Max concurrent sessions

use crate::helpers::*;
use std::time::Duration;

fn mock_stdio_cmd() -> String {
    find_binary("MOCK_STDIO_SERVER", "mock_stdio_server")
        .to_string_lossy()
        .to_string()
}

fn gateway_args(extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        "--stdio".to_string(),
        mock_stdio_cmd(),
        "--outputTransport".to_string(),
        "streamableHttp".to_string(),
        "--stateful".to_string(),
        "--port".to_string(),
        "0".to_string(),
    ];
    for a in extra {
        args.push(a.to_string());
    }
    args
}

/// Full session lifecycle: initialize -> request -> delete.
#[test]
fn session_lifecycle() {
    let args = gateway_args(&[]);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut gw = spawn_gateway(&args_ref);
    let port = require_gateway!(gw, "stdio->stateful HTTP lifecycle");
    let addr = format!("127.0.0.1:{port}");

    // 1. Initialize creates session.
    let (status, headers, resp) = post_json(&addr, "/mcp", &[], &jsonrpc_initialize(1));
    assert_eq!(status, 200, "Initialize should return 200");
    assert!(resp["result"]["capabilities"].is_object());
    assert_eq!(resp["id"], 1);

    let session_id = get_header(&headers, "mcp-session-id").expect("Mcp-Session-Id header");

    // 2. POST with session ID works.
    let (status2, _, resp2) = post_json(
        &addr,
        "/mcp",
        &[("Mcp-Session-Id", session_id)],
        &jsonrpc_tools_list(2),
    );
    assert_eq!(status2, 200);
    assert_eq!(resp2["result"]["tools"][0]["name"], "echo");
    assert_eq!(resp2["id"], 2);

    // 3. DELETE session.
    let (status3, _, _) = http_request(
        &addr,
        "DELETE",
        "/mcp",
        &[("Mcp-Session-Id", session_id)],
        None,
    );
    assert_eq!(status3, 200, "DELETE should return 200");

    // 4. POST after delete should 404 (D-015).
    let body = serde_json::to_string(&jsonrpc_tools_list(3)).unwrap();
    let (status4, _, _) = http_request(
        &addr,
        "POST",
        "/mcp",
        &[
            ("Content-Type", "application/json"),
            ("Mcp-Session-Id", session_id),
        ],
        Some(&body),
    );
    assert_eq!(status4, 404, "Post-delete should return 404 (D-015)");

    kill_and_wait(&mut gw);
}

/// Missing session ID on non-init request returns 400.
#[test]
fn missing_session_id_returns_400() {
    let args = gateway_args(&[]);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut gw = spawn_gateway(&args_ref);
    let port = require_gateway!(gw, "stdio->stateful HTTP missing session");
    let addr = format!("127.0.0.1:{port}");

    let (status, _, _) = post_json(&addr, "/mcp", &[], &jsonrpc_tools_list(1));
    assert_eq!(status, 400, "Missing session ID should return 400");

    kill_and_wait(&mut gw);
}

/// Invalid session ID returns 404 (D-015).
#[test]
fn invalid_session_id_returns_404() {
    let args = gateway_args(&[]);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut gw = spawn_gateway(&args_ref);
    let port = require_gateway!(gw, "stdio->stateful HTTP invalid session");
    let addr = format!("127.0.0.1:{port}");

    let (status, _, _) = post_json(
        &addr,
        "/mcp",
        &[("Mcp-Session-Id", "nonexistent-session-id")],
        &jsonrpc_tools_list(1),
    );
    assert_eq!(status, 404, "Nonexistent session should return 404 (D-015)");

    kill_and_wait(&mut gw);
}

/// Concurrent POSTs are correlated correctly (from bead notes).
#[test]
fn concurrent_post_correlation() {
    let args = gateway_args(&[]);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut gw = spawn_gateway(&args_ref);
    let port = require_gateway!(gw, "stdio->stateful HTTP concurrent POSTs");
    let addr = format!("127.0.0.1:{port}");

    // Initialize.
    let (_, headers, _) = post_json(&addr, "/mcp", &[], &jsonrpc_initialize(1));
    let session_id = get_header(&headers, "mcp-session-id")
        .expect("session id")
        .to_string();

    // Send two concurrent requests with distinct IDs.
    let addr2 = addr.clone();
    let sid2 = session_id.clone();
    let handle = std::thread::spawn(move || {
        post_json(
            &addr2,
            "/mcp",
            &[("Mcp-Session-Id", &sid2)],
            &jsonrpc_echo(serde_json::json!(100), serde_json::json!({"req": "A"})),
        )
    });

    let (_, _, resp_b) = post_json(
        &addr,
        "/mcp",
        &[("Mcp-Session-Id", &session_id)],
        &jsonrpc_echo(serde_json::json!(200), serde_json::json!({"req": "B"})),
    );

    let (_, _, resp_a) = handle.join().unwrap();

    // Each response should match its request ID.
    assert_eq!(resp_a["id"], 100, "Response A should have id=100");
    assert_eq!(resp_b["id"], 200, "Response B should have id=200");

    kill_and_wait(&mut gw);
}

/// GET /mcp with session ID opens SSE notification stream.
#[test]
fn get_sse_notification_stream() {
    let args = gateway_args(&[]);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut gw = spawn_gateway(&args_ref);
    let port = require_gateway!(gw, "stdio->stateful HTTP GET SSE");
    let addr = format!("127.0.0.1:{port}");

    // Initialize to create session.
    let (_, headers, _) = post_json(&addr, "/mcp", &[], &jsonrpc_initialize(1));
    let session_id = get_header(&headers, "mcp-session-id")
        .expect("session id")
        .to_string();

    // GET /mcp with session ID for SSE notification stream.
    let (status, resp_headers, _reader) = http_request_streaming(
        &addr,
        "GET",
        "/mcp",
        &[("Mcp-Session-Id", &session_id)],
    );
    assert_eq!(status, 200, "GET SSE stream should return 200");
    assert_eq!(
        get_header(&resp_headers, "content-type"),
        Some("text/event-stream"),
        "GET should return SSE stream"
    );

    kill_and_wait(&mut gw);
}

/// Notification (no id) returns 202.
#[test]
fn notification_returns_202() {
    let args = gateway_args(&[]);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut gw = spawn_gateway(&args_ref);
    let port = require_gateway!(gw, "stdio->stateful HTTP notification");
    let addr = format!("127.0.0.1:{port}");

    // Initialize.
    let (_, headers, _) = post_json(&addr, "/mcp", &[], &jsonrpc_initialize(1));
    let session_id = get_header(&headers, "mcp-session-id")
        .expect("session id")
        .to_string();

    // Send notification (no id).
    let notif = serde_json::to_string(&jsonrpc_initialized()).unwrap();
    let (status, _, _) = http_request(
        &addr,
        "POST",
        "/mcp",
        &[
            ("Content-Type", "application/json"),
            ("Mcp-Session-Id", &session_id),
        ],
        Some(&notif),
    );
    assert_eq!(status, 202, "Notification should return 202");

    kill_and_wait(&mut gw);
}

/// Session timeout closes session after inactivity.
#[test]
fn session_timeout() {
    let args = gateway_args(&["--sessionTimeout", "1000"]); // 1s timeout
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut gw = spawn_gateway(&args_ref);
    let port = require_gateway!(gw, "stdio->stateful HTTP timeout");
    let addr = format!("127.0.0.1:{port}");

    // Initialize.
    let (_, headers, _) = post_json(&addr, "/mcp", &[], &jsonrpc_initialize(1));
    let session_id = get_header(&headers, "mcp-session-id")
        .expect("session id")
        .to_string();

    // Wait for timeout.
    std::thread::sleep(Duration::from_millis(1500));

    // POST should fail — session timed out.
    let (status, _, _) = post_json(
        &addr,
        "/mcp",
        &[("Mcp-Session-Id", &session_id)],
        &jsonrpc_tools_list(2),
    );
    assert!(
        status == 404 || status == 503,
        "Timed-out session should return 404 or 503, got {status}"
    );

    kill_and_wait(&mut gw);
}

/// Session closing body check: POST during closing returns 503 with -32000 (from bead notes).
#[test]
fn session_closing_returns_503() {
    let args = gateway_args(&[]);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut gw = spawn_gateway(&args_ref);
    let port = require_gateway!(gw, "stdio->stateful HTTP closing");
    let addr = format!("127.0.0.1:{port}");

    // Initialize.
    let (_, headers, _) = post_json(&addr, "/mcp", &[], &jsonrpc_initialize(1));
    let session_id = get_header(&headers, "mcp-session-id")
        .expect("session id")
        .to_string();

    // DELETE session (starts closing).
    let _ = http_request(
        &addr,
        "DELETE",
        "/mcp",
        &[("Mcp-Session-Id", &session_id)],
        None,
    );

    // Immediately POST — might get 503 (closing) or 404 (already closed).
    // This depends on timing; the test verifies the gateway handles it gracefully.
    let (status, _, resp_body) = http_request(
        &addr,
        "POST",
        "/mcp",
        &[
            ("Content-Type", "application/json"),
            ("Mcp-Session-Id", &session_id),
        ],
        Some(&serde_json::to_string(&jsonrpc_tools_list(2)).unwrap()),
    );

    assert!(
        status == 503 || status == 404,
        "Post after DELETE should return 503 (closing) or 404 (closed), got {status}"
    );

    // If 503, verify JSON-RPC error body.
    if status == 503 {
        let resp: serde_json::Value = serde_json::from_str(&resp_body).unwrap_or_default();
        assert_eq!(
            resp["error"]["code"], -32000,
            "503 response should contain JSON-RPC error -32000"
        );
    }

    kill_and_wait(&mut gw);
}
