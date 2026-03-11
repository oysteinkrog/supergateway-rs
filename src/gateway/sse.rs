
//! stdio → SSE gateway.
//!
//! Single shared child process, broadcast to all connected SSE clients (B-001).
//!
//! # Endpoints
//!
//! - `GET <ssePath>`: SSE event stream with per-client session tracking
//! - `POST <messagePath>?sessionId=<uuid>`: JSON-RPC message from client to child
//! - Health endpoints as configured by `--healthEndpoint`
//!
//! # Architecture
//!
//! One [`ChildBridge`] is shared across all connected SSE clients. A relay thread
//! reads child stdout and broadcasts every message to all clients via bounded
//! per-client channels (capacity 256). Messages arriving before the first client
//! connects are buffered (up to 256) and drained to the first client.
//!
//! POST handlers validate the `sessionId` query parameter, read the raw request
//! body (4MB limit), and write to child stdin via the ChildBridge's internal mutex
//! (D-012: serialized writes prevent interleaved JSON lines).
//!
//! When the child process exits, the relay thread returns its exit code.
//! The caller (main.rs) must exit the process with that code to ensure
//! pm2/systemd restarts work correctly.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::child::ChildBridge;
use crate::cli::{Header, LogLevel};
use crate::cors::{self, CorsHandler, CorsResult};
use crate::error::GatewayError;
use crate::health;
use crate::jsonrpc::Parsed;
use crate::observe::{Logger, Metrics};
use crate::session::SessionId;

// ─── Constants ──────────────────────────────────────────────────────

/// Per-client channel capacity (D-017).
#[allow(dead_code)]
const CLIENT_CHANNEL_CAP: usize = 256;

/// Maximum early buffer size (messages before first client).
#[allow(dead_code)]
const EARLY_BUFFER_CAP: usize = 256;

/// Backpressure timeout: disconnect client if channel full for this long (D-017).
#[allow(dead_code)]
const BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(30);

/// SSE keepalive interval.
#[allow(dead_code)]
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Maximum POST body size (4MB — TS SDK raw-body default for SSE mode).
#[allow(dead_code)]
const MAX_BODY_SIZE: usize = 4 * 1024 * 1024;

// ─── SSE Event ──────────────────────────────────────────────────────

/// An event to send over the SSE stream.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum SseEvent {
    /// First event: `event: endpoint\ndata: <url>\n\n`
    Endpoint(String),
    /// Message event: `event: message\ndata: <json>\n\n`
    Message(String),
    /// Keepalive comment: `: keepalive\n\n`
    Keepalive,
}

#[allow(dead_code)]
impl SseEvent {
    /// Serialize to SSE wire format.
    #[allow(dead_code)]
    pub fn serialize(&self) -> String {
        match self {
            Self::Endpoint(url) => format!("event: endpoint\ndata: {url}\n\n"),
            Self::Message(data) => format!("event: message\ndata: {data}\n\n"),
            Self::Keepalive => ": keepalive\n\n".to_string(),
        }
    }
}

// ─── Internal client state ──────────────────────────────────────────

#[allow(dead_code)]
struct ClientState {
    tx: SyncSender<SseEvent>,
    /// When backpressure started (channel full). None = healthy.
    backpressure_since: Option<Instant>,
}

/// Shared interior state behind Mutex.
#[allow(dead_code)]
struct Inner {
    clients: HashMap<SessionId, ClientState>,
    /// Messages buffered before first client connects. `None` after first client.
    early_buffer: Option<Vec<String>>,
}

// ─── GatewayResponse ────────────────────────────────────────────────

/// Framework-agnostic HTTP response.
#[allow(dead_code)]
pub struct GatewayResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

#[allow(dead_code)]
impl GatewayResponse {
    #[allow(dead_code)]
    fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    #[allow(dead_code)]
    fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    #[allow(dead_code)]
    fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers.extend(headers);
        self
    }
}

// ─── SseConnection ──────────────────────────────────────────────────

/// Returned by [`SseGateway::handle_sse_connect`] for the caller to stream.
#[allow(dead_code)]
pub struct SseConnection {
    /// Session ID for this client.
    pub session_id: SessionId,
    /// Response headers for the SSE stream.
    pub headers: Vec<(String, String)>,
    /// Receiver for SSE events. The caller reads and writes to HTTP response.
    pub receiver: Receiver<SseEvent>,
}

// ─── SseGateway ─────────────────────────────────────────────────────

/// stdio → SSE gateway.
///
/// All state is behind `Arc` — cloning is cheap.
#[allow(dead_code)]
pub struct SseGateway {
    child: Arc<ChildBridge>,
    inner: Arc<Mutex<Inner>>,
    base_url: String,
    message_path: String,
    cors: Arc<CorsHandler>,
    custom_headers: Arc<Vec<Header>>,
    metrics: Arc<Metrics>,
    logger: Arc<Logger>,
    log_level: LogLevel,
    health_endpoints: Arc<Vec<String>>,
}

#[allow(dead_code)]
impl Clone for SseGateway {
    #[allow(dead_code)]
    fn clone(&self) -> Self {
        Self {
            child: Arc::clone(&self.child),
            inner: Arc::clone(&self.inner),
            base_url: self.base_url.clone(),
            message_path: self.message_path.clone(),
            cors: Arc::clone(&self.cors),
            custom_headers: Arc::clone(&self.custom_headers),
            metrics: Arc::clone(&self.metrics),
            logger: Arc::clone(&self.logger),
            log_level: self.log_level,
            health_endpoints: Arc::clone(&self.health_endpoints),
        }
    }
}

#[allow(dead_code)]
impl SseGateway {
    /// Create a new SSE gateway.
    ///
    /// The child must already be spawned. Call [`run_relay`](Self::run_relay)
    /// or [`spawn_relay`](Self::spawn_relay) to start the broadcast loop.
    #[allow(dead_code)]
    pub fn new(
        child: Arc<ChildBridge>,
        base_url: String,
        message_path: String,
        cors: CorsHandler,
        custom_headers: Vec<Header>,
        metrics: Arc<Metrics>,
        logger: Arc<Logger>,
        log_level: LogLevel,
        health_endpoints: Vec<String>,
    ) -> Self {
        Self {
            child,
            inner: Arc::new(Mutex::new(Inner {
                clients: HashMap::new(),
                early_buffer: Some(Vec::new()),
            })),
            base_url,
            message_path,
            cors: Arc::new(cors),
            custom_headers: Arc::new(custom_headers),
            metrics,
            logger,
            log_level,
            health_endpoints: Arc::new(health_endpoints),
        }
    }

    // ─── Relay ──────────────────────────────────────────────────────

    /// Run the relay loop (blocking). Returns the child's exit code.
    ///
    /// Reads child stdout and broadcasts every message to all connected SSE
    /// clients. Messages before the first client are buffered (up to 256).
    ///
    /// Returns when the child exits. The caller MUST exit the process with
    /// the returned code (or 1 if None) so pm2/systemd can restart.
    #[allow(dead_code)]
    pub fn run_relay(&self) -> Option<i32> {
        loop {
            match self.child.recv_message() {
                Ok(parsed) => {
                    let messages = match parsed {
                        Parsed::Single(msg) => vec![msg],
                        Parsed::Batch(msgs) => msgs,
                    };

                    for msg in messages {
                        let json = match serde_json::to_string(&msg) {
                            Ok(j) => j,
                            Err(e) => {
                                self.logger
                                    .error(&format!("failed to serialize child message: {e}"));
                                continue;
                            }
                        };

                        let mut state = self.inner.lock().unwrap();

                        if state.early_buffer.is_some() && state.clients.is_empty() {
                            // No clients yet — buffer the message
                            let buf = state.early_buffer.as_mut().unwrap();
                            if buf.len() < EARLY_BUFFER_CAP {
                                buf.push(json);
                            } else {
                                self.logger.debug("early buffer full, dropping message");
                            }
                            continue;
                        }

                        // Broadcast to all clients
                        Self::broadcast_locked(&mut state, &json, &self.metrics, &self.logger);
                    }
                }
                Err(e) => {
                    self.logger.info(&format!("child stdout closed: {e}"));
                    break;
                }
            }
        }

        // Ensure child process group is cleaned up (D-011, D-016)
        self.child.kill();
        self.child.exit_code()
    }

    /// Convenience: spawn the relay in a background thread.
    ///
    /// When the child exits the thread returns the exit code. The caller
    /// should join and call `process::exit(code.unwrap_or(1))`.
    #[allow(dead_code)]
    pub fn spawn_relay(&self) -> std::thread::JoinHandle<Option<i32>> {
        let gw = self.clone();
        std::thread::Builder::new()
            .name("sse-relay".into())
            .spawn(move || gw.run_relay())
            .expect("spawn sse-relay thread")
    }

    /// Broadcast a JSON message to all connected clients.
    ///
    /// Handles backpressure: if a client's channel is full for >30s,
    /// the client is disconnected (D-017).
    #[allow(dead_code)]
    fn broadcast_locked(
        state: &mut Inner,
        json: &str,
        metrics: &Metrics,
        logger: &Logger,
    ) {
        let event = SseEvent::Message(json.to_string());
        let mut to_remove = Vec::new();

        for (id, client) in state.clients.iter_mut() {
            match client.tx.try_send(event.clone()) {
                Ok(()) => {
                    if client.backpressure_since.take().is_some() {
                        logger.debug(&format!("backpressure resolved for session {id}"));
                    }
                }
                Err(TrySendError::Full(_)) => {
                    let now = Instant::now();
                    match client.backpressure_since {
                        Some(since) if now.duration_since(since) >= BACKPRESSURE_TIMEOUT => {
                            logger.info(&format!(
                                "disconnecting session {id}: backpressure timeout (>{}s)",
                                BACKPRESSURE_TIMEOUT.as_secs()
                            ));
                            Metrics::inc(&metrics.backpressure_events);
                            to_remove.push(id.clone());
                        }
                        Some(_) => { /* within timeout window, skip message */ }
                        None => {
                            client.backpressure_since = Some(now);
                            logger.info(&format!("backpressure started for session {id}"));
                        }
                    }
                }
                Err(TrySendError::Disconnected(_)) => {
                    to_remove.push(id.clone());
                }
            }
        }

        for id in &to_remove {
            state.clients.remove(id);
            Metrics::inc(&metrics.client_disconnects);
            Metrics::dec_and_log(&metrics.active_clients, "active_clients", logger);
        }
    }

    // ─── GET /sse handler ───────────────────────────────────────────

    /// Handle an SSE connection request (GET /sse).
    ///
    /// Returns an [`SseConnection`] with the session ID, response headers,
    /// and a receiver for SSE events. The caller must:
    /// 1. Write the response headers
    /// 2. Loop on the receiver, writing events via [`SseEvent::serialize`]
    /// 3. Send [`SseEvent::Keepalive`] every [`KEEPALIVE_INTERVAL`]
    /// 4. Call [`disconnect_client`](Self::disconnect_client) on connection close
    #[allow(dead_code)]
    pub fn handle_sse_connect(&self, origin: Option<&str>) -> SseConnection {
        let cors_headers = match self.cors.process("GET", origin, None) {
            CorsResult::Disabled => vec![],
            CorsResult::ResponseHeaders(h) => h,
            CorsResult::Preflight(_) => vec![], // unreachable for GET
        };

        let session_id = SessionId::new();
        let (tx, rx) = mpsc::sync_channel(CLIENT_CHANNEL_CAP);

        // Build endpoint URL: baseUrl + messagePath + ?sessionId=<id>
        // NO slash normalization (per spec)
        let endpoint_url = format!(
            "{}{}?sessionId={}",
            self.base_url,
            self.message_path,
            session_id.as_str()
        );

        // Send endpoint event first (channel is empty, always succeeds)
        let _ = tx.try_send(SseEvent::Endpoint(endpoint_url));

        // Register client and drain early buffer
        {
            let mut state = self.inner.lock().unwrap();

            if let Some(buffer) = state.early_buffer.take() {
                for json in buffer {
                    let _ = tx.try_send(SseEvent::Message(json));
                }
            }

            state.clients.insert(
                session_id.clone(),
                ClientState {
                    tx,
                    backpressure_since: None,
                },
            );
        }

        Metrics::inc_and_log(&self.metrics.active_clients, "active_clients", &self.logger);
        self.logger
            .info(&format!("SSE client connected: session={session_id}"));

        // Build SSE response headers
        let mut headers = vec![
            ("Content-Type".to_string(), "text/event-stream".to_string()),
            ("Cache-Control".to_string(), "no-cache".to_string()),
            ("Connection".to_string(), "keep-alive".to_string()),
            (
                "X-Accel-Buffering".to_string(),
                "no".to_string(),
            ),
        ];
        headers.extend(cors_headers);
        cors::apply_custom_headers(&mut headers, &self.custom_headers);

        SseConnection {
            session_id,
            headers,
            receiver: rx,
        }
    }

    /// Remove a disconnected SSE client from the client map.
    ///
    /// Must be called when the SSE connection closes.
    #[allow(dead_code)]
    pub fn disconnect_client(&self, id: &SessionId) {
        let removed = {
            let mut state = self.inner.lock().unwrap();
            state.clients.remove(id).is_some()
        };

        if removed {
            Metrics::inc(&self.metrics.client_disconnects);
            Metrics::dec_and_log(&self.metrics.active_clients, "active_clients", &self.logger);
            self.logger
                .info(&format!("SSE client disconnected: session={id}"));
        }
    }

    // ─── POST /message handler ──────────────────────────────────────

    /// Handle a POST to the message endpoint.
    ///
    /// `session_id_param`: value of `?sessionId=` query parameter (`None` if absent).
    /// `body`: raw request body bytes (NOT parsed by framework body parser).
    /// `origin`: Origin header for CORS.
    #[allow(dead_code)]
    pub fn handle_message(
        &self,
        session_id_param: Option<&str>,
        body: &[u8],
        origin: Option<&str>,
    ) -> GatewayResponse {
        Metrics::inc(&self.metrics.total_requests);

        let cors_headers = match self.cors.process("POST", origin, None) {
            CorsResult::Disabled => vec![],
            CorsResult::ResponseHeaders(h) => h,
            CorsResult::Preflight(_) => vec![], // unreachable for POST
        };

        // Validate sessionId parameter
        let session_id_str = match session_id_param {
            Some(s) if !s.is_empty() => s,
            _ => {
                return self.plain_response(400, "Missing sessionId parameter", cors_headers);
            }
        };

        // Validate session exists
        {
            let state = self.inner.lock().unwrap();
            if !state
                .clients
                .contains_key(&SessionId::from_value(session_id_str))
            {
                let msg = format!("No active SSE connection for session {session_id_str}");
                return self.plain_response(503, &msg, cors_headers);
            }
        }

        // Validate body size
        if body.len() > MAX_BODY_SIZE {
            return self.error_response(GatewayError::payload_too_large(), cors_headers);
        }

        // Validate non-empty body
        if body.is_empty() {
            return self.plain_response(400, "Empty body", cors_headers);
        }

        // Parse as UTF-8 and validate JSON
        let body_str = match std::str::from_utf8(body) {
            Ok(s) => s,
            Err(_) => {
                return self.plain_response(400, "Invalid UTF-8", cors_headers);
            }
        };

        let parsed = match crate::jsonrpc::parse_line(body_str) {
            Ok(p) => p,
            Err(_) => {
                return self.plain_response(400, "Bad Request", cors_headers);
            }
        };

        // Write to child stdin (mutex-protected via ChildBridge — D-012)
        let messages = match parsed {
            Parsed::Single(msg) => vec![msg],
            Parsed::Batch(msgs) => msgs,
        };

        for msg in &messages {
            if let Err(e) = self.child.write_message(msg) {
                self.logger.error(&format!("failed to write to child: {e}"));
                return self.error_response(GatewayError::child_dead(), cors_headers);
            }
        }

        // 202 Accepted (B-002)
        let mut resp = GatewayResponse::new(202, "Accepted")
            .with_header("Content-Type", "text/plain");
        resp = resp.with_headers(cors_headers);
        cors::apply_custom_headers(&mut resp.headers, &self.custom_headers);
        resp
    }

    // ─── OPTIONS handler ────────────────────────────────────────────

    /// Handle a CORS preflight request. Returns `None` if CORS is disabled.
    #[allow(dead_code)]
    pub fn handle_options(
        &self,
        origin: Option<&str>,
        request_headers: Option<&str>,
    ) -> Option<GatewayResponse> {
        match self.cors.process("OPTIONS", origin, request_headers) {
            CorsResult::Preflight(headers) => {
                let mut resp = GatewayResponse::new(204, "").with_headers(headers);
                cors::apply_custom_headers(&mut resp.headers, &self.custom_headers);
                Some(resp)
            }
            _ => None,
        }
    }

    // ─── Health handler ─────────────────────────────────────────────

    /// Handle a health endpoint request. Returns `None` if path is not a health endpoint.
    #[allow(dead_code)]
    pub fn handle_health(
        &self,
        path: &str,
        detail: bool,
        origin: Option<&str>,
    ) -> Option<GatewayResponse> {
        if !self.health_endpoints.iter().any(|ep| ep == path) {
            return None;
        }

        let health_resp = health::check_health(&self.metrics, detail, self.log_level);
        let cors_headers = match self.cors.process("GET", origin, None) {
            CorsResult::Disabled => vec![],
            CorsResult::ResponseHeaders(h) => h,
            CorsResult::Preflight(_) => vec![],
        };

        let mut resp = GatewayResponse::new(health_resp.status_code(), health_resp.body())
            .with_header("Content-Type", health_resp.content_type());
        resp = resp.with_headers(cors_headers);
        cors::apply_custom_headers(&mut resp.headers, &self.custom_headers);
        Some(resp)
    }

    // ─── Helpers ────────────────────────────────────────────────────

    #[allow(dead_code)]
    fn plain_response(
        &self,
        status: u16,
        body: &str,
        cors_headers: Vec<(String, String)>,
    ) -> GatewayResponse {
        let mut resp =
            GatewayResponse::new(status, body).with_header("Content-Type", "text/plain");
        resp = resp.with_headers(cors_headers);
        cors::apply_custom_headers(&mut resp.headers, &self.custom_headers);
        resp
    }

    #[allow(dead_code)]
    fn error_response(
        &self,
        err: GatewayError,
        cors_headers: Vec<(String, String)>,
    ) -> GatewayResponse {
        let mut resp = GatewayResponse::new(err.status_code(), err.body())
            .with_header("Content-Type", err.content_type());
        resp = resp.with_headers(cors_headers);
        cors::apply_custom_headers(&mut resp.headers, &self.custom_headers);
        resp
    }

    /// Number of connected SSE clients.
    #[allow(dead_code)]
    pub fn client_count(&self) -> usize {
        self.inner.lock().unwrap().clients.len()
    }

    /// Check if the child process is dead.
    #[allow(dead_code)]
    pub fn is_child_dead(&self) -> bool {
        self.child.is_dead()
    }
}

// ─── Entry point ────────────────────────────────────────────────────────

/// Run the stdio → SSE gateway.
#[allow(dead_code)]
pub async fn run(config: crate::cli::Config) -> anyhow::Result<()> {
    let logger = Arc::new(Logger::new(config.output_transport, config.log_level));
    let metrics = Metrics::new();

    logger.startup(
        env!("CARGO_PKG_VERSION"),
        &config.input_value,
        &config.output_transport.to_string(),
        config.port,
    );

    let _shutdown = crate::signal::install(&logger)?;

    let child = Arc::new(crate::child::ChildBridge::spawn(
        &config.input_value,
        metrics.clone(),
        logger.clone(),
    )?);

    let cors_handler = CorsHandler::new(config.cors, false);
    let _gw = SseGateway::new(
        child,
        config.base_url,
        config.message_path,
        cors_handler,
        config.headers,
        metrics,
        logger,
        config.log_level,
        config.health_endpoints,
    );

    // TODO: Wire up TCP listener + HTTP serving (upcoming bead)
    anyhow::bail!("stdio->SSE serving not yet implemented")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{CorsConfig, LogLevel, OutputTransport};
    use std::sync::atomic::Ordering;

    #[allow(dead_code)]
    fn test_metrics() -> Arc<Metrics> {
        Metrics::new()
    }

    #[allow(dead_code)]
    fn test_logger() -> Arc<Logger> {
        Arc::new(Logger::buffered(OutputTransport::Sse, LogLevel::Debug))
    }

    /// Create a gateway with a `cat` child (echoes stdin to stdout).
    #[allow(dead_code)]
    fn make_gateway_with_child(cmd: &str) -> (SseGateway, Arc<ChildBridge>) {
        let metrics = test_metrics();
        let logger = test_logger();
        let child =
            Arc::new(ChildBridge::spawn(cmd, metrics.clone(), logger.clone()).unwrap());
        let gw = SseGateway::new(
            Arc::clone(&child),
            String::new(),
            "/message".to_string(),
            CorsHandler::new(CorsConfig::Disabled, false),
            vec![],
            metrics,
            logger,
            LogLevel::Debug,
            vec!["/health".to_string()],
        );
        (gw, child)
    }

    #[allow(dead_code)]
    fn make_gateway(cmd: &str) -> SseGateway {
        make_gateway_with_child(cmd).0
    }

    // ─── SseEvent serialization ─────────────────────────────────────

    #[test]
    fn event_serialize_endpoint() {
        let e = SseEvent::Endpoint("http://localhost:8000/message?sessionId=abc".into());
        assert_eq!(
            e.serialize(),
            "event: endpoint\ndata: http://localhost:8000/message?sessionId=abc\n\n"
        );
    }

    #[test]
    fn event_serialize_message() {
        let e = SseEvent::Message(r#"{"jsonrpc":"2.0","id":1,"result":"ok"}"#.into());
        assert_eq!(
            e.serialize(),
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"ok\"}\n\n"
        );
    }

    #[test]
    fn event_serialize_keepalive() {
        assert_eq!(SseEvent::Keepalive.serialize(), ": keepalive\n\n");
    }

    // ─── SSE connect ────────────────────────────────────────────────

    #[test]
    fn connect_returns_endpoint_event() {
        let gw = make_gateway("cat");
        let conn = gw.handle_sse_connect(None);

        // First event should be endpoint
        let first = conn.receiver.try_recv().unwrap();
        match first {
            SseEvent::Endpoint(url) => {
                assert!(url.starts_with("/message?sessionId="));
                assert!(url.contains(conn.session_id.as_str()));
            }
            other => panic!("expected Endpoint, got {other:?}"),
        }

        assert_eq!(gw.client_count(), 1);
    }

    #[test]
    fn connect_with_base_url() {
        let metrics = test_metrics();
        let logger = test_logger();
        let child =
            Arc::new(ChildBridge::spawn("cat", metrics.clone(), logger.clone()).unwrap());
        let gw = SseGateway::new(
            child,
            "http://example.com".to_string(),
            "/message".to_string(),
            CorsHandler::new(CorsConfig::Disabled, false),
            vec![],
            metrics,
            logger,
            LogLevel::Debug,
            vec![],
        );

        let conn = gw.handle_sse_connect(None);
        let first = conn.receiver.try_recv().unwrap();
        match first {
            SseEvent::Endpoint(url) => {
                assert!(url.starts_with("http://example.com/message?sessionId="));
            }
            other => panic!("expected Endpoint, got {other:?}"),
        }
    }

    #[test]
    fn connect_sse_response_headers() {
        let gw = make_gateway("cat");
        let conn = gw.handle_sse_connect(None);

        let find = |name: &str| -> Option<String> {
            conn.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        };

        assert_eq!(find("Content-Type").as_deref(), Some("text/event-stream"));
        assert_eq!(find("Cache-Control").as_deref(), Some("no-cache"));
        assert_eq!(find("Connection").as_deref(), Some("keep-alive"));
        assert_eq!(find("X-Accel-Buffering").as_deref(), Some("no"));
    }

    // ─── Disconnect ─────────────────────────────────────────────────

    #[test]
    fn disconnect_removes_client() {
        let gw = make_gateway("cat");
        let conn = gw.handle_sse_connect(None);
        assert_eq!(gw.client_count(), 1);

        gw.disconnect_client(&conn.session_id);
        assert_eq!(gw.client_count(), 0);
    }

    #[test]
    fn disconnect_idempotent() {
        let gw = make_gateway("cat");
        let conn = gw.handle_sse_connect(None);

        gw.disconnect_client(&conn.session_id);
        gw.disconnect_client(&conn.session_id); // no panic
        assert_eq!(gw.client_count(), 0);
    }

    // ─── POST /message validation ───────────────────────────────────

    #[test]
    fn message_missing_session_id() {
        let gw = make_gateway("cat");
        let resp = gw.handle_message(None, b"body", None);
        assert_eq!(resp.status, 400);
        assert_eq!(resp.body, "Missing sessionId parameter");
    }

    #[test]
    fn message_empty_session_id() {
        let gw = make_gateway("cat");
        let resp = gw.handle_message(Some(""), b"body", None);
        assert_eq!(resp.status, 400);
        assert_eq!(resp.body, "Missing sessionId parameter");
    }

    #[test]
    fn message_unknown_session_id() {
        let gw = make_gateway("cat");
        let resp = gw.handle_message(Some("nonexistent-id"), b"body", None);
        assert_eq!(resp.status, 503);
        assert!(resp.body.contains("No active SSE connection for session"));
        assert!(resp.body.contains("nonexistent-id"));
    }

    #[test]
    fn message_empty_body() {
        let gw = make_gateway("cat");
        let conn = gw.handle_sse_connect(None);
        let resp = gw.handle_message(Some(conn.session_id.as_str()), b"", None);
        assert_eq!(resp.status, 400);
        assert_eq!(resp.body, "Empty body");
    }

    #[test]
    fn message_too_large() {
        let gw = make_gateway("cat");
        let conn = gw.handle_sse_connect(None);
        let big_body = vec![b'{'; MAX_BODY_SIZE + 1];
        let resp = gw.handle_message(Some(conn.session_id.as_str()), &big_body, None);
        assert_eq!(resp.status, 413);
    }

    #[test]
    fn message_malformed_json() {
        let gw = make_gateway("cat");
        let conn = gw.handle_sse_connect(None);
        let resp = gw.handle_message(Some(conn.session_id.as_str()), b"not json!", None);
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn message_invalid_utf8() {
        let gw = make_gateway("cat");
        let conn = gw.handle_sse_connect(None);
        let resp = gw.handle_message(Some(conn.session_id.as_str()), &[0xFF, 0xFE], None);
        assert_eq!(resp.status, 400);
        assert_eq!(resp.body, "Invalid UTF-8");
    }

    #[test]
    fn message_valid_returns_202() {
        let gw = make_gateway("cat");
        let conn = gw.handle_sse_connect(None);
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let resp = gw.handle_message(Some(conn.session_id.as_str()), body, None);
        assert_eq!(resp.status, 202);
        assert_eq!(resp.body, "Accepted");
    }

    #[test]
    fn message_batch_valid() {
        let gw = make_gateway("cat");
        let conn = gw.handle_sse_connect(None);
        let body = br#"[{"jsonrpc":"2.0","id":1,"method":"a"},{"jsonrpc":"2.0","method":"b"}]"#;
        let resp = gw.handle_message(Some(conn.session_id.as_str()), body, None);
        assert_eq!(resp.status, 202);
    }

    // ─── Relay roundtrip ────────────────────────────────────────────

    #[test]
    fn relay_roundtrip() {
        let (gw, child) = make_gateway_with_child("cat");
        let conn = gw.handle_sse_connect(None);

        // Consume endpoint event
        let _ = conn.receiver.try_recv().unwrap();

        // Start relay
        let relay_gw = gw.clone();
        let relay = std::thread::spawn(move || relay_gw.run_relay());

        // POST a message — cat echoes it back
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let resp = gw.handle_message(Some(conn.session_id.as_str()), body, None);
        assert_eq!(resp.status, 202);

        // Receive broadcast
        let msg = conn
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("should receive broadcast");
        match msg {
            SseEvent::Message(json) => {
                let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
                assert_eq!(parsed["method"], "ping");
            }
            other => panic!("expected Message, got {other:?}"),
        }

        child.kill();
        let exit_code = relay.join().unwrap();
        // cat killed by signal → exit code may be None
        assert!(exit_code.is_none() || exit_code == Some(0));
    }

    // ─── Early buffer ───────────────────────────────────────────────

    #[test]
    fn early_buffer_drains_to_first_client() {
        let (gw, _child) = make_gateway_with_child("cat");

        // Simulate buffered messages by manually inserting into early_buffer
        {
            let mut state = gw.inner.lock().unwrap();
            let buf = state.early_buffer.as_mut().unwrap();
            buf.push(r#"{"jsonrpc":"2.0","method":"notif1"}"#.to_string());
            buf.push(r#"{"jsonrpc":"2.0","method":"notif2"}"#.to_string());
        }

        // Connect client — should receive buffered messages
        let conn = gw.handle_sse_connect(None);

        // First: endpoint event
        let first = conn.receiver.try_recv().unwrap();
        assert!(matches!(first, SseEvent::Endpoint(_)));

        // Then: buffered messages
        let msg1 = conn.receiver.try_recv().unwrap();
        match msg1 {
            SseEvent::Message(json) => assert!(json.contains("notif1")),
            other => panic!("expected Message, got {other:?}"),
        }
        let msg2 = conn.receiver.try_recv().unwrap();
        match msg2 {
            SseEvent::Message(json) => assert!(json.contains("notif2")),
            other => panic!("expected Message, got {other:?}"),
        }

        // Early buffer should be None now
        assert!(gw.inner.lock().unwrap().early_buffer.is_none());
    }

    // ─── Broadcast to multiple clients ──────────────────────────────

    #[test]
    fn broadcast_to_multiple_clients() {
        let (gw, child) = make_gateway_with_child("cat");

        let conn1 = gw.handle_sse_connect(None);
        let conn2 = gw.handle_sse_connect(None);
        assert_eq!(gw.client_count(), 2);

        // Consume endpoint events
        let _ = conn1.receiver.try_recv().unwrap();
        let _ = conn2.receiver.try_recv().unwrap();

        // Start relay
        let relay_gw = gw.clone();
        let relay = std::thread::spawn(move || relay_gw.run_relay());

        // POST a message
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"broadcast_test"}"#;
        let resp = gw.handle_message(Some(conn1.session_id.as_str()), body, None);
        assert_eq!(resp.status, 202);

        // Both clients should receive the broadcast
        let msg1 = conn1
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let msg2 = conn2
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        for msg in [&msg1, &msg2] {
            match msg {
                SseEvent::Message(json) => {
                    assert!(json.contains("broadcast_test"));
                }
                other => panic!("expected Message, got {other:?}"),
            }
        }

        child.kill();
        relay.join().unwrap();
    }

    // ─── Dead child on POST ─────────────────────────────────────────

    #[test]
    fn message_to_dead_child() {
        let (gw, child) = make_gateway_with_child("true");

        // Wait for child to die
        for _ in 0..40 {
            if child.is_dead() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(child.is_dead());

        let conn = gw.handle_sse_connect(None);
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let resp = gw.handle_message(Some(conn.session_id.as_str()), body, None);
        assert_eq!(resp.status, 502);
    }

    // ─── Health endpoint ────────────────────────────────────────────

    #[test]
    fn health_endpoint_not_ready() {
        let gw = make_gateway("cat");
        let resp = gw.handle_health("/health", false, None);
        assert!(resp.is_some());
        let resp = resp.unwrap();
        assert_eq!(resp.status, 503);
    }

    #[test]
    fn health_endpoint_ready() {
        let gw = make_gateway("cat");
        gw.metrics.set_ready();
        let resp = gw.handle_health("/health", false, None).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, "ok");
    }

    #[test]
    fn health_endpoint_wrong_path() {
        let gw = make_gateway("cat");
        assert!(gw.handle_health("/not-health", false, None).is_none());
    }

    // ─── CORS ───────────────────────────────────────────────────────

    #[test]
    fn cors_headers_on_message() {
        let metrics = test_metrics();
        let logger = test_logger();
        let child =
            Arc::new(ChildBridge::spawn("cat", metrics.clone(), logger.clone()).unwrap());
        let gw = SseGateway::new(
            child,
            String::new(),
            "/message".to_string(),
            CorsHandler::new(CorsConfig::Wildcard, false),
            vec![],
            metrics,
            logger,
            LogLevel::Debug,
            vec![],
        );

        let conn = gw.handle_sse_connect(Some("https://example.com"));
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"test"}"#;
        let resp = gw.handle_message(
            Some(conn.session_id.as_str()),
            body,
            Some("https://example.com"),
        );
        assert_eq!(resp.status, 202);

        let has_cors = resp
            .headers
            .iter()
            .any(|(n, v)| n == "Access-Control-Allow-Origin" && v == "*");
        assert!(has_cors, "expected CORS header, got {:?}", resp.headers);
    }

    #[test]
    fn cors_preflight() {
        let metrics = test_metrics();
        let logger = test_logger();
        let child =
            Arc::new(ChildBridge::spawn("cat", metrics.clone(), logger.clone()).unwrap());
        let gw = SseGateway::new(
            child,
            String::new(),
            "/message".to_string(),
            CorsHandler::new(CorsConfig::Wildcard, false),
            vec![],
            metrics,
            logger,
            LogLevel::Debug,
            vec![],
        );

        let resp = gw.handle_options(Some("https://example.com"), None);
        assert!(resp.is_some());
        let resp = resp.unwrap();
        assert_eq!(resp.status, 204);
    }

    #[test]
    fn cors_disabled_no_options() {
        let gw = make_gateway("cat");
        assert!(gw.handle_options(Some("https://example.com"), None).is_none());
    }

    // ─── Custom headers ─────────────────────────────────────────────

    #[test]
    fn custom_headers_applied() {
        let metrics = test_metrics();
        let logger = test_logger();
        let child =
            Arc::new(ChildBridge::spawn("cat", metrics.clone(), logger.clone()).unwrap());
        let custom = vec![Header {
            name: "X-Custom".into(),
            value: "hello".into(),
        }];
        let gw = SseGateway::new(
            child,
            String::new(),
            "/message".to_string(),
            CorsHandler::new(CorsConfig::Disabled, false),
            custom,
            metrics,
            logger,
            LogLevel::Debug,
            vec![],
        );

        // Check on SSE connect
        let conn = gw.handle_sse_connect(None);
        let has_custom = conn
            .headers
            .iter()
            .any(|(n, v)| n == "X-Custom" && v == "hello");
        assert!(has_custom, "expected X-Custom on SSE, got {:?}", conn.headers);

        // Check on POST
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"test"}"#;
        let resp = gw.handle_message(Some(conn.session_id.as_str()), body, None);
        let has_custom = resp
            .headers
            .iter()
            .any(|(n, v)| n == "X-Custom" && v == "hello");
        assert!(has_custom, "expected X-Custom on POST, got {:?}", resp.headers);
    }

    // ─── Metrics ────────────────────────────────────────────────────

    #[test]
    fn metrics_tracked_on_connect_disconnect() {
        let metrics = test_metrics();
        let logger = test_logger();
        let child =
            Arc::new(ChildBridge::spawn("cat", metrics.clone(), logger.clone()).unwrap());
        let gw = SseGateway::new(
            child,
            String::new(),
            "/message".to_string(),
            CorsHandler::new(CorsConfig::Disabled, false),
            vec![],
            metrics.clone(),
            logger,
            LogLevel::Debug,
            vec![],
        );

        let conn = gw.handle_sse_connect(None);
        assert_eq!(metrics.active_clients.load(Ordering::Relaxed), 1);

        gw.disconnect_client(&conn.session_id);
        assert_eq!(metrics.active_clients.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.client_disconnects.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn metrics_total_requests_on_post() {
        let metrics = test_metrics();
        let logger = test_logger();
        let child =
            Arc::new(ChildBridge::spawn("cat", metrics.clone(), logger.clone()).unwrap());
        let gw = SseGateway::new(
            child,
            String::new(),
            "/message".to_string(),
            CorsHandler::new(CorsConfig::Disabled, false),
            vec![],
            metrics.clone(),
            logger,
            LogLevel::Debug,
            vec![],
        );

        let conn = gw.handle_sse_connect(None);
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"test"}"#;
        gw.handle_message(Some(conn.session_id.as_str()), body, None);
        assert_eq!(metrics.total_requests.load(Ordering::Relaxed), 1);

        // Even failed requests count
        gw.handle_message(None, b"", None);
        assert_eq!(metrics.total_requests.load(Ordering::Relaxed), 2);
    }

    // ─── Rapid connect/disconnect ───────────────────────────────────

    #[test]
    fn rapid_connect_disconnect_no_leaks() {
        let gw = make_gateway("cat");
        let mut connections = Vec::new();

        for _ in 0..20 {
            let conn = gw.handle_sse_connect(None);
            connections.push(conn);
        }
        assert_eq!(gw.client_count(), 20);

        for conn in &connections {
            gw.disconnect_client(&conn.session_id);
        }
        assert_eq!(gw.client_count(), 0);
        drop(connections);
    }
}
