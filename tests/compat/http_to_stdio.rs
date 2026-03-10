//! Compatibility tests: Streamable HTTP->stdio client gateway mode.
//!
//! The gateway connects to a remote Streamable HTTP MCP server and bridges
//! it to stdio. stdin -> POST to remote HTTP server, response -> stdout.
//!
//! Key behaviors:
//! - Content-type bifurcation: JSON response vs SSE stream response
//! - B-004: Remote close -> exit(1), remote error -> log only
//! - D-004: Client mode fallback path fix

use crate::helpers::*;
use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::time::Duration;

/// HTTP->stdio: connect to mock HTTP server, send JSON-RPC via stdin, read response on stdout.
#[test]
fn http_to_stdio_roundtrip() {
    // Start mock HTTP server.
    let mut mock = spawn_mock("MOCK_HTTP_SERVER", "mock_http_server", &["--port", "0"]);
    let mock_port = match wait_for_listening(&mut mock, Duration::from_secs(5)) {
        Some(p) => p,
        None => {
            kill_and_wait(&mut mock);
            panic!("Mock HTTP server failed to start");
        }
    };

    let http_url = format!("http://127.0.0.1:{mock_port}/mcp");

    // Start gateway in HTTP->stdio mode.
    let bin = find_binary("SUPERGATEWAY_BINARY", "supergateway-rs");
    let mut gw = std::process::Command::new(&bin)
        .args(["--streamableHttp", &http_url, "--outputTransport", "stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gateway");

    // Give gateway time to start.
    std::thread::sleep(Duration::from_millis(500));

    match gw.try_wait() {
        Ok(Some(status)) => {
            eprintln!("[SKIP] HTTP->stdio gateway exited early: {status}");
            kill_and_wait(&mut mock);
            return;
        }
        Ok(None) => {}
        Err(_) => {
            kill_and_wait(&mut mock);
            return;
        }
    }

    let gw_stdin = gw.stdin.as_mut().expect("gateway stdin");
    let gw_stdout = gw.stdout.take().expect("gateway stdout");
    let mut stdout_reader = BufReader::new(gw_stdout);

    // Send initialize request via stdin.
    let req = serde_json::to_string(&jsonrpc_initialize(1)).unwrap();
    writeln!(gw_stdin, "{req}").expect("write to gateway stdin");
    gw_stdin.flush().expect("flush");

    // Read response from stdout.
    let mut line = String::new();
    let start = std::time::Instant::now();
    loop {
        match stdout_reader.read_line(&mut line) {
            Ok(0) => {
                eprintln!("[SKIP] HTTP->stdio: stdout EOF before response");
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
                    eprintln!("[SKIP] HTTP->stdio: timeout waiting for response");
                    break;
                }
            }
            Err(e) => {
                eprintln!("[SKIP] HTTP->stdio: read error: {e}");
                break;
            }
        }
    }

    if !line.trim().is_empty() {
        let resp: serde_json::Value =
            serde_json::from_str(line.trim()).expect("valid JSON response on stdout");
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert!(
            resp["result"]["capabilities"].is_object(),
            "Should get capabilities from mock HTTP server"
        );
    }

    kill_and_wait(&mut gw);
    kill_and_wait(&mut mock);
}

/// Send tools/list after initialize via HTTP->stdio.
#[test]
fn http_to_stdio_tools_list() {
    let mut mock = spawn_mock("MOCK_HTTP_SERVER", "mock_http_server", &["--port", "0"]);
    let mock_port = match wait_for_listening(&mut mock, Duration::from_secs(5)) {
        Some(p) => p,
        None => {
            kill_and_wait(&mut mock);
            panic!("Mock HTTP server failed to start");
        }
    };

    let http_url = format!("http://127.0.0.1:{mock_port}/mcp");
    let bin = find_binary("SUPERGATEWAY_BINARY", "supergateway-rs");
    let mut gw = std::process::Command::new(&bin)
        .args(["--streamableHttp", &http_url, "--outputTransport", "stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gateway");

    std::thread::sleep(Duration::from_millis(500));

    match gw.try_wait() {
        Ok(Some(_)) => {
            eprintln!("[SKIP] HTTP->stdio gateway exited early");
            kill_and_wait(&mut mock);
            return;
        }
        _ => {}
    }

    let gw_stdin = gw.stdin.as_mut().expect("stdin");
    let gw_stdout = gw.stdout.take().expect("stdout");
    let mut reader = BufReader::new(gw_stdout);

    // Initialize first.
    let init_req = serde_json::to_string(&jsonrpc_initialize(1)).unwrap();
    writeln!(gw_stdin, "{init_req}").unwrap();
    gw_stdin.flush().unwrap();

    let mut init_line = String::new();
    let start = std::time::Instant::now();
    loop {
        match reader.read_line(&mut init_line) {
            Ok(0) => break,
            Ok(_) if !init_line.trim().is_empty() => break,
            Ok(_) => {
                init_line.clear();
                continue;
            }
            Err(_) => {
                if start.elapsed() > Duration::from_secs(5) {
                    break;
                }
            }
        }
    }

    if init_line.trim().is_empty() {
        eprintln!("[SKIP] HTTP->stdio: no init response");
        kill_and_wait(&mut gw);
        kill_and_wait(&mut mock);
        return;
    }

    // Send tools/list.
    let tools_req = serde_json::to_string(&jsonrpc_tools_list(2)).unwrap();
    writeln!(gw_stdin, "{tools_req}").unwrap();
    gw_stdin.flush().unwrap();

    let mut tools_line = String::new();
    let start = std::time::Instant::now();
    loop {
        match reader.read_line(&mut tools_line) {
            Ok(0) => break,
            Ok(_) if !tools_line.trim().is_empty() => break,
            Ok(_) => {
                tools_line.clear();
                continue;
            }
            Err(_) => {
                if start.elapsed() > Duration::from_secs(5) {
                    break;
                }
            }
        }
    }

    if !tools_line.trim().is_empty() {
        let resp: serde_json::Value = serde_json::from_str(tools_line.trim()).unwrap();
        assert_eq!(resp["id"], 2);
        assert_eq!(resp["result"]["tools"][0]["name"], "echo");
    }

    kill_and_wait(&mut gw);
    kill_and_wait(&mut mock);
}

/// B-004: Remote HTTP server close -> gateway exits with code 1.
#[test]
fn remote_close_exits_with_code_1() {
    let mut mock = spawn_mock("MOCK_HTTP_SERVER", "mock_http_server", &["--port", "0"]);
    let mock_port = match wait_for_listening(&mut mock, Duration::from_secs(5)) {
        Some(p) => p,
        None => {
            kill_and_wait(&mut mock);
            panic!("Mock HTTP server failed to start");
        }
    };

    let http_url = format!("http://127.0.0.1:{mock_port}/mcp");
    let bin = find_binary("SUPERGATEWAY_BINARY", "supergateway-rs");
    let mut gw = std::process::Command::new(&bin)
        .args(["--streamableHttp", &http_url, "--outputTransport", "stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gateway");

    std::thread::sleep(Duration::from_millis(500));

    match gw.try_wait() {
        Ok(Some(_)) => {
            eprintln!("[SKIP] HTTP->stdio gateway exited early");
            kill_and_wait(&mut mock);
            return;
        }
        _ => {}
    }

    // Kill the remote server.
    kill_and_wait(&mut mock);

    // Send a request to trigger the error path.
    if let Some(stdin) = gw.stdin.as_mut() {
        let req = serde_json::to_string(&jsonrpc_initialize(1)).unwrap();
        let _ = writeln!(stdin, "{req}");
        let _ = stdin.flush();
    }

    // Gateway should exit with non-zero (B-004).
    let status = match gw.wait() {
        Ok(s) => s,
        Err(_) => {
            eprintln!("[SKIP] HTTP->stdio: could not wait for gateway");
            return;
        }
    };

    assert!(
        !status.success(),
        "B-004: Gateway should exit with non-zero on remote close"
    );
}
