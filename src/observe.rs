
use crate::cli::{LogLevel, OutputTransport};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

// ─── Transport enum ──────────────────────────────────────────────────────

/// Identifies which gateway transport is recording a metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Transport {
    Sse,
    Ws,
    StatefulHttp,
    StatelessHttp,
    SseToStdio,
    HttpToStdio,
}

// ─── Per-transport counters ──────────────────────────────────────────────

/// Latency histogram with bucket boundaries: 1,2,5,10,20,50,100,200,500,1000 ms.
/// Each bucket counts requests with latency <= boundary (and > previous boundary).
/// The last bucket ("overflow") counts requests > 1000 ms.
#[allow(dead_code)]
pub struct LatencyHistogram {
    /// Bucket boundaries in ms: 1,2,5,10,20,50,100,200,500,1000
    buckets: [AtomicU64; 10],
    /// Overflow: latency > 1000 ms
    overflow: AtomicU64,
    /// Running sum for p50 estimation
    total_ms: AtomicU64,
    total_count: AtomicU64,
}

const BUCKET_BOUNDS: [u64; 10] = [1, 2, 5, 10, 20, 50, 100, 200, 500, 1000];

#[allow(dead_code)]
impl LatencyHistogram {
    fn new() -> Self {
        Self {
            buckets: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            overflow: AtomicU64::new(0),
            total_ms: AtomicU64::new(0),
            total_count: AtomicU64::new(0),
        }
    }

    pub fn record(&self, ms: u64) {
        self.total_ms.fetch_add(ms, Ordering::Relaxed);
        self.total_count.fetch_add(1, Ordering::Relaxed);
        for (i, &bound) in BUCKET_BOUNDS.iter().enumerate() {
            if ms <= bound {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        self.overflow.fetch_add(1, Ordering::Relaxed);
    }

    /// Estimate p50 using the midpoint of the bucket containing the median.
    pub fn p50_ms(&self) -> u64 {
        let total = self.total_count.load(Ordering::Relaxed);
        if total == 0 {
            return 0;
        }
        let target = total.div_ceil(2);
        let mut cumulative = 0u64;
        for (i, &bound) in BUCKET_BOUNDS.iter().enumerate() {
            cumulative += self.buckets[i].load(Ordering::Relaxed);
            if cumulative >= target {
                let lower = if i == 0 { 0 } else { BUCKET_BOUNDS[i - 1] };
                return (lower + bound) / 2;
            }
        }
        // Overflow bucket midpoint
        1500
    }
}

/// Per-transport request/error counters and latency histogram.
#[allow(dead_code)]
pub struct TransportCounters {
    pub requests: AtomicU64,
    pub errors: AtomicU64,
    pub latency: LatencyHistogram,
}

#[allow(dead_code)]
impl TransportCounters {
    fn new() -> Self {
        Self {
            requests: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            latency: LatencyHistogram::new(),
        }
    }

    pub fn snapshot_json(&self) -> String {
        format!(
            "{{\"requests\":{},\"errors\":{},\"latency_p50_ms\":{}}}",
            self.requests.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
            self.latency.p50_ms(),
        )
    }
}

// ─── Free functions for instrumentation sites ────────────────────────────

/// Increment the request counter for a transport.
#[allow(dead_code)]
pub fn record_request(metrics: &Metrics, transport: Transport) {
    metrics.transport_counters(transport).requests.fetch_add(1, Ordering::Relaxed);
}

/// Increment the error counter for a transport.
#[allow(dead_code)]
pub fn record_error(metrics: &Metrics, transport: Transport) {
    metrics.transport_counters(transport).errors.fetch_add(1, Ordering::Relaxed);
}

/// Record a latency sample for a transport.
#[allow(dead_code)]
pub fn record_latency(metrics: &Metrics, transport: Transport, ms: u64) {
    metrics.transport_counters(transport).latency.record(ms);
}

// ─── Metrics struct ──────────────────────────────────────────────────────

/// Observability counters shared across all gateway tasks.
#[allow(dead_code)]
pub struct Metrics {
    pub active_sessions: AtomicU64,
    pub active_children: AtomicU64,
    pub active_clients: AtomicU64,
    pub total_requests: AtomicU64,
    pub total_spawns: AtomicU64,
    pub spawn_failures: AtomicU64,
    pub session_timeouts: AtomicU64,
    pub forced_kills: AtomicU64,
    pub client_disconnects: AtomicU64,
    pub backpressure_events: AtomicU64,
    pub queue_depth_max: AtomicU64,
    pub decode_errors: AtomicU64,
    /// Readiness flag — set to true when child started + transport bound + relay active.
    pub ready: AtomicBool,
    // Per-transport counters
    pub sse: TransportCounters,
    pub ws: TransportCounters,
    pub stateful_http: TransportCounters,
    pub stateless_http: TransportCounters,
    pub sse_to_stdio: TransportCounters,
    pub http_to_stdio: TransportCounters,
}

#[allow(dead_code)]
impl Metrics {
    #[allow(dead_code)]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            active_sessions: AtomicU64::new(0),
            active_children: AtomicU64::new(0),
            active_clients: AtomicU64::new(0),
            total_requests: AtomicU64::new(0),
            total_spawns: AtomicU64::new(0),
            spawn_failures: AtomicU64::new(0),
            session_timeouts: AtomicU64::new(0),
            forced_kills: AtomicU64::new(0),
            client_disconnects: AtomicU64::new(0),
            backpressure_events: AtomicU64::new(0),
            queue_depth_max: AtomicU64::new(0),
            decode_errors: AtomicU64::new(0),
            ready: AtomicBool::new(false),
            sse: TransportCounters::new(),
            ws: TransportCounters::new(),
            stateful_http: TransportCounters::new(),
            stateless_http: TransportCounters::new(),
            sse_to_stdio: TransportCounters::new(),
            http_to_stdio: TransportCounters::new(),
        })
    }

    /// Return a reference to the [`TransportCounters`] for the given transport.
    #[allow(dead_code)]
    pub fn transport_counters(&self, transport: Transport) -> &TransportCounters {
        match transport {
            Transport::Sse => &self.sse,
            Transport::Ws => &self.ws,
            Transport::StatefulHttp => &self.stateful_http,
            Transport::StatelessHttp => &self.stateless_http,
            Transport::SseToStdio => &self.sse_to_stdio,
            Transport::HttpToStdio => &self.http_to_stdio,
        }
    }

    /// Mark the gateway as ready (child started + transport bound + relay active).
    #[allow(dead_code)]
    pub fn set_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }

    /// Check if the gateway is ready.
    #[allow(dead_code)]
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Increment a monotonic counter.
    #[allow(dead_code)]
    pub fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment a gauge counter and log the change at info level.
    #[allow(dead_code)]
    pub fn inc_and_log(counter: &AtomicU64, name: &str, logger: &Logger) {
        let new = counter.fetch_add(1, Ordering::Relaxed) + 1;
        logger.info(&format!("{name}={new} (+1)"));
    }

    /// Decrement a gauge counter and log the change at info level.
    ///
    /// Uses `fetch_update` with saturating subtraction to prevent u64 underflow
    /// if the counter is already zero.
    #[allow(dead_code)]
    pub fn dec_and_log(counter: &AtomicU64, name: &str, logger: &Logger) {
        let prev = counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                Some(v.saturating_sub(1))
            })
            .unwrap_or(0);
        let new = prev.saturating_sub(1);
        logger.info(&format!("{name}={new} (-1)"));
    }

    /// Update max queue depth if new value is higher.
    #[allow(dead_code)]
    pub fn update_max(counter: &AtomicU64, value: u64) {
        counter.fetch_max(value, Ordering::Relaxed);
    }

    /// Snapshot all counters as JSON (without per-transport detail).
    #[allow(dead_code)]
    pub fn snapshot_json(&self) -> String {
        format!(
            concat!(
                "{{",
                "\"active_sessions\":{},",
                "\"active_children\":{},",
                "\"active_clients\":{},",
                "\"total_requests\":{},",
                "\"total_spawns\":{},",
                "\"spawn_failures\":{},",
                "\"session_timeouts\":{},",
                "\"forced_kills\":{},",
                "\"client_disconnects\":{},",
                "\"backpressure_events\":{},",
                "\"queue_depth_max\":{},",
                "\"decode_errors\":{},",
                "\"ready\":{}",
                "}}"
            ),
            self.active_sessions.load(Ordering::Relaxed),
            self.active_children.load(Ordering::Relaxed),
            self.active_clients.load(Ordering::Relaxed),
            self.total_requests.load(Ordering::Relaxed),
            self.total_spawns.load(Ordering::Relaxed),
            self.spawn_failures.load(Ordering::Relaxed),
            self.session_timeouts.load(Ordering::Relaxed),
            self.forced_kills.load(Ordering::Relaxed),
            self.client_disconnects.load(Ordering::Relaxed),
            self.backpressure_events.load(Ordering::Relaxed),
            self.queue_depth_max.load(Ordering::Relaxed),
            self.decode_errors.load(Ordering::Relaxed),
            self.ready.load(Ordering::Acquire),
        )
    }

    /// Snapshot including per-transport counters (used when `?detail=true`).
    #[allow(dead_code)]
    pub fn snapshot_json_detail(&self) -> String {
        format!(
            concat!(
                "{{",
                "\"active_sessions\":{},",
                "\"active_children\":{},",
                "\"active_clients\":{},",
                "\"total_requests\":{},",
                "\"total_spawns\":{},",
                "\"spawn_failures\":{},",
                "\"session_timeouts\":{},",
                "\"forced_kills\":{},",
                "\"client_disconnects\":{},",
                "\"backpressure_events\":{},",
                "\"queue_depth_max\":{},",
                "\"decode_errors\":{},",
                "\"ready\":{},",
                "\"transports\":{{",
                "\"sse\":{},",
                "\"ws\":{},",
                "\"stateful_http\":{},",
                "\"stateless_http\":{},",
                "\"sse_to_stdio\":{},",
                "\"http_to_stdio\":{}",
                "}}",
                "}}"
            ),
            self.active_sessions.load(Ordering::Relaxed),
            self.active_children.load(Ordering::Relaxed),
            self.active_clients.load(Ordering::Relaxed),
            self.total_requests.load(Ordering::Relaxed),
            self.total_spawns.load(Ordering::Relaxed),
            self.spawn_failures.load(Ordering::Relaxed),
            self.session_timeouts.load(Ordering::Relaxed),
            self.forced_kills.load(Ordering::Relaxed),
            self.client_disconnects.load(Ordering::Relaxed),
            self.backpressure_events.load(Ordering::Relaxed),
            self.queue_depth_max.load(Ordering::Relaxed),
            self.decode_errors.load(Ordering::Relaxed),
            self.ready.load(Ordering::Acquire),
            self.sse.snapshot_json(),
            self.ws.snapshot_json(),
            self.stateful_http.snapshot_json(),
            self.stateless_http.snapshot_json(),
            self.sse_to_stdio.snapshot_json(),
            self.http_to_stdio.snapshot_json(),
        )
    }
}

#[allow(dead_code)]
impl Default for Metrics {
    #[allow(dead_code)]
    fn default() -> Self {
        // Use new() through Arc; this is for testing convenience.
        Self {
            active_sessions: AtomicU64::new(0),
            active_children: AtomicU64::new(0),
            active_clients: AtomicU64::new(0),
            total_requests: AtomicU64::new(0),
            total_spawns: AtomicU64::new(0),
            spawn_failures: AtomicU64::new(0),
            session_timeouts: AtomicU64::new(0),
            forced_kills: AtomicU64::new(0),
            client_disconnects: AtomicU64::new(0),
            backpressure_events: AtomicU64::new(0),
            queue_depth_max: AtomicU64::new(0),
            decode_errors: AtomicU64::new(0),
            ready: AtomicBool::new(false),
            sse: TransportCounters::new(),
            ws: TransportCounters::new(),
            stateful_http: TransportCounters::new(),
            stateless_http: TransportCounters::new(),
            sse_to_stdio: TransportCounters::new(),
            http_to_stdio: TransportCounters::new(),
        }
    }
}

/// Logger that routes output based on output transport and log level.
///
/// When outputTransport is `stdio`, ALL log output goes to stderr
/// (stdout is the JSON-RPC transport). Otherwise, info goes to stdout
/// and error goes to stderr.
#[allow(dead_code)]
pub struct Logger {
    level: LogLevel,
    /// When true, all output goes to stderr.
    all_stderr: bool,
    /// Optional buffer for testing — captures output instead of writing to stderr/stdout.
    #[cfg(test)]
    pub(crate) buffer: Option<std::sync::Mutex<Vec<u8>>>,
}

impl Logger {
    /// Create a logger for the given output transport and log level.
    #[allow(dead_code)]
    pub fn new(output_transport: OutputTransport, level: LogLevel) -> Self {
        Self {
            level,
            all_stderr: output_transport == OutputTransport::Stdio,
            #[cfg(test)]
            buffer: None,
        }
    }

    /// Create a logger that captures output to a buffer (for testing).
    #[cfg(test)]
    pub fn buffered(output_transport: OutputTransport, level: LogLevel) -> Self {
        Self {
            level,
            all_stderr: output_transport == OutputTransport::Stdio,
            buffer: Some(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Get captured buffer contents (for testing).
    #[cfg(test)]
    pub fn get_output(&self) -> String {
        match &self.buffer {
            Some(buf) => String::from_utf8_lossy(&buf.lock().unwrap()).to_string(),
            None => String::new(),
        }
    }

    #[allow(dead_code)]
    pub fn level(&self) -> LogLevel {
        self.level
    }

    #[allow(dead_code)]
    pub fn is_debug(&self) -> bool {
        self.level == LogLevel::Debug
    }

    /// Log at info level. Suppressed when level is None.
    #[allow(dead_code)]
    pub fn info(&self, msg: &str) {
        if self.level == LogLevel::None {
            return;
        }
        self.write_info(&format!("[supergateway] {msg}"));
    }

    /// Log at debug level. Only emitted when level is Debug.
    #[allow(dead_code)]
    pub fn debug(&self, msg: &str) {
        if self.level != LogLevel::Debug {
            return;
        }
        self.write_info(&format!("[supergateway] {msg}"));
    }

    /// Log at error level. Suppressed when level is None.
    #[allow(dead_code)]
    pub fn error(&self, msg: &str) {
        if self.level == LogLevel::None {
            return;
        }
        self.write_err(&format!("[supergateway] {msg}"));
    }

    /// Log the startup banner.
    #[allow(dead_code)]
    pub fn startup(&self, version: &str, input_desc: &str, output_desc: &str, port: u16) {
        self.info(&format!(
            "supergateway v{version} | {input_desc} → {output_desc} | port {port}"
        ));
    }

    #[allow(dead_code)]
    fn write_info(&self, line: &str) {
        #[cfg(test)]
        if let Some(buf) = &self.buffer {
            let mut b = buf.lock().unwrap();
            let _ = writeln!(b, "{line}");
            return;
        }

        if self.all_stderr {
            let _ = writeln!(std::io::stderr(), "{line}");
        } else {
            let _ = writeln!(std::io::stdout(), "{line}");
        }
    }

    #[allow(dead_code)]
    fn write_err(&self, line: &str) {
        #[cfg(test)]
        if let Some(buf) = &self.buffer {
            let mut b = buf.lock().unwrap();
            let _ = writeln!(b, "{line}");
            return;
        }

        let _ = writeln!(std::io::stderr(), "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_new() {
        let m = Metrics::new();
        assert_eq!(m.active_sessions.load(Ordering::Relaxed), 0);
        assert!(!m.is_ready());
    }

    #[test]
    fn test_metrics_inc_dec() {
        let m = Metrics::new();
        let logger = Logger::buffered(OutputTransport::Sse, LogLevel::Info);

        Metrics::inc_and_log(&m.active_sessions, "active_sessions", &logger);
        assert_eq!(m.active_sessions.load(Ordering::Relaxed), 1);

        Metrics::inc_and_log(&m.active_sessions, "active_sessions", &logger);
        assert_eq!(m.active_sessions.load(Ordering::Relaxed), 2);

        Metrics::dec_and_log(&m.active_sessions, "active_sessions", &logger);
        assert_eq!(m.active_sessions.load(Ordering::Relaxed), 1);

        let output = logger.get_output();
        assert!(output.contains("active_sessions=1 (+1)"));
        assert!(output.contains("active_sessions=2 (+1)"));
        assert!(output.contains("active_sessions=1 (-1)"));
    }

    #[test]
    fn test_metrics_monotonic_inc() {
        let m = Metrics::new();
        Metrics::inc(&m.total_requests);
        Metrics::inc(&m.total_requests);
        Metrics::inc(&m.total_requests);
        assert_eq!(m.total_requests.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn test_metrics_update_max() {
        let m = Metrics::new();
        Metrics::update_max(&m.queue_depth_max, 10);
        assert_eq!(m.queue_depth_max.load(Ordering::Relaxed), 10);
        Metrics::update_max(&m.queue_depth_max, 5);
        assert_eq!(m.queue_depth_max.load(Ordering::Relaxed), 10);
        Metrics::update_max(&m.queue_depth_max, 20);
        assert_eq!(m.queue_depth_max.load(Ordering::Relaxed), 20);
    }

    #[test]
    fn test_metrics_set_ready() {
        let m = Metrics::new();
        assert!(!m.is_ready());
        m.set_ready();
        assert!(m.is_ready());
    }

    #[test]
    fn test_metrics_snapshot_json() {
        let m = Metrics::new();
        m.active_sessions.store(2, Ordering::Relaxed);
        m.total_requests.store(42, Ordering::Relaxed);
        m.set_ready();

        let json = m.snapshot_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["active_sessions"], 2);
        assert_eq!(parsed["total_requests"], 42);
        assert_eq!(parsed["ready"], true);
        assert_eq!(parsed["decode_errors"], 0);
    }

    #[test]
    fn test_logger_prefix() {
        let logger = Logger::buffered(OutputTransport::Sse, LogLevel::Info);
        logger.info("hello");
        let output = logger.get_output();
        assert!(output.starts_with("[supergateway] hello"));
    }

    #[test]
    fn test_logger_debug_suppressed_at_info() {
        let logger = Logger::buffered(OutputTransport::Sse, LogLevel::Info);
        logger.debug("secret debug message");
        let output = logger.get_output();
        assert!(output.is_empty(), "debug should be suppressed at info level");
    }

    #[test]
    fn test_logger_debug_emitted_at_debug() {
        let logger = Logger::buffered(OutputTransport::Sse, LogLevel::Debug);
        logger.debug("visible debug message");
        let output = logger.get_output();
        assert!(output.contains("visible debug message"));
    }

    #[test]
    fn test_logger_none_suppresses_all() {
        let logger = Logger::buffered(OutputTransport::Sse, LogLevel::None);
        logger.info("should not appear");
        logger.error("should not appear");
        logger.debug("should not appear");
        let output = logger.get_output();
        assert!(output.is_empty(), "none level should suppress all output");
    }

    #[test]
    fn test_logger_startup_message() {
        let logger = Logger::buffered(OutputTransport::Sse, LogLevel::Info);
        logger.startup("0.1.0", "stdio", "sse", 8000);
        let output = logger.get_output();
        assert!(output.contains("[supergateway] supergateway v0.1.0"));
        assert!(output.contains("stdio → sse"));
        assert!(output.contains("port 8000"));
    }

    #[test]
    fn test_logger_stderr_routing_for_stdio_output() {
        let logger = Logger::new(OutputTransport::Stdio, LogLevel::Info);
        assert!(logger.all_stderr);
    }

    #[test]
    fn test_logger_mixed_routing_for_non_stdio_output() {
        let logger = Logger::new(OutputTransport::Sse, LogLevel::Info);
        assert!(!logger.all_stderr);

        let logger2 = Logger::new(OutputTransport::Ws, LogLevel::Info);
        assert!(!logger2.all_stderr);

        let logger3 = Logger::new(OutputTransport::StreamableHttp, LogLevel::Info);
        assert!(!logger3.all_stderr);
    }

    #[test]
    fn test_no_header_values_at_info_level() {
        // Verify that info-level logging of headers only shows count, not values.
        let logger = Logger::buffered(OutputTransport::Sse, LogLevel::Info);
        // Simulate what a gateway would do: log header count at info, values at debug.
        let headers = vec![("Authorization", "Bearer secret-token")];
        logger.info(&format!("custom headers configured: {}", headers.len()));
        logger.debug(&format!("headers: {:?}", headers));
        let output = logger.get_output();
        assert!(output.contains("custom headers configured: 1"));
        assert!(
            !output.contains("secret-token"),
            "header values must not appear at info level"
        );
    }

    #[test]
    fn test_inc_and_log_output_prefixed() {
        let m = Metrics::new();
        let logger = Logger::buffered(OutputTransport::Sse, LogLevel::Info);
        Metrics::inc_and_log(&m.active_clients, "active_clients", &logger);
        let output = logger.get_output();
        assert!(output.contains("[supergateway] active_clients=1 (+1)"));
    }

    #[test]
    fn test_per_transport_record_request_all_variants() {
        use super::{record_error, record_latency, record_request, Transport};
        use std::sync::atomic::Ordering;

        let m = Metrics::new();

        let variants = [
            Transport::Sse,
            Transport::Ws,
            Transport::StatefulHttp,
            Transport::StatelessHttp,
            Transport::SseToStdio,
            Transport::HttpToStdio,
        ];

        for &t in &variants {
            record_request(&m, t);
            record_latency(&m, t, 5);
        }

        // Each transport should have exactly 1 request, 0 errors, p50=5ms
        for &t in &variants {
            let counters = m.transport_counters(t);
            assert_eq!(
                counters.requests.load(Ordering::Relaxed),
                1,
                "transport {:?} requests should be 1",
                t
            );
            assert_eq!(
                counters.errors.load(Ordering::Relaxed),
                0,
                "transport {:?} errors should be 0",
                t
            );
            assert_eq!(
                counters.latency.p50_ms(),
                3, // bucket [2,5] midpoint → (2+5)/2 = 3
                "transport {:?} p50 should reflect latency=5ms bucket",
                t
            );
        }

        // Test error recording
        record_error(&m, Transport::Sse);
        record_error(&m, Transport::Ws);

        assert_eq!(m.sse.errors.load(Ordering::Relaxed), 1);
        assert_eq!(m.ws.errors.load(Ordering::Relaxed), 1);
        assert_eq!(m.stateful_http.errors.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_snapshot_json_detail_contains_all_transports() {
        let m = Metrics::new();
        m.sse.requests.store(7, Ordering::Relaxed);
        m.ws.errors.store(2, Ordering::Relaxed);
        m.stateful_http.requests.store(3, Ordering::Relaxed);

        let json = m.snapshot_json_detail();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        let transports = parsed.get("transports").expect("missing transports field");
        for key in &["sse", "ws", "stateful_http", "stateless_http", "sse_to_stdio", "http_to_stdio"] {
            let t = transports.get(key).unwrap_or_else(|| panic!("missing transport: {key}"));
            assert!(t.get("requests").is_some(), "missing requests in {key}");
            assert!(t.get("errors").is_some(), "missing errors in {key}");
            assert!(t.get("latency_p50_ms").is_some(), "missing latency_p50_ms in {key}");
        }

        assert_eq!(transports["sse"]["requests"], 7);
        assert_eq!(transports["ws"]["errors"], 2);
        assert_eq!(transports["stateful_http"]["requests"], 3);
    }
}
