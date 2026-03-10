//! Compatibility tests: stdio->Stateless Streamable HTTP gateway mode.
//!
//! In stateless mode, each request spawns a new child process.
//! Initialize requests pass through directly; non-init requests trigger
//! automatic initialization before forwarding.
//!
//! Key behaviors:
//! - D-005: Auto-init batch message buffer (improvement)
//! - D-101: Max message size enforcement

use crate::helpers::*;

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
        // No --stateful = stateless mode.
        "--port".to_string(),
        "0".to_string(),
    ];
    for a in extra {
        args.push(a.to_string());
    }
    args
}

/// Initialize request passes through to child and returns response.
#[test]
fn initialize_passthrough() {
    let args = gateway_args(&[]);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut gw = spawn_gateway(&args_ref);
    let port = require_gateway!(gw, "stdio->stateless HTTP init");
    let addr = format!("127.0.0.1:{port}");

    let (status, _, resp) = post_json(&addr, "/mcp", &[], &jsonrpc_initialize(1));
    assert_eq!(status, 200, "Initialize should return 200");
    assert!(resp["result"]["capabilities"].is_object());
    assert_eq!(resp["id"], 1);
    assert_eq!(
        resp["result"]["serverInfo"]["name"], "mock-mcp-server",
        "Response should come from mock server"
    );

    kill_and_wait(&mut gw);
}

/// Non-init request triggers auto-initialization, then forwards the request.
#[test]
fn auto_init_for_non_init_request() {
    let args = gateway_args(&[]);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut gw = spawn_gateway(&args_ref);
    let port = require_gateway!(gw, "stdio->stateless HTTP auto-init");
    let addr = format!("127.0.0.1:{port}");

    // Send tools/list without prior initialize — gateway should auto-init.
    let (status, _, resp) = post_json(&addr, "/mcp", &[], &jsonrpc_tools_list(1));
    assert_eq!(status, 200, "Auto-init should succeed");
    assert_eq!(
        resp["result"]["tools"][0]["name"], "echo",
        "Should get tools list after auto-init"
    );
    assert_eq!(resp["id"], 1);

    kill_and_wait(&mut gw);
}

/// Echo request through stateless mode.
#[test]
fn echo_through_stateless() {
    let args = gateway_args(&[]);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut gw = spawn_gateway(&args_ref);
    let port = require_gateway!(gw, "stdio->stateless HTTP echo");
    let addr = format!("127.0.0.1:{port}");

    let (status, _, resp) = post_json(
        &addr,
        "/mcp",
        &[],
        &jsonrpc_echo(
            serde_json::json!(42),
            serde_json::json!({"hello": "stateless"}),
        ),
    );
    assert_eq!(status, 200);
    assert_eq!(resp["id"], 42);
    assert_eq!(resp["result"]["hello"], "stateless");

    kill_and_wait(&mut gw);
}

/// Multiple sequential requests each get fresh child (stateless).
#[test]
fn sequential_requests_independent() {
    let args = gateway_args(&[]);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut gw = spawn_gateway(&args_ref);
    let port = require_gateway!(gw, "stdio->stateless HTTP sequential");
    let addr = format!("127.0.0.1:{port}");

    for i in 1..=3 {
        let (status, _, resp) = post_json(
            &addr,
            "/mcp",
            &[],
            &jsonrpc_echo(
                serde_json::json!(i),
                serde_json::json!({"seq": i}),
            ),
        );
        assert_eq!(status, 200, "Request {i} should succeed");
        assert_eq!(resp["id"], i, "Request {i} ID preserved");
    }

    kill_and_wait(&mut gw);
}

/// DELETE in stateless mode returns 405 (no sessions to delete).
#[test]
fn delete_returns_405_in_stateless() {
    let args = gateway_args(&[]);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut gw = spawn_gateway(&args_ref);
    let port = require_gateway!(gw, "stdio->stateless HTTP DELETE");
    let addr = format!("127.0.0.1:{port}");

    let (status, _, _) = http_request(&addr, "DELETE", "/mcp", &[], None);
    // Stateless mode has no sessions — DELETE should be rejected.
    assert!(
        status == 405 || status == 400,
        "DELETE in stateless mode should return 405 or 400, got {status}"
    );

    kill_and_wait(&mut gw);
}

/// Custom streamableHttpPath.
#[test]
fn custom_http_path() {
    let args = gateway_args(&["--streamableHttpPath", "/custom-mcp"]);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut gw = spawn_gateway(&args_ref);
    let port = require_gateway!(gw, "stdio->stateless HTTP custom path");
    let addr = format!("127.0.0.1:{port}");

    // Default path should 404.
    let (status_default, _, _) = post_json(&addr, "/mcp", &[], &jsonrpc_initialize(1));
    assert_eq!(status_default, 404, "Default /mcp should 404");

    // Custom path should work.
    let (status_custom, _, resp) = post_json(&addr, "/custom-mcp", &[], &jsonrpc_initialize(1));
    assert_eq!(status_custom, 200, "Custom path should work");
    assert!(resp["result"]["capabilities"].is_object());

    kill_and_wait(&mut gw);
}

/// Health endpoint works in stateless mode.
#[test]
fn stateless_health_endpoint() {
    let args = gateway_args(&["--healthEndpoint", "/healthz"]);
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut gw = spawn_gateway(&args_ref);
    let port = require_gateway!(gw, "stdio->stateless HTTP health");
    let addr = format!("127.0.0.1:{port}");

    let (status, _, body) = http_request(&addr, "GET", "/healthz", &[], None);
    assert_eq!(status, 200);
    assert!(body.contains("ok"));

    kill_and_wait(&mut gw);
}
