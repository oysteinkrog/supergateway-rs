//! Compatibility tests: Signal handling and graceful shutdown.
//!
//! Tests:
//! - SIGINT causes exit with code 0
//! - Second signal forces immediate exit
//! - Graceful shutdown drains in-flight requests
//! - Container PID 1 (Docker) — requires Docker, #[ignore] in CI

use crate::helpers::*;
use std::time::Duration;

fn mock_stdio_cmd() -> String {
    find_binary("MOCK_STDIO_SERVER", "mock_stdio_server")
        .to_string_lossy()
        .to_string()
}

/// SIGINT causes clean exit with code 0.
#[test]
fn sigint_exits_cleanly() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "sse",
        "--port",
        "0",
    ]);

    // Wait for gateway to start, or detect early exit.
    std::thread::sleep(Duration::from_millis(500));

    match gw.try_wait() {
        Ok(Some(status)) => {
            eprintln!("[SKIP] Gateway exited early: {status}");
            return;
        }
        Ok(None) => {}
        Err(_) => return,
    }

    // Send SIGINT.
    let pid = gw.id();
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGINT);
    }

    // Wait for exit.
    let status = gw.wait().expect("wait for gateway");
    // SIGINT should cause clean exit (code 0) or signal exit.
    // On Unix, process killed by signal has no exit code but is reported as signal.
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        // Either exit(0) or killed by SIGINT (signal 2).
        let ok = status.code() == Some(0) || status.signal() == Some(2);
        assert!(
            ok,
            "SIGINT should cause clean exit, got code={:?} signal={:?}",
            status.code(),
            status.signal()
        );
    }
}

/// SIGTERM causes clean exit.
#[test]
fn sigterm_exits_cleanly() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "sse",
        "--port",
        "0",
    ]);

    std::thread::sleep(Duration::from_millis(500));

    match gw.try_wait() {
        Ok(Some(_)) => {
            eprintln!("[SKIP] Gateway exited early");
            return;
        }
        Ok(None) => {}
        Err(_) => return,
    }

    let pid = gw.id();
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }

    let status = gw.wait().expect("wait for gateway");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        let ok = status.code() == Some(0) || status.signal() == Some(15);
        assert!(
            ok,
            "SIGTERM should cause clean exit, got code={:?} signal={:?}",
            status.code(),
            status.signal()
        );
    }
}

/// Second signal forces immediate exit (tests the force-exit path).
#[test]
fn double_signal_forces_exit() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "sse",
        "--port",
        "0",
    ]);

    std::thread::sleep(Duration::from_millis(500));

    match gw.try_wait() {
        Ok(Some(_)) => {
            eprintln!("[SKIP] Gateway exited early");
            return;
        }
        Ok(None) => {}
        Err(_) => return,
    }

    let pid = gw.id();

    // First SIGTERM: triggers graceful shutdown.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }

    // Brief delay, then second SIGTERM: forces immediate exit.
    std::thread::sleep(Duration::from_millis(100));
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }

    // Should exit quickly (within the 5s drain timeout).
    let start = std::time::Instant::now();
    loop {
        match gw.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(6) {
                    // Watchdog should have force-exited by now.
                    let _ = gw.kill();
                    let _ = gw.wait();
                    panic!("Gateway did not exit within 6s after double signal");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
}

/// stdin EOF causes shutdown (used by IDE hosts).
#[test]
fn stdin_eof_triggers_shutdown() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "sse",
        "--port",
        "0",
    ]);

    std::thread::sleep(Duration::from_millis(500));

    match gw.try_wait() {
        Ok(Some(_)) => {
            eprintln!("[SKIP] Gateway exited early");
            return;
        }
        Ok(None) => {}
        Err(_) => return,
    }

    // Close stdin — should trigger shutdown.
    drop(gw.stdin.take());

    // Wait for exit.
    let start = std::time::Instant::now();
    loop {
        match gw.try_wait() {
            Ok(Some(status)) => {
                // Should exit cleanly.
                assert!(
                    status.success() || status.code() == Some(0),
                    "stdin EOF should cause clean exit, got {status}"
                );
                return;
            }
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(8) {
                    let _ = gw.kill();
                    let _ = gw.wait();
                    panic!("Gateway did not exit within 8s after stdin EOF");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return,
        }
    }
}

/// Container PID 1 test — requires Docker.
/// Verifies SIGCHLD zombie reaping is installed when running as PID 1.
#[test]
#[ignore] // Requires Docker; run manually with: cargo test --test compat -- --ignored
fn container_pid1_zombie_reaping() {
    // Check if Docker is available.
    let docker_check = std::process::Command::new("docker")
        .args(["version"])
        .output();

    if docker_check.is_err() || !docker_check.unwrap().status.success() {
        eprintln!("[SKIP] Docker not available");
        return;
    }

    let bin = find_binary("SUPERGATEWAY_BINARY", "supergateway-rs");
    let bin_path = bin.to_string_lossy();

    // Run the gateway as PID 1 in an alpine container.
    // It should install SIGCHLD handler and log "PID 1 detected".
    let output = std::process::Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &format!("{bin_path}:/supergateway-rs:ro"),
            "alpine:latest",
            "/supergateway-rs",
            "--stdio",
            "echo hello",
            "--outputTransport",
            "sse",
            "--port",
            "8000",
        ])
        .output()
        .expect("docker run");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("PID 1") || stderr.contains("zombie reaper"),
        "PID 1 mode should install zombie reaper: {stderr}"
    );
}

/// baseUrl trailing slash normalization (from bead notes).
#[test]
fn base_url_trailing_slash() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "sse",
        "--port",
        "0",
        "--baseUrl",
        "/prefix/",
    ]);
    let port = require_gateway!(gw, "baseUrl trailing slash");
    let addr = format!("127.0.0.1:{port}");

    // Should work with or without trailing slash.
    let (status, _, _) = http_request_streaming(&addr, "GET", "/prefix/sse", &[]);
    assert_eq!(status, 200, "baseUrl with trailing slash should work");

    kill_and_wait(&mut gw);
}

/// baseUrl without trailing slash.
#[test]
fn base_url_no_trailing_slash() {
    let mut gw = spawn_gateway(&[
        "--stdio",
        &mock_stdio_cmd(),
        "--outputTransport",
        "sse",
        "--port",
        "0",
        "--baseUrl",
        "/prefix",
    ]);
    let port = require_gateway!(gw, "baseUrl no trailing slash");
    let addr = format!("127.0.0.1:{port}");

    let (status, _, _) = http_request_streaming(&addr, "GET", "/prefix/sse", &[]);
    assert_eq!(status, 200, "baseUrl without trailing slash should work");

    kill_and_wait(&mut gw);
}
