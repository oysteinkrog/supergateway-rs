// Public API consumed by downstream gateway beads — suppress dead_code until wired up.
#![allow(dead_code)]

use std::io::{self, BufReader};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

use crate::codec::{self, CodecError, StdoutCodec, DEFAULT_PARTIAL_BUFFER, STDERR_MAX_LINE};
use crate::jsonrpc::{Parsed, RawMessage};
use crate::observe::{Logger, Metrics};

/// Channel capacity for stdout messages.
const STDOUT_CHANNEL_CAP: usize = 256;

/// Timeout for graceful kill (SIGTERM → wait → SIGKILL).
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// Stderr BufReader capacity.
const STDERR_BUF_CAP: usize = 64 * 1024;

/// Drop timeout: brief wait for SIGTERM before SIGKILL.
const DROP_TIMEOUT: Duration = Duration::from_secs(1);

/// Poll interval for kill loop.
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, thiserror::Error)]
pub enum ChildError {
    #[error("failed to spawn child: {0}")]
    Spawn(io::Error),

    #[error("broken pipe to child stdin")]
    BrokenPipe,

    #[error("codec error: {0}")]
    Codec(#[from] CodecError),

    #[error("child stdout closed")]
    StdoutClosed,

    #[error("I/O error: {0}")]
    Io(io::Error),
}

/// Manages a child MCP server process.
///
/// Spawns via `sh -c <cmd>` with `setsid()` for process group isolation (D-100).
/// Three background threads handle I/O:
/// - **stdout reader**: reads JSON-RPC messages via [`StdoutCodec`], sends to bounded channel
/// - **stderr reader**: reads lines with lossy UTF-8 (4KB truncation), logs at error level
/// - **wait**: blocks on `child.wait()`, captures exit code, updates metrics
///
/// # Region integration
///
/// ChildBridge itself is Region-agnostic. The caller spawns a task inside a Region
/// that owns the ChildBridge. When the Region is cancelled, the task is aborted,
/// ChildBridge is dropped, and the Drop impl sends SIGTERM as a safety net.
///
/// # Process group management
///
/// `setsid()` in `pre_exec` creates a new session and process group. `killpg()`
/// sends signals to the entire group, killing grandchildren (e.g., npm → node)
/// that `sh -c` may have spawned. This fixes the TypeScript bug where only `sh`
/// was killed, leaving child processes orphaned.
pub struct ChildBridge {
    pid: Pid,
    pgid: Pid,
    stdin: Mutex<Option<ChildStdin>>,
    stdout_rx: Mutex<Receiver<Result<Parsed, CodecError>>>,
    dead: Arc<AtomicBool>,
    exit_code: Arc<Mutex<Option<i32>>>,
    stdout_handle: Option<JoinHandle<()>>,
    stderr_handle: Option<JoinHandle<()>>,
    wait_handle: Option<JoinHandle<()>>,
    metrics: Arc<Metrics>,
    logger: Arc<Logger>,
}

impl ChildBridge {
    /// Spawn a child process via `sh -c <cmd>`.
    ///
    /// `cmd` is a shell command string (e.g., `"npx -y @modelcontextprotocol/server-filesystem /"`).
    /// The child runs in its own process group via `setsid`.
    pub fn spawn(
        cmd: &str,
        metrics: Arc<Metrics>,
        logger: Arc<Logger>,
    ) -> Result<Self, ChildError> {
        // SAFETY: pre_exec runs between fork and exec. setsid() is async-signal-safe
        // per POSIX. No other operations are performed in the closure.
        let mut child = unsafe {
            Command::new("sh")
                .args(["-c", cmd])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .pre_exec(|| {
                    nix::unistd::setsid().map_err(|e| {
                        io::Error::other(format!("setsid: {e}"))
                    })?;
                    Ok(())
                })
                .spawn()
                .map_err(ChildError::Spawn)?
        };

        let raw_pid = child.id() as i32;
        let pid = Pid::from_raw(raw_pid);
        let pgid = pid; // setsid makes child its own process group leader

        Metrics::inc(&metrics.total_spawns);
        Metrics::inc_and_log(&metrics.active_children, "active_children", &logger);
        logger.info(&format!("spawned child {raw_pid}: sh -c {cmd:?}"));

        let child_stdin = child.stdin.take();
        let child_stdout = child.stdout.take().expect("stdout was piped");
        let child_stderr = child.stderr.take().expect("stderr was piped");

        let dead = Arc::new(AtomicBool::new(false));
        let exit_code: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        let (stdout_tx, stdout_rx) = mpsc::sync_channel(STDOUT_CHANNEL_CAP);

        // ─── stdout reader thread ──────────────────────────────────────
        let stdout_metrics = metrics.clone();
        let stdout_logger = logger.clone();
        let stdout_dead = dead.clone();
        let stdout_handle = std::thread::Builder::new()
            .name(format!("child-{raw_pid}-stdout"))
            .spawn(move || {
                Self::stdout_reader_loop(
                    child_stdout,
                    stdout_tx,
                    stdout_metrics,
                    stdout_logger,
                    stdout_dead,
                );
            })
            .expect("spawn stdout thread");

        // ─── stderr reader thread ──────────────────────────────────────
        let stderr_logger = logger.clone();
        let stderr_handle = std::thread::Builder::new()
            .name(format!("child-{raw_pid}-stderr"))
            .spawn(move || {
                Self::stderr_reader_loop(child_stderr, stderr_logger);
            })
            .expect("spawn stderr thread");

        // ─── wait thread ───────────────────────────────────────────────
        let wait_dead = dead.clone();
        let wait_exit = exit_code.clone();
        let wait_metrics = metrics.clone();
        let wait_logger = logger.clone();
        let wait_handle = std::thread::Builder::new()
            .name(format!("child-{raw_pid}-wait"))
            .spawn(move || {
                Self::wait_loop(child, raw_pid, wait_dead, wait_exit, wait_metrics, wait_logger);
            })
            .expect("spawn wait thread");

        Ok(Self {
            pid,
            pgid,
            stdin: Mutex::new(child_stdin),
            stdout_rx: Mutex::new(stdout_rx),
            dead,
            exit_code,
            stdout_handle: Some(stdout_handle),
            stderr_handle: Some(stderr_handle),
            wait_handle: Some(wait_handle),
            metrics,
            logger,
        })
    }

    /// Receive the next message from child stdout (blocking).
    ///
    /// Returns `Err(StdoutClosed)` on EOF.
    /// Returns `Err(Codec(e))` on fatal codec errors (InvalidUtf8, BufferOverflow).
    pub fn recv_message(&self) -> Result<Parsed, ChildError> {
        let rx = self.stdout_rx.lock().unwrap();
        match rx.recv() {
            Ok(Ok(parsed)) => Ok(parsed),
            Ok(Err(codec_err)) => Err(ChildError::Codec(codec_err)),
            Err(_) => Err(ChildError::StdoutClosed),
        }
    }

    /// Try to receive a message without blocking.
    ///
    /// Returns `Ok(None)` if no message is available yet.
    pub fn try_recv_message(&self) -> Result<Option<Parsed>, ChildError> {
        let rx = self.stdout_rx.lock().unwrap();
        match rx.try_recv() {
            Ok(Ok(parsed)) => Ok(Some(parsed)),
            Ok(Err(codec_err)) => Err(ChildError::Codec(codec_err)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(ChildError::StdoutClosed),
        }
    }

    /// Write a message to child stdin.
    ///
    /// Thread-safe: the internal Mutex serializes concurrent writes.
    /// Returns `Err(BrokenPipe)` if stdin is closed or child is dead.
    pub fn write_message(&self, msg: &RawMessage) -> Result<(), ChildError> {
        if self.is_dead() {
            return Err(ChildError::BrokenPipe);
        }
        let mut guard = self.stdin.lock().unwrap();
        let stdin = guard.as_mut().ok_or(ChildError::BrokenPipe)?;
        match codec::write_message(stdin, msg) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                self.dead.store(true, Ordering::Release);
                *guard = None;
                Err(ChildError::BrokenPipe)
            }
            Err(e) => Err(ChildError::Io(e)),
        }
    }

    /// Check if child is dead (exited, killed, or fatal codec error).
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// Get exit code (None if still running or killed by signal).
    pub fn exit_code(&self) -> Option<i32> {
        *self.exit_code.lock().unwrap()
    }

    /// Get the child PID.
    pub fn pid(&self) -> Pid {
        self.pid
    }

    /// Get the process group ID (same as PID after setsid).
    pub fn pgid(&self) -> Pid {
        self.pgid
    }

    /// Kill the child process group.
    ///
    /// 1. Close stdin (signals EOF to child)
    /// 2. SIGTERM to process group
    /// 3. Wait up to 5s for child exit
    /// 4. SIGKILL if still alive (with forced_kills metric)
    pub fn kill(&self) {
        // Close stdin first to signal EOF
        {
            let mut guard = self.stdin.lock().unwrap();
            *guard = None;
        }

        // SIGTERM to process group
        if signal::killpg(self.pgid, Signal::SIGTERM).is_err() {
            // Process group already gone
            return;
        }

        // Wait up to 5s for graceful exit
        let start = Instant::now();
        while start.elapsed() < KILL_TIMEOUT {
            if self.is_dead() {
                return;
            }
            std::thread::sleep(KILL_POLL_INTERVAL);
        }

        // Still alive — escalate to SIGKILL
        if !self.is_dead() {
            self.logger.info(&format!(
                "child {} did not exit after SIGTERM, sending SIGKILL",
                self.pid
            ));
            Metrics::inc(&self.metrics.forced_kills);
            let _ = signal::killpg(self.pgid, Signal::SIGKILL);
        }
    }

    // ─── Background thread loops ───────────────────────────────────────

    fn stdout_reader_loop(
        reader: std::process::ChildStdout,
        tx: SyncSender<Result<Parsed, CodecError>>,
        metrics: Arc<Metrics>,
        logger: Arc<Logger>,
        dead: Arc<AtomicBool>,
    ) {
        let mut codec = StdoutCodec::new(reader, DEFAULT_PARTIAL_BUFFER);
        loop {
            match codec.read_message(&metrics, &logger) {
                Ok(Some(parsed)) => {
                    if tx.send(Ok(parsed)).is_err() {
                        break; // receiver dropped
                    }
                }
                Ok(None) => break, // EOF
                Err(e) => {
                    if matches!(e, CodecError::InvalidUtf8 | CodecError::BufferOverflow { .. }) {
                        dead.store(true, Ordering::Release);
                    }
                    let _ = tx.send(Err(e));
                    break; // all errors are terminal for the reader
                }
            }
        }
    }

    fn stderr_reader_loop(reader: std::process::ChildStderr, logger: Arc<Logger>) {
        let mut buf_reader = BufReader::with_capacity(STDERR_BUF_CAP, reader);
        let mut buf = Vec::new();
        loop {
            match codec::read_stderr_line(&mut buf_reader, &mut buf, STDERR_MAX_LINE) {
                Ok(Some(line)) => {
                    if !line.is_empty() {
                        logger.error(&format!("[child stderr] {line}"));
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    }

    fn wait_loop(
        mut child: Child,
        raw_pid: i32,
        dead: Arc<AtomicBool>,
        exit_code: Arc<Mutex<Option<i32>>>,
        metrics: Arc<Metrics>,
        logger: Arc<Logger>,
    ) {
        let status = child.wait();
        dead.store(true, Ordering::Release);
        let code = status.ok().and_then(|s| s.code());
        *exit_code.lock().unwrap() = code;
        Metrics::dec_and_log(&metrics.active_children, "active_children", &logger);
        logger.info(&format!("child {raw_pid} exited with code {code:?}"));
    }
}

impl Drop for ChildBridge {
    fn drop(&mut self) {
        if !self.is_dead() {
            // Safety net: SIGTERM to process group
            let _ = signal::killpg(self.pgid, Signal::SIGTERM);
            let start = Instant::now();
            while start.elapsed() < DROP_TIMEOUT {
                if self.is_dead() {
                    break;
                }
                std::thread::sleep(KILL_POLL_INTERVAL);
            }
            // Escalate to SIGKILL if still alive
            if !self.is_dead() {
                let _ = signal::killpg(self.pgid, Signal::SIGKILL);
            }
        }
        // Join all background threads to prevent leaks
        if let Some(h) = self.stdout_handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.stderr_handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.wait_handle.take() {
            let _ = h.join();
        }
    }
}

// SAFETY: ChildBridge fields are thread-safe in practice:
// - stdin_tx (Mutex<ChildStdin>): Mutex provides synchronization
// - stdout_rx (Receiver): only ever read from one thread at a time
// - All other fields are Arc, AtomicBool, Mutex, or Pid (Copy)
unsafe impl Sync for ChildBridge {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{LogLevel, OutputTransport};
    use serde_json::value::RawValue;
    use std::sync::atomic::Ordering;

    fn test_metrics() -> Arc<Metrics> {
        Metrics::new()
    }

    fn test_logger() -> Arc<Logger> {
        Arc::new(Logger::buffered(OutputTransport::Sse, LogLevel::Debug))
    }

    fn make_request(id: &str, method: &str) -> RawMessage {
        RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(RawValue::from_string(id.into()).unwrap()),
            method: Some(method.into()),
            params: None,
            result: None,
            error: None,
        }
    }

    // ─── Spawn and read a message ──────────────────────────────────────

    #[test]
    fn spawn_echo_and_read() {
        let metrics = test_metrics();
        let logger = test_logger();

        let bridge = ChildBridge::spawn(
            r#"echo '{"jsonrpc":"2.0","id":1,"result":"hello"}'"#,
            metrics.clone(),
            logger,
        )
        .unwrap();

        let msg = bridge.recv_message().unwrap();
        match msg {
            Parsed::Single(m) => {
                assert!(m.is_response());
                assert!(m.result.is_some());
            }
            _ => panic!("expected single message"),
        }

        // After echo exits, next recv should get StdoutClosed
        match bridge.recv_message() {
            Err(ChildError::StdoutClosed) => {}
            other => panic!("expected StdoutClosed, got {other:?}"),
        }
    }

    // ─── Cat roundtrip: write message, read it back ────────────────────

    #[test]
    fn spawn_cat_roundtrip() {
        let metrics = test_metrics();
        let logger = test_logger();

        let bridge = ChildBridge::spawn("cat", metrics.clone(), logger).unwrap();

        let msg = make_request("1", "ping");
        bridge.write_message(&msg).unwrap();

        let response = bridge.recv_message().unwrap();
        match response {
            Parsed::Single(m) => {
                assert!(m.is_request());
                assert_eq!(m.method_str(), Some("ping"));
            }
            _ => panic!("expected single message"),
        }

        bridge.kill();
    }

    // ─── EPIPE on dead child ───────────────────────────────────────────

    #[test]
    fn epipe_on_dead_child() {
        let metrics = test_metrics();
        let logger = test_logger();

        // 'true' exits immediately
        let bridge = ChildBridge::spawn("true", metrics.clone(), logger).unwrap();

        // Wait for child to die
        for _ in 0..40 {
            if bridge.is_dead() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let msg = make_request("1", "test");

        // First write might succeed (race) or fail with BrokenPipe
        match bridge.write_message(&msg) {
            Err(ChildError::BrokenPipe) => {} // expected
            Ok(()) => {
                // Second write must fail — child is definitely dead by now
                match bridge.write_message(&msg) {
                    Err(ChildError::BrokenPipe) => {}
                    other => panic!("expected BrokenPipe on retry, got {other:?}"),
                }
            }
            other => panic!("expected BrokenPipe or Ok, got {other:?}"),
        }
    }

    // ─── Kill terminates process group ─────────────────────────────────

    #[test]
    fn kill_terminates_process_group() {
        let metrics = test_metrics();
        let logger = test_logger();

        let bridge = ChildBridge::spawn("sleep 999", metrics.clone(), logger).unwrap();
        let pid = bridge.pid();

        assert!(!bridge.is_dead());

        bridge.kill();

        // Wait briefly for death to be detected
        for _ in 0..40 {
            if bridge.is_dead() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(bridge.is_dead());

        // Verify process is gone (signal 0 checks existence)
        let result = signal::kill(pid, None);
        assert!(result.is_err(), "process {pid} should not exist after kill");
    }

    // ─── Drop sends SIGTERM ────────────────────────────────────────────

    #[test]
    fn drop_sends_sigterm() {
        let metrics = test_metrics();
        let logger = test_logger();

        let bridge = ChildBridge::spawn("sleep 999", metrics.clone(), logger).unwrap();
        let pid = bridge.pid();

        drop(bridge);

        // Process should be dead after drop
        std::thread::sleep(Duration::from_millis(200));
        let result = signal::kill(pid, None);
        assert!(result.is_err(), "process {pid} should not exist after drop");
    }

    // ─── Rapid spawn/kill — no leaked PIDs ─────────────────────────────

    #[test]
    fn rapid_spawn_kill_no_leaks() {
        let metrics = test_metrics();
        let logger = test_logger();
        let mut pids = Vec::new();

        for _ in 0..100 {
            let bridge =
                ChildBridge::spawn("true", metrics.clone(), logger.clone()).unwrap();
            pids.push(bridge.pid());
            drop(bridge);
        }

        // Brief sleep for all processes to be fully reaped
        std::thread::sleep(Duration::from_millis(500));

        // Verify no leaked PIDs
        for pid in &pids {
            let result = signal::kill(*pid, None);
            assert!(result.is_err(), "leaked process: {pid}");
        }

        assert_eq!(metrics.active_children.load(Ordering::Relaxed), 0);
    }

    // ─── Stderr is captured and logged ─────────────────────────────────

    #[test]
    fn stderr_is_logged() {
        let metrics = test_metrics();
        let logger = test_logger();

        let bridge = ChildBridge::spawn(
            "echo >&2 'child error output'",
            metrics.clone(),
            logger.clone(),
        )
        .unwrap();

        // Wait for child to exit and stderr to be read
        for _ in 0..40 {
            if bridge.is_dead() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(bridge);

        let output = logger.get_output();
        assert!(
            output.contains("[child stderr] child error output"),
            "stderr not logged: {output}"
        );
    }

    // ─── Metrics tracked correctly ─────────────────────────────────────

    #[test]
    fn metrics_tracked() {
        let metrics = test_metrics();
        let logger = test_logger();

        let bridge = ChildBridge::spawn("true", metrics.clone(), logger).unwrap();

        assert!(metrics.total_spawns.load(Ordering::Relaxed) >= 1);

        // Wait for child to exit
        for _ in 0..40 {
            if bridge.is_dead() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(bridge);

        assert_eq!(metrics.active_children.load(Ordering::Relaxed), 0);
    }

    // ─── Exit code captured ────────────────────────────────────────────

    #[test]
    fn exit_code_captured() {
        let metrics = test_metrics();
        let logger = test_logger();

        let bridge = ChildBridge::spawn("exit 42", metrics.clone(), logger).unwrap();

        // Wait for child to exit
        for _ in 0..40 {
            if bridge.is_dead() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        assert!(bridge.is_dead());
        assert_eq!(bridge.exit_code(), Some(42));
    }

    // ─── Grandchild killed via process group ───────────────────────────

    #[test]
    fn grandchild_killed_via_pgid() {
        let metrics = test_metrics();
        let logger = test_logger();

        // sh -c spawns sh, which spawns sleep as grandchild
        let bridge = ChildBridge::spawn(
            "sleep 999 & sleep 999 & wait",
            metrics.clone(),
            logger,
        )
        .unwrap();
        let pgid = bridge.pgid();

        // Give children time to start
        std::thread::sleep(Duration::from_millis(200));

        bridge.kill();

        // Wait for kill to complete
        for _ in 0..40 {
            if bridge.is_dead() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // All processes in the group should be dead
        let result = signal::killpg(pgid, None);
        assert!(
            result.is_err(),
            "process group {pgid} should not exist after kill"
        );
    }
}
