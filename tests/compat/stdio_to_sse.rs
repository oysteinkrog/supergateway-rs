//! Compatibility tests: stdio->SSE gateway mode.
//!
//! Spawns the gateway with `--stdio <mock_stdio_server> --outputTransport sse --port 0`
//! and verifies SSE streaming behavior.
//!
//! Behavioral notes:
//! - B-001: SSE broadcast to all connected clients
//! - B-002: POST always returns 202
//! - D-012: Stdin write serialization (improvement)
//! - D-017: 30s backpressure timeout (improvement)

use crate::helpers::*;
use std::time::Duration;

fn mock_stdio_cmd() -> String {
    find_binary("MOCK_STDIO_SERVER", "mock_stdio_server")
        .to_string_lossy()
        .to_string()
}

/// SSE connect: GET /sse returns event stream with endpoint event.
#[test]
fn sse_connect_returns_endpoint_event() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "sse",
        "--port",
        "0",
    ]);
    let port = require_gateway!(gw, "stdio->SSE connect");
    let addr = format!("127.0.0.1:{port}");

    // Connect to SSE endpoint.
    let (status, headers, mut reader) = http_request_streaming(&addr, "GET", "/sse", &[]);

    assert_eq!(status, 200, "SSE connect should return 200");
    assert_eq!(
        get_header(&headers, "content-type"),
        Some("text/event-stream"),
        "Should be SSE content type"
    );

    // Read the endpoint event.
    let events = read_sse_events(&mut reader, 1, Duration::from_secs(5));
    assert!(!events.is_empty(), "Should receive at least one SSE event");
    assert_eq!(events[0].0, "endpoint", "First event should be 'endpoint'");
    assert!(
        events[0].1.contains("/message"),
        "Endpoint event should contain message path"
    );

    kill_and_wait(&mut gw);
}

/// POST to /message returns 202 Accepted (B-002).
#[test]
fn post_message_returns_202() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "sse",
        "--port",
        "0",
    ]);
    let port = require_gateway!(gw, "stdio->SSE POST 202");
    let addr = format!("127.0.0.1:{port}");

    // First connect SSE to get the endpoint URL.
    let (_, _, mut reader) = http_request_streaming(&addr, "GET", "/sse", &[]);
    let events = read_sse_events(&mut reader, 1, Duration::from_secs(5));
    assert!(!events.is_empty(), "Need endpoint event");

    let message_path = &events[0].1;

    // POST a JSON-RPC request.
    let body = serde_json::to_string(&jsonrpc_initialize(1)).unwrap();
    let (status, _, _) = http_request(
        &addr,
        "POST",
        message_path,
        &[("Content-Type", "application/json")],
        Some(&body),
    );

    // B-002: POST always returns 202 in SSE mode.
    assert_eq!(status, 202, "POST should return 202 Accepted (B-002)");

    kill_and_wait(&mut gw);
}

/// Send message via POST, receive response on SSE stream.
#[test]
fn send_receive_roundtrip() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "sse",
        "--port",
        "0",
    ]);
    let port = require_gateway!(gw, "stdio->SSE roundtrip");
    let addr = format!("127.0.0.1:{port}");

    // Connect SSE.
    let (_, _, mut reader) = http_request_streaming(&addr, "GET", "/sse", &[]);
    let events = read_sse_events(&mut reader, 1, Duration::from_secs(5));
    let message_path = events[0].1.clone();

    // Send initialize.
    let body = serde_json::to_string(&jsonrpc_initialize(1)).unwrap();
    let _ = http_request(
        &addr,
        "POST",
        &message_path,
        &[("Content-Type", "application/json")],
        Some(&body),
    );

    // Read response from SSE stream.
    let response_events = read_sse_events(&mut reader, 1, Duration::from_secs(5));
    assert!(
        !response_events.is_empty(),
        "Should receive response on SSE stream"
    );

    let resp: serde_json::Value = serde_json::from_str(&response_events[0].1)
        .expect("SSE message should be valid JSON");
    assert_eq!(resp["jsonrpc"], "2.0");
    assert!(resp["result"].is_object(), "Should have result");
    assert_eq!(resp["id"], 1);

    kill_and_wait(&mut gw);
}

/// Broadcast: all connected SSE clients receive the same response (B-001).
#[test]
fn broadcast_to_all_clients() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "sse",
        "--port",
        "0",
    ]);
    let port = require_gateway!(gw, "stdio->SSE broadcast");
    let addr = format!("127.0.0.1:{port}");

    // Connect two SSE clients.
    let (_, _, mut reader1) = http_request_streaming(&addr, "GET", "/sse", &[]);
    let events1 = read_sse_events(&mut reader1, 1, Duration::from_secs(5));
    let message_path = events1[0].1.clone();

    let (_, _, mut reader2) = http_request_streaming(&addr, "GET", "/sse", &[]);
    let _events2 = read_sse_events(&mut reader2, 1, Duration::from_secs(5));

    // Send a message via POST.
    let body = serde_json::to_string(&jsonrpc_echo(
        serde_json::json!(42),
        serde_json::json!({"msg": "broadcast"}),
    ))
    .unwrap();
    let _ = http_request(
        &addr,
        "POST",
        &message_path,
        &[("Content-Type", "application/json")],
        Some(&body),
    );

    // Both clients should receive the response (B-001).
    let resp1 = read_sse_events(&mut reader1, 1, Duration::from_secs(5));
    let resp2 = read_sse_events(&mut reader2, 1, Duration::from_secs(5));

    assert!(!resp1.is_empty(), "Client 1 should receive broadcast");
    assert!(!resp2.is_empty(), "Client 2 should receive broadcast");

    kill_and_wait(&mut gw);
}

/// Health endpoint returns 200 when ready.
#[test]
fn health_endpoint() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "sse",
        "--port",
        "0",
        "--healthEndpoint",
        "/healthz",
    ]);
    let port = require_gateway!(gw, "stdio->SSE health");
    let addr = format!("127.0.0.1:{port}");

    let (status, _, body) = http_request(&addr, "GET", "/healthz", &[], None);
    assert_eq!(status, 200, "Health endpoint should return 200");
    assert!(body.contains("ok"), "Health body should contain 'ok'");

    kill_and_wait(&mut gw);
}

/// Custom headers are included in responses (D-019).
#[test]
fn custom_headers_in_response() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "sse",
        "--port",
        "0",
        "--header",
        "X-Custom-Test: hello-from-compat",
    ]);
    let port = require_gateway!(gw, "stdio->SSE custom headers");
    let addr = format!("127.0.0.1:{port}");

    // SSE connect should include custom header.
    let (_, headers, _) = http_request_streaming(&addr, "GET", "/sse", &[]);
    let custom = get_header(&headers, "x-custom-test");
    assert_eq!(
        custom,
        Some("hello-from-compat"),
        "Custom header should be present in SSE response"
    );

    kill_and_wait(&mut gw);
}

/// Custom SSE and message paths.
#[test]
fn custom_paths() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "sse",
        "--port",
        "0",
        "--ssePath",
        "/custom-sse",
        "--messagePath",
        "/custom-msg",
    ]);
    let port = require_gateway!(gw, "stdio->SSE custom paths");
    let addr = format!("127.0.0.1:{port}");

    // Default /sse should 404.
    let (status_default, _, _) = http_request(&addr, "GET", "/sse", &[], None);
    assert_eq!(status_default, 404, "Default /sse should 404");

    // Custom path should work.
    let (status_custom, headers, _) =
        http_request_streaming(&addr, "GET", "/custom-sse", &[]);
    assert_eq!(status_custom, 200, "Custom SSE path should work");
    assert_eq!(
        get_header(&headers, "content-type"),
        Some("text/event-stream")
    );

    kill_and_wait(&mut gw);
}
