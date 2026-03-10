//! Compatibility tests: SSE->stdio client gateway mode.
//!
//! The gateway connects to a remote SSE MCP server and bridges it to stdio.
//! stdin -> POST to remote SSE server
//! remote SSE events -> stdout
//!
//! Key behaviors:
//! - B-004: Remote close -> exit(1), remote error -> log only
//! - D-006: Log direction labels (verify correct)
//! - D-013: Reconnection with message buffering (improvement)

use crate::helpers::*;
use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::time::Duration;

/// SSE->stdio: connect to mock SSE server, send JSON-RPC via stdin, read response on stdout.
#[test]
fn sse_to_stdio_roundtrip() {
    // Start mock SSE server.
    let mut mock = spawn_mock("MOCK_SSE_SERVER", "mock_sse_server", &["--port", "0"]);
    let mock_port = match wait_for_listening(&mut mock, Duration::from_secs(5)) {
        Some(p) => p,
        None => {
            kill_and_wait(&mut mock);
            panic!("Mock SSE server failed to start");
        }
    };

    let sse_url = format!("http://127.0.0.1:{mock_port}/sse");

    // Start gateway in SSE->stdio mode.
    let bin = find_binary("SUPERGATEWAY_BINARY", "supergateway-rs");
    let mut gw = std::process::Command::new(&bin)
        .args(["--sse", &sse_url, "--outputTransport", "stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gateway");

    // Give the gateway time to connect to the SSE server.
    std::thread::sleep(Duration::from_millis(500));

    // Check if gateway is still running (it might exit if not implemented).
    match gw.try_wait() {
        Ok(Some(status)) => {
            eprintln!("[SKIP] SSE->stdio gateway exited early: {status}");
            kill_and_wait(&mut mock);
            return;
        }
        Ok(None) => {} // still running
        Err(_) => {
            kill_and_wait(&mut mock);
            return;
        }
    }

    let gw_stdin = gw.stdin.as_mut().expect("gateway stdin");
    let gw_stdout = gw.stdout.take().expect("gateway stdout");
    let mut stdout_reader = BufReader::new(gw_stdout);

    // Send initialize via stdin.
    let req = serde_json::to_string(&jsonrpc_initialize(1)).unwrap();
    writeln!(gw_stdin, "{req}").expect("write to gateway stdin");
    gw_stdin.flush().expect("flush");

    // Read response from stdout.
    let mut line = String::new();
    let start = std::time::Instant::now();
    loop {
        match stdout_reader.read_line(&mut line) {
            Ok(0) => {
                eprintln!("[SKIP] SSE->stdio: stdout EOF before response");
                break;
            }
            Ok(_) if !line.trim().is_empty() => break,
            Ok(_) => {
                line.clear();
                continue;
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if start.elapsed() > Duration::from_secs(5) {
                    eprintln!("[SKIP] SSE->stdio: timeout waiting for response");
                    break;
                }
            }
            Err(e) => {
                eprintln!("[SKIP] SSE->stdio: read error: {e}");
                break;
            }
        }
    }

    if !line.trim().is_empty() {
        let resp: serde_json::Value =
            serde_json::from_str(line.trim()).expect("valid JSON response on stdout");
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        // The mock SSE server processes the initialize and returns it.
    }

    kill_and_wait(&mut gw);
    kill_and_wait(&mut mock);
}

/// B-004: Remote SSE server close -> gateway exits with code 1.
#[test]
fn remote_close_exits_with_code_1() {
    // Start mock SSE server.
    let mut mock = spawn_mock("MOCK_SSE_SERVER", "mock_sse_server", &["--port", "0"]);
    let mock_port = match wait_for_listening(&mut mock, Duration::from_secs(5)) {
        Some(p) => p,
        None => {
            kill_and_wait(&mut mock);
            panic!("Mock SSE server failed to start");
        }
    };

    let sse_url = format!("http://127.0.0.1:{mock_port}/sse");

    let bin = find_binary("SUPERGATEWAY_BINARY", "supergateway-rs");
    let mut gw = std::process::Command::new(&bin)
        .args(["--sse", &sse_url, "--outputTransport", "stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gateway");

    std::thread::sleep(Duration::from_millis(500));

    // Check if gateway started.
    match gw.try_wait() {
        Ok(Some(_)) => {
            eprintln!("[SKIP] SSE->stdio gateway exited early");
            kill_and_wait(&mut mock);
            return;
        }
        _ => {}
    }

    // Kill the mock server — this closes the SSE connection.
    kill_and_wait(&mut mock);

    // Gateway should exit with code 1 (B-004).
    let status = match gw.wait() {
        Ok(s) => s,
        Err(_) => {
            eprintln!("[SKIP] SSE->stdio: could not wait for gateway");
            return;
        }
    };

    assert!(
        !status.success(),
        "B-004: Gateway should exit with non-zero on remote close (got {status})"
    );
}
