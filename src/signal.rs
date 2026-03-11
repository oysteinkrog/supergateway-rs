
//! Signal handling and graceful shutdown for supergateway.
//!
//! Installs SIGINT/SIGTERM/SIGHUP handlers for graceful shutdown.
//! Second signal forces immediate exit. PID 1 gets SIGCHLD zombie reaping.
//!
//! # Usage
//!
//! ```ignore
//! let shutdown = signal::install(&logger)?;
//! signal::spawn_stdin_eof_watcher(logger.clone());
//!
//! // Block until signal or stdin EOF
//! let sig = shutdown.wait();
//! logger.info(&format!("received {}", signal::signal_name(sig)));
//!
//! // Start drain with watchdog backstop
//! signal::spawn_shutdown_watchdog(logger.clone());
//! // ... cancel regions, drain connections ...
//! // If still alive after 5s, watchdog calls process::exit(1)
//! ```

use std::io;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;

use crate::observe::Logger;

/// Drain budget: maximum time for graceful shutdown before force exit.
#[allow(dead_code)]
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

// ─── Global state (set by signal handler, read by main thread) ──────────

/// Combined shutdown signal: 0 = not requested, positive = signal number,
/// -1 = programmatic shutdown (stdin EOF). Using a single atomic eliminates
/// the TOCTOU window between SHUTDOWN_REQUESTED and SIGNAL_NUM.
static SHUTDOWN_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// Set to true on second signal — skip drain, exit immediately.
static FORCE_EXIT: AtomicBool = AtomicBool::new(false);

/// Self-pipe read fd. Initialized once by [`install()`].
static SIGNAL_PIPE_READ: AtomicI32 = AtomicI32::new(-1);

/// Self-pipe write fd. Initialized once by [`install()`].
/// Read by signal handler via atomic load (async-signal-safe).
static SIGNAL_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

// ─── Signal handlers (async-signal-safe) ────────────────────────────────

/// Shutdown signal handler. First call CAS 0 → sig; second call sets FORCE_EXIT.
/// Single atomic eliminates TOCTOU between flag and signal number.
/// Wakes the self-pipe.
extern "C" fn shutdown_handler(sig: libc::c_int) {
    // Single CAS: 0 → sig succeeds on first signal; fails on second → FORCE_EXIT.
    if SHUTDOWN_SIGNAL
        .compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        FORCE_EXIT.store(true, Ordering::SeqCst);
    }
    // Wake any thread blocked on the pipe (async-signal-safe write)
    let fd = SIGNAL_PIPE_WRITE.load(Ordering::Relaxed);
    if fd >= 0 {
        unsafe { libc::write(fd, [1u8].as_ptr().cast(), 1) };
    }
}

/// SIGCHLD handler for PID 1: reap all available zombie children.
/// Uses `waitpid(-1, WNOHANG)` which is async-signal-safe per POSIX.
extern "C" fn sigchld_handler(_sig: libc::c_int) {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(_) => break,
            Ok(_) => continue, // reaped one zombie, try for more
        }
    }
}

// ─── ShutdownSignal ─────────────────────────────────────────────────────

/// Handle for waiting on shutdown events.
///
/// Returned by [`install()`]. Use [`wait()`](ShutdownSignal::wait) to block
/// until a shutdown signal is received or [`request_shutdown()`] is called.
#[allow(dead_code)]
pub struct ShutdownSignal {
    pipe_read: libc::c_int,
}

// SAFETY: pipe_read is a plain file descriptor (c_int), safe across threads.
unsafe impl Send for ShutdownSignal {}
unsafe impl Sync for ShutdownSignal {}

#[allow(dead_code)]
impl ShutdownSignal {
    /// Block until shutdown is requested.
    ///
    /// Returns the signal number: 2 (SIGINT), 15 (SIGTERM), 1 (SIGHUP),
    /// or 0 for programmatic shutdown (e.g., stdin EOF).
    #[allow(dead_code)]
    pub fn wait(&self) -> i32 {
        // -1 is the sentinel for programmatic shutdown; convert back to 0 for callers.
        let decode = |v: i32| if v == -1 { 0 } else { v };
        let sig = SHUTDOWN_SIGNAL.load(Ordering::SeqCst);
        if sig != 0 {
            return decode(sig);
        }
        let mut buf = [0u8; 1];
        // Blocking read — wakes when signal handler or request_shutdown() writes
        unsafe {
            libc::read(self.pipe_read, buf.as_mut_ptr().cast(), 1);
        }
        decode(SHUTDOWN_SIGNAL.load(Ordering::SeqCst))
    }
}

// ─── Public API ─────────────────────────────────────────────────────────

/// Install signal handlers for graceful shutdown.
///
/// Handles SIGINT, SIGTERM, SIGHUP. First signal triggers graceful shutdown;
/// second signal sets force-exit flag. If running as PID 1 (Docker container),
/// also installs SIGCHLD handler for zombie reaping.
///
/// Must be called once at startup, before spawning worker threads.
#[allow(dead_code)]
pub fn install(logger: &Logger) -> io::Result<ShutdownSignal> {
    // Create self-pipe
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }

    // Write end must be non-blocking so signal handler never blocks
    unsafe {
        let flags = libc::fcntl(fds[1], libc::F_GETFL);
        libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    SIGNAL_PIPE_READ.store(fds[0], Ordering::SeqCst);
    SIGNAL_PIPE_WRITE.store(fds[1], Ordering::SeqCst);

    // Install shutdown handlers for SIGINT, SIGTERM, SIGHUP
    let action = SigAction::new(
        SigHandler::Handler(shutdown_handler),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    for sig in [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP] {
        unsafe {
            sigaction(sig, &action)
                .map_err(|e| io::Error::other(format!("sigaction {sig}: {e}")))?;
        }
    }
    logger.info("signal handlers installed (SIGINT, SIGTERM, SIGHUP)");

    // PID 1: install SIGCHLD zombie reaper
    if nix::unistd::getpid().as_raw() == 1 {
        let chld_action = SigAction::new(
            SigHandler::Handler(sigchld_handler),
            SaFlags::SA_RESTART | SaFlags::SA_NOCLDSTOP,
            SigSet::empty(),
        );
        unsafe {
            sigaction(Signal::SIGCHLD, &chld_action)
                .map_err(|e| io::Error::other(format!("sigaction SIGCHLD: {e}")))?;
        }
        logger.info("PID 1 detected: SIGCHLD zombie reaper installed");
    }

    Ok(ShutdownSignal { pipe_read: fds[0] })
}

/// Check if shutdown has been requested (non-blocking).
#[allow(dead_code)]
pub fn is_shutdown_requested() -> bool {
    SHUTDOWN_SIGNAL.load(Ordering::SeqCst) != 0
}

/// Check if force exit was requested (second signal received).
#[allow(dead_code)]
pub fn is_force_exit() -> bool {
    FORCE_EXIT.load(Ordering::SeqCst)
}

/// Programmatically trigger shutdown (e.g., on stdin EOF).
///
/// First call triggers graceful shutdown. Second call sets force-exit.
/// Wakes any thread blocked in [`ShutdownSignal::wait()`].
#[allow(dead_code)]
pub fn request_shutdown() {
    // -1 = programmatic shutdown; wait() converts back to 0 for callers.
    if SHUTDOWN_SIGNAL
        .compare_exchange(0, -1, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        FORCE_EXIT.store(true, Ordering::SeqCst);
    }
    let fd = SIGNAL_PIPE_WRITE.load(Ordering::Relaxed);
    if fd >= 0 {
        unsafe { libc::write(fd, [1u8].as_ptr().cast(), 1) };
    }
}

/// Spawn a thread that triggers shutdown when stdin reaches EOF.
///
/// Used in server modes where stdin is not the JSON-RPC transport.
/// Handles the case where the parent process (e.g., IDE) closes stdin.
#[allow(dead_code)]
pub fn spawn_stdin_eof_watcher(logger: Arc<Logger>) {
    std::thread::Builder::new()
        .name("stdin-eof".into())
        .spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 64];
            loop {
                match std::io::stdin().read(&mut buf) {
                    Ok(0) | Err(_) => {
                        logger.info("stdin EOF detected, initiating shutdown");
                        request_shutdown();
                        break;
                    }
                    Ok(_) => continue,
                }
            }
        })
        .expect("spawn stdin-eof watcher");
}

/// Spawn a watchdog that force-exits after [`DRAIN_TIMEOUT`].
///
/// Call when shutdown begins. If the process hasn't exited within the
/// drain budget, the watchdog logs an error and calls `process::exit(1)`.
#[allow(dead_code)]
pub fn spawn_shutdown_watchdog(logger: Arc<Logger>) {
    std::thread::Builder::new()
        .name("shutdown-watchdog".into())
        .spawn(move || {
            std::thread::sleep(DRAIN_TIMEOUT);
            logger.error("shutdown drain timeout exceeded, forcing exit");
            std::process::exit(1);
        })
        .expect("spawn shutdown watchdog");
}

/// Human-readable name for a signal number.
#[allow(dead_code)]
pub fn signal_name(sig: i32) -> &'static str {
    match sig {
        0 => "stdin EOF",
        1 => "SIGHUP",
        2 => "SIGINT",
        15 => "SIGTERM",
        _ => "unknown",
    }
}

#[cfg(test)]
fn reset_for_testing() {
    SHUTDOWN_SIGNAL.store(0, Ordering::SeqCst);
    FORCE_EXIT.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{LogLevel, OutputTransport};
    use std::sync::Mutex;

    /// Serializes tests that share global signal state.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[allow(dead_code)]
    fn test_logger() -> Arc<Logger> {
        Arc::new(Logger::buffered(OutputTransport::Sse, LogLevel::Debug))
    }

    #[test]
    fn signal_name_coverage() {
        assert_eq!(signal_name(0), "stdin EOF");
        assert_eq!(signal_name(1), "SIGHUP");
        assert_eq!(signal_name(2), "SIGINT");
        assert_eq!(signal_name(15), "SIGTERM");
        assert_eq!(signal_name(99), "unknown");
    }

    #[test]
    fn install_logs_message() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset_for_testing();

        let logger = test_logger();
        let _shutdown = install(&logger).unwrap();
        assert!(logger.get_output().contains("signal handlers installed"));
    }

    #[test]
    fn sigterm_triggers_shutdown() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset_for_testing();

        let logger = test_logger();
        let shutdown = install(&logger).unwrap();

        assert!(!is_shutdown_requested());

        nix::sys::signal::kill(nix::unistd::getpid(), Signal::SIGTERM).unwrap();

        let sig = shutdown.wait();
        assert_eq!(sig, 15);
        assert!(is_shutdown_requested());
        assert!(!is_force_exit());
    }

    #[test]
    fn second_signal_sets_force_exit() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset_for_testing();

        let logger = test_logger();
        let _shutdown = install(&logger).unwrap();

        nix::sys::signal::kill(nix::unistd::getpid(), Signal::SIGTERM).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert!(is_shutdown_requested());

        nix::sys::signal::kill(nix::unistd::getpid(), Signal::SIGINT).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert!(is_force_exit());
    }

    #[test]
    fn programmatic_shutdown_via_request() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset_for_testing();

        let logger = test_logger();
        let shutdown = install(&logger).unwrap();

        let handle = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            request_shutdown();
        });

        let sig = shutdown.wait();
        assert_eq!(sig, 0); // programmatic = 0
        assert!(is_shutdown_requested());
        assert!(!is_force_exit());

        handle.join().unwrap();
    }

    #[test]
    fn wait_returns_immediately_if_already_requested() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset_for_testing();

        let logger = test_logger();
        let shutdown = install(&logger).unwrap();

        request_shutdown();
        let sig = shutdown.wait();
        assert_eq!(sig, 0);
    }

    #[test]
    fn double_request_shutdown_forces_exit() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset_for_testing();

        assert!(!is_shutdown_requested());
        assert!(!is_force_exit());

        request_shutdown();
        assert!(is_shutdown_requested());
        assert!(!is_force_exit());

        request_shutdown();
        assert!(is_force_exit());
    }
}
