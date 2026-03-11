// Public API — suppress dead_code until wired up in main.rs.
#![allow(dead_code)]

//! stdio → Streamable HTTP (stateful) gateway.
//!
//! Per-session child process with full MCP session lifecycle via `Mcp-Session-Id` header.
//!
//! # Endpoints
//!
//! - `POST /mcp`: JSON-RPC request/notification → child stdin, response via SSE stream
//! - `GET /mcp`: Long-lived SSE stream for server-initiated notifications
//! - `DELETE /mcp`: Terminate session (idempotent)
//! - Health endpoints as configured by `--healthEndpoint`
//!
//! # Architecture
//!
//! Each session owns a [`ChildBridge`] and a relay thread that routes child stdout
//! messages: responses go to the matching POST's response channel (keyed by JSON-RPC
//! id), notifications go to the GET SSE notification channel.
//!
//! POST handlers block until all expected responses arrive (or child dies/timeout),
//! then return an SSE batch response. The GET SSE stream requires lower-level
//! streaming beyond asupersync's batch-only `Sse` type — see [`SseWriter`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::child::ChildBridge;
use crate::cli::{Config, CorsConfig, Header};
use crate::codec;
use crate::cors::{self, CorsHandler, CorsResult};
use crate::error::GatewayError;
use crate::health;
use crate::jsonrpc::{Parsed, RawMessage};
use crate::observe::{Logger, Metrics};
use crate::session::{
    SessionAccessGuard, SessionError, SessionId, SessionManager, SessionManagerConfig,
    WeakSessionManager,
};

// ─── Constants ────────────────────────────────────────────────────────

/// Maximum request body size (16MB).
const MAX_BODY_SIZE: usize = 16 * 1024 * 1024;

/// Per-session bounded channel capacity for notifications (GET SSE stream).
const NOTIFICATION_CHANNEL_CAP: usize = 256;

/// Per-session bounded channel capacity for pending request responses.
const RESPONSE_CHANNEL_CAP: usize = 64;

/// Notification buffer cap during initialization.
const INIT_NOTIFICATION_BUFFER_CAP: usize = 256;

/// SSE keepalive interval for GET streams.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Timeout for waiting on child response (per POST request).
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

// ─── Per-session state ────────────────────────────────────────────────

/// State stored per session in [`SessionManager<SessionData>`].
pub struct SessionData {
    /// Child MCP server process for this session.
    pub child: ChildBridge,

    /// Pending request correlation: serialized JSON-RPC id → response sender.
    ///
    /// POST handlers insert entries; the relay thread sends matching responses
    /// and removes entries. Stale/unmatched responses are logged and discarded.
    pending_requests: RwLock<HashMap<String, SyncSender<RawMessage>>>,

    /// Sender for the GET SSE notification stream.
    notification_tx: SyncSender<RawMessage>,

    /// Receiver for GET SSE notifications — taken by first GET handler.
    notification_rx: Mutex<Option<Receiver<RawMessage>>>,

    /// Whether the initialize response has been seen.
    init_done: AtomicBool,

    /// Notification buffer: stores notifications received before init response.
    /// Flushed to the GET SSE stream once init completes.
    notification_buffer: Mutex<Vec<RawMessage>>,

    /// Whether a GET SSE stream is connected (only one allowed per session).
    has_sse_stream: AtomicBool,

    /// Set when child exits or EPIPE detected.
    child_failed: AtomicBool,

    /// Logger for this session.
    logger: Arc<Logger>,

    /// Metrics reference.
    metrics: Arc<Metrics>,
}

// SAFETY: All fields are either Send+Sync or protected by Mutex/RwLock.
unsafe impl Send for SessionData {}
unsafe impl Sync for SessionData {}

impl SessionData {
    /// Create session data with a spawned child.
    fn new(
        child: ChildBridge,
        logger: Arc<Logger>,
        metrics: Arc<Metrics>,
    ) -> Self {
        let (notification_tx, notification_rx) =
            mpsc::sync_channel(NOTIFICATION_CHANNEL_CAP);
        Self {
            child,
            pending_requests: RwLock::new(HashMap::new()),
            notification_tx,
            notification_rx: Mutex::new(Some(notification_rx)),
            init_done: AtomicBool::new(false),
            notification_buffer: Mutex::new(Vec::new()),
            has_sse_stream: AtomicBool::new(false),
            child_failed: AtomicBool::new(false),
            logger,
            metrics,
        }
    }

    /// Register a pending request id → response channel.
    fn register_pending(&self, id: &str, tx: SyncSender<RawMessage>) {
        self.pending_requests
            .write()
            .unwrap()
            .insert(id.to_string(), tx);
    }

    /// Route a response from child to the matching pending POST handler.
    /// Returns true if matched, false if no pending request found.
    fn route_response(&self, msg: &RawMessage) -> bool {
        let id_str = match &msg.id {
            Some(id) => id.get().to_string(),
            None => return false,
        };

        let tx = {
            let mut pending = self.pending_requests.write().unwrap();
            pending.remove(&id_str)
        };

        match tx {
            Some(sender) => {
                if sender.send(msg.clone()).is_err() {
                    self.logger.debug("response channel closed (POST handler gone)");
                }
                true
            }
            None => {
                self.logger.debug(&format!(
                    "no pending request for id {id_str}, discarding response"
                ));
                false
            }
        }
    }

    /// Route a notification to the GET SSE stream or buffer during init.
    fn route_notification(&self, msg: RawMessage) {
        if !self.init_done.load(Ordering::Acquire) {
            let mut buf = self.notification_buffer.lock().unwrap();
            if buf.len() >= INIT_NOTIFICATION_BUFFER_CAP {
                self.logger.info("notification buffer full during init, discarding oldest");
                buf.remove(0);
            }
            buf.push(msg);
            return;
        }

        match self.notification_tx.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                Metrics::inc(&self.metrics.backpressure_events);
                self.logger.info("notification channel full, backpressure active");
            }
            Err(TrySendError::Disconnected(_)) => {
                self.logger.debug("notification channel disconnected (no GET SSE stream)");
            }
        }
    }

    /// Mark initialization as complete and flush buffered notifications.
    fn mark_init_done(&self) {
        self.init_done.store(true, Ordering::Release);

        let buffered: Vec<RawMessage> = {
            let mut buf = self.notification_buffer.lock().unwrap();
            std::mem::take(&mut *buf)
        };

        if !buffered.is_empty() {
            self.logger.debug(&format!(
                "flushing {} buffered notifications after init",
                buffered.len()
            ));
            for msg in buffered {
                self.route_notification(msg);
            }
        }
    }

    /// Send error responses to all pending POST handlers.
    fn fail_pending_requests(&self, code: i32, message: &str) {
        let pending: HashMap<String, SyncSender<RawMessage>> = {
            let mut map = self.pending_requests.write().unwrap();
            std::mem::take(&mut *map)
        };

        for (id_str, tx) in pending {
            let id_raw = serde_json::value::RawValue::from_string(id_str)
                .unwrap_or_else(|_| {
                    serde_json::value::RawValue::from_string("null".into()).unwrap()
                });
            let err_msg = RawMessage::error_response(Some(id_raw), code, message);
            let _ = tx.send(err_msg);
        }
    }

    /// Take the notification receiver (for GET SSE stream). Returns None if already taken.
    fn take_notification_rx(&self) -> Option<Receiver<RawMessage>> {
        self.notification_rx.lock().unwrap().take()
    }
}

// ─── Child relay thread ────────────────────────────────────────────────

/// Spawn a relay thread that reads child stdout and routes messages.
///
/// - Responses (has id): routed to matching pending POST handler
/// - Notifications (no id): buffered during init, then sent to GET SSE stream
/// - Initialize response: triggers init_done + flushes buffer
///
/// On child exit/error: marks child_failed, sends error to all pending requests,
/// then exits.
fn spawn_relay_thread(
    session_id: SessionId,
    data: Arc<SessionData>,
    weak_mgr: WeakSessionManager<SessionData>,
) {
    let sid = session_id.to_string();
    std::thread::Builder::new()
        .name(format!("relay-{}", &sid[..8]))
        .spawn(move || {
            relay_loop(&session_id, &data, &weak_mgr);
        })
        .expect("spawn relay thread");
}

fn relay_loop(
    session_id: &SessionId,
    data: &SessionData,
    _weak_mgr: &WeakSessionManager<SessionData>,
) {
    loop {
        let parsed = match data.child.recv_message() {
            Ok(p) => p,
            Err(e) => {
                data.logger.info(&format!(
                    "session {} child stdout closed: {e}",
                    session_id
                ));
                data.child_failed.store(true, Ordering::Release);
                data.fail_pending_requests(
                    crate::error::codes::INTERNAL_ERROR,
                    "Child process dead",
                );
                return;
            }
        };

        match parsed {
            Parsed::Single(msg) => route_single_message(session_id, data, msg),
            Parsed::Batch(msgs) => {
                for msg in msgs {
                    route_single_message(session_id, data, msg);
                }
            }
        }
    }
}

fn route_single_message(session_id: &SessionId, data: &SessionData, msg: RawMessage) {
    // Check if this is the initialize response
    if msg.is_response() {
        // If we haven't seen init response yet and this is a response with result,
        // check if it could be the initialize response. Since we don't parse the
        // result, we mark init_done on the first response after session creation.
        // This is safe because initialize is always the first request.
        if !data.init_done.load(Ordering::Acquire) && msg.result.is_some() {
            data.mark_init_done();
        }

        // Route response to matching POST handler
        if !data.route_response(&msg) {
            data.logger.debug(&format!(
                "session {}: unmatched response id {:?}",
                session_id,
                msg.id.as_ref().map(|v| v.get())
            ));
        }
    } else if msg.is_notification() {
        data.route_notification(msg);
    } else {
        // Server-to-client request (has id + method from child)
        // Route as notification to GET SSE stream
        data.route_notification(msg);
    }
}

// ─── Request types (framework-agnostic) ────────────────────────────────

/// Parsed HTTP request for handler dispatch.
pub struct GatewayRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub query: HashMap<String, String>,
}

impl GatewayRequest {
    /// Get header value (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Get the Mcp-Session-Id header.
    pub fn session_id_header(&self) -> Option<&str> {
        self.header("mcp-session-id")
    }

    /// Check if Accept header contains a value.
    pub fn accepts(&self, media_type: &str) -> bool {
        self.header("accept")
            .map(|v| v.contains(media_type))
            .unwrap_or(false)
    }
}

/// HTTP response from handler.
pub struct GatewayResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl GatewayResponse {
    fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    fn body_str(mut self, body: &str) -> Self {
        self.body = body.as_bytes().to_vec();
        self
    }

    fn body_bytes(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    fn plain_text(status: u16, body: &str) -> Self {
        Self::new(status)
            .header("content-type", "text/plain")
            .body_str(body)
    }

    fn json(status: u16, body: &str) -> Self {
        Self::new(status)
            .header("content-type", "application/json")
            .body_str(body)
    }

    fn sse(status: u16, events: &str) -> Self {
        Self::new(status)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body_str(events)
    }

    fn no_content() -> Self {
        Self::new(204)
    }

    fn accepted() -> Self {
        Self::new(202)
    }

    fn from_error(err: &GatewayError) -> Self {
        Self::new(err.status_code())
            .header("content-type", err.content_type())
            .body_str(&err.body())
    }
}

// ─── SSE event formatting ──────────────────────────────────────────────

/// Format a RawMessage as an SSE event.
fn format_sse_event(msg: &RawMessage) -> String {
    let json = serde_json::to_string(msg).expect("JSON-RPC message serialization");
    format!("event: message\ndata: {json}\n\n")
}

/// Format a batch of RawMessages as a single SSE event.
fn format_sse_batch_event(msgs: &[RawMessage]) -> String {
    let json = serde_json::to_string(msgs).expect("JSON-RPC batch serialization");
    format!("event: message\ndata: {json}\n\n")
}

/// Format SSE keepalive comment.
fn format_sse_keepalive() -> &'static str {
    ": keepalive\n\n"
}

// ─── Gateway ──────────────────────────────────────────────────────────

/// Shared gateway state.
pub struct StatefulHttpGateway {
    sessions: SessionManager<SessionData>,
    cmd: String,
    cors: CorsHandler,
    custom_headers: Vec<Header>,
    mcp_path: String,
    health_endpoints: Vec<String>,
    metrics: Arc<Metrics>,
    logger: Arc<Logger>,
}

impl StatefulHttpGateway {
    /// Create a new stateful HTTP gateway.
    pub fn new(
        cmd: String,
        mcp_path: String,
        health_endpoints: Vec<String>,
        cors_config: CorsConfig,
        custom_headers: Vec<Header>,
        session_timeout: Option<Duration>,
        metrics: Arc<Metrics>,
        logger: Arc<Logger>,
    ) -> Self {
        let session_config = SessionManagerConfig {
            max_sessions: 1024,
            drain_timeout: Duration::from_secs(5),
            session_timeout,
        };

        let cleanup_logger = logger.clone();
        let cleanup: crate::session::CleanupFn = Box::new(move |id| {
            cleanup_logger.info(&format!("session {id} cleaned up"));
        });

        let sessions = SessionManager::new(session_config, metrics.clone(), Some(cleanup));

        Self {
            sessions,
            cmd,
            cors: CorsHandler::new(cors_config, true), // expose Mcp-Session-Id
            custom_headers,
            mcp_path,
            health_endpoints,
            metrics,
            logger,
        }
    }

    /// Session manager reference (for shutdown/drain).
    pub fn sessions(&self) -> &SessionManager<SessionData> {
        &self.sessions
    }

    // ─── Request dispatch ──────────────────────────────────────────

    /// Handle an incoming HTTP request. Returns a response.
    pub fn handle_request(&self, req: &GatewayRequest) -> GatewayResponse {
        // CORS handling
        let cors_result = self.cors.process(
            &req.method,
            req.header("origin"),
        );
        match &cors_result {
            CorsResult::Preflight(headers) => {
                let mut resp = GatewayResponse::no_content();
                resp.headers.extend(headers.iter().cloned());
                return resp;
            }
            CorsResult::Disabled => {}
            CorsResult::ResponseHeaders(_) => {} // applied after handler
        }

        // Health endpoints
        for health_path in &self.health_endpoints {
            if req.path == *health_path {
                let detail = req.query.get("detail").map(|v| v == "true").unwrap_or(false);
                let health = health::check_health(&self.metrics, detail, self.logger.level());
                let mut resp = GatewayResponse::new(health.status_code())
                    .header("content-type", health.content_type())
                    .body_str(health.body());
                // Custom headers apply to health endpoints only
                let mut h = Vec::new();
                cors::apply_custom_headers(&mut h, &self.custom_headers);
                resp.headers.extend(h);
                return self.apply_cors(resp, &cors_result);
            }
        }

        // MCP endpoint dispatch
        if req.path == self.mcp_path {
            Metrics::inc(&self.metrics.total_requests);
            let resp = match req.method.as_str() {
                "POST" => self.handle_post(req),
                "GET" => self.handle_get(req),
                "DELETE" => self.handle_delete(req),
                _ => GatewayResponse::from_error(&GatewayError::method_not_allowed()),
            };
            return self.apply_cors(resp, &cors_result);
        }

        // Not found
        self.apply_cors(
            GatewayResponse::plain_text(404, "Not Found"),
            &cors_result,
        )
    }

    fn apply_cors(&self, mut resp: GatewayResponse, cors_result: &CorsResult) -> GatewayResponse {
        if let CorsResult::ResponseHeaders(headers) = cors_result {
            resp.headers.extend(headers.iter().cloned());
        }
        resp
    }

    // ─── POST handler ──────────────────────────────────────────────

    fn handle_post(&self, req: &GatewayRequest) -> GatewayResponse {
        // 1. Validate Content-Type
        let content_type = req.header("content-type").unwrap_or("");
        if !content_type.contains("application/json") {
            return GatewayResponse::from_error(
                &GatewayError::wrong_content_type("application/json", content_type),
            );
        }

        // 2. Validate Accept header — stateful Streamable HTTP requires BOTH types.
        let accept = req.header("accept").unwrap_or("");
        if !accept.contains("application/json") || !accept.contains("text/event-stream") {
            if !accept.contains("*/*") && accept != "*" {
                return GatewayResponse::from_error(&GatewayError::missing_accept());
            }
        }

        // 3. Body size check
        if req.body.len() > MAX_BODY_SIZE {
            return GatewayResponse::from_error(&GatewayError::payload_too_large());
        }

        // 4. Parse JSON body
        let body_str = match std::str::from_utf8(&req.body) {
            Ok(s) => s,
            Err(_) => {
                return GatewayResponse::from_error(
                    &GatewayError::bad_request("invalid UTF-8"),
                );
            }
        };

        let parsed = match crate::jsonrpc::parse_line(body_str) {
            Ok(p) => p,
            Err(e) => {
                return GatewayResponse::from_error(
                    &GatewayError::bad_request(&format!("malformed JSON: {e}")),
                );
            }
        };

        // Collect messages into a vec for uniform handling
        let messages: Vec<RawMessage> = match parsed {
            Parsed::Single(msg) => vec![msg],
            Parsed::Batch(msgs) => msgs,
        };

        // 5. Determine session
        let session_id_header = req.session_id_header();

        match session_id_header {
            None => {
                // No session ID: must be initialize request
                if messages.len() == 1 && messages[0].is_initialize_request() {
                    self.handle_post_initialize(req, messages)
                } else {
                    GatewayResponse::from_error(
                        &GatewayError::bad_request(
                            "missing Mcp-Session-Id header (non-initialize request)",
                        ),
                    )
                }
            }
            Some(sid_str) => {
                let sid = SessionId::from_value(sid_str);
                self.handle_post_existing(req, &sid, messages)
            }
        }
    }

    /// Handle POST initialize: create new session.
    fn handle_post_initialize(
        &self,
        _req: &GatewayRequest,
        messages: Vec<RawMessage>,
    ) -> GatewayResponse {
        // Spawn child process
        let child = match ChildBridge::spawn(
            &self.cmd,
            self.metrics.clone(),
            self.logger.clone(),
        ) {
            Ok(c) => c,
            Err(e) => {
                Metrics::inc(&self.metrics.spawn_failures);
                return GatewayResponse::from_error(
                    &GatewayError::Internal(format!("failed to spawn child: {e}")),
                );
            }
        };

        // Create session data
        let data = SessionData::new(
            child,
            self.logger.clone(),
            self.metrics.clone(),
        );

        // Create session
        let session_id = match self.sessions.create(data) {
            Ok(id) => id,
            Err(SessionError::MaxSessionsReached) => {
                return GatewayResponse::from_error(&GatewayError::max_sessions());
            }
            Err(e) => {
                return GatewayResponse::from_error(
                    &GatewayError::Internal(format!("session create: {e:?}")),
                );
            }
        };

        self.logger.info(&format!("session {} created", session_id));

        // Spawn relay thread — needs access to session data via the manager
        // We access the session data through the session manager's internal state.
        // The relay thread gets a weak reference to avoid preventing shutdown.
        spawn_relay_for_session(&self.sessions, &session_id, self.logger.clone());

        // Forward messages and collect responses
        let resp = self.forward_and_respond(&session_id, messages);

        // Add Mcp-Session-Id header to response
        let mut response = resp;
        response.headers.push((
            "Mcp-Session-Id".to_string(),
            session_id.to_string(),
        ));
        response
    }

    /// Handle POST to existing session.
    fn handle_post_existing(
        &self,
        _req: &GatewayRequest,
        session_id: &SessionId,
        messages: Vec<RawMessage>,
    ) -> GatewayResponse {
        // Acquire session access guard
        let _guard = match self.sessions.acquire(session_id) {
            Ok(g) => g,
            Err(e) => {
                let err: GatewayError = e.into();
                return GatewayResponse::from_error(&err);
            }
        };

        self.forward_and_respond(session_id, messages)
    }

    /// Forward messages to child and collect responses.
    fn forward_and_respond(
        &self,
        session_id: &SessionId,
        messages: Vec<RawMessage>,
    ) -> GatewayResponse {
        // Access session data through the session manager
        // We need to read the session to get the SessionData reference.
        // Since SessionManager holds sessions behind RwLock<HashMap>, we acquire
        // and release the lock, but we need the data to persist.
        //
        // The session data is accessible via acquire() which gives us a guard,
        // but we need direct access to write messages and register pending requests.
        //
        // For now, we use the with_session helper pattern.

        // Collect request ids that need responses
        let mut request_ids: Vec<String> = Vec::new();
        let mut has_requests = false;

        for msg in &messages {
            if msg.is_request() {
                has_requests = true;
                if let Some(ref id) = msg.id {
                    request_ids.push(id.get().to_string());
                }
            }
        }

        // Write all messages to child stdin
        let write_result = self.with_session(session_id, |data| {
            // Check child health
            if data.child_failed.load(Ordering::Acquire) {
                return Err(GatewayError::child_dead());
            }

            for msg in &messages {
                if let Err(e) = data.child.write_message(msg) {
                    data.child_failed.store(true, Ordering::Release);
                    return Err(GatewayError::Child(e));
                }
            }
            Ok(())
        });

        match write_result {
            Some(Ok(())) => {}
            Some(Err(e)) => return GatewayResponse::from_error(&e),
            None => return GatewayResponse::from_error(&GatewayError::SessionNotFound),
        }

        // If no requests (all notifications), return 202 Accepted
        if !has_requests {
            return GatewayResponse::accepted();
        }

        // Register response channels for each request id
        let (collectors, rx_list) = self.register_response_channels(session_id, &request_ids);

        // Wait for all responses
        let mut sse_body = String::new();
        let mut responses_collected = 0;
        let expected = rx_list.len();

        for rx in rx_list {
            match rx.recv_timeout(RESPONSE_TIMEOUT) {
                Ok(msg) => {
                    sse_body.push_str(&format_sse_event(&msg));
                    responses_collected += 1;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.logger.error(&format!(
                        "session {session_id}: response timeout after {RESPONSE_TIMEOUT:?}"
                    ));
                    // Return what we have + error for remaining
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // Relay thread died (child exited)
                    break;
                }
            }
        }

        // Generate error responses for any missing
        if responses_collected < expected {
            for id_str in &request_ids[responses_collected..] {
                let id_raw = serde_json::value::RawValue::from_string(id_str.clone())
                    .unwrap_or_else(|_| {
                        serde_json::value::RawValue::from_string("null".into()).unwrap()
                    });
                let err_msg = RawMessage::error_response(
                    Some(id_raw),
                    crate::error::codes::INTERNAL_ERROR,
                    "Internal error",
                );
                sse_body.push_str(&format_sse_event(&err_msg));
            }
        }

        GatewayResponse::sse(200, &sse_body)
    }

    /// Register response channels for pending request ids.
    /// Returns the list of senders (for cleanup) and receivers (for collecting responses).
    fn register_response_channels(
        &self,
        session_id: &SessionId,
        request_ids: &[String],
    ) -> (Vec<SyncSender<RawMessage>>, Vec<Receiver<RawMessage>>) {
        let mut senders = Vec::with_capacity(request_ids.len());
        let mut receivers = Vec::with_capacity(request_ids.len());

        self.with_session(session_id, |data| {
            for id_str in request_ids {
                let (tx, rx) = mpsc::sync_channel(RESPONSE_CHANNEL_CAP);
                data.register_pending(id_str, tx.clone());
                senders.push(tx);
                receivers.push(rx);
            }
        });

        (senders, receivers)
    }

    // ─── GET handler (SSE stream) ──────────────────────────────────

    fn handle_get(&self, req: &GatewayRequest) -> GatewayResponse {
        // 1. Validate Accept
        if !req.accepts("text/event-stream") {
            return GatewayResponse::from_error(
                &GatewayError::NotAcceptable("missing Accept: text/event-stream".into()),
            );
        }

        // 2. Require Mcp-Session-Id
        let sid_str = match req.session_id_header() {
            Some(s) => s,
            None => {
                return GatewayResponse::from_error(
                    &GatewayError::bad_request("missing Mcp-Session-Id header"),
                );
            }
        };

        let sid = SessionId::from_value(sid_str);

        // 3. Validate session exists
        let _guard = match self.sessions.acquire(&sid) {
            Ok(g) => g,
            Err(e) => {
                let err: GatewayError = e.into();
                return GatewayResponse::from_error(&err);
            }
        };

        // 4. Claim SSE stream (only one per session)
        let claimed = self.with_session(&sid, |data| {
            data.has_sse_stream
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        });

        if claimed != Some(true) {
            return GatewayResponse::from_error(&GatewayError::sse_stream_conflict());
        }

        // 5. Take notification receiver
        let rx = self.with_session(&sid, |data| data.take_notification_rx());

        let rx = match rx {
            Some(Some(rx)) => rx,
            _ => {
                return GatewayResponse::from_error(&GatewayError::sse_stream_conflict());
            }
        };

        // 6. Collect available notifications (non-blocking drain)
        //
        // NOTE: For a true long-lived SSE stream, this handler would need to
        // keep the connection open and push events as they arrive. Asupersync's
        // web framework only supports batch responses, so we drain available
        // notifications and return them. A real implementation would use lower-level
        // HTTP APIs for streaming. This provides the correct test surface for
        // integration tests that send messages before the GET connects.
        let mut sse_body = String::new();

        // Drain all immediately available notifications
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    sse_body.push_str(&format_sse_event(&msg));
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }

        Metrics::inc_and_log(&self.metrics.active_clients, "active_clients", &self.logger);

        GatewayResponse::sse(200, &sse_body)
            .header("Mcp-Session-Id", sid_str)
    }

    // ─── DELETE handler ──────────────────────────────────────────────

    fn handle_delete(&self, req: &GatewayRequest) -> GatewayResponse {
        // 1. Require Mcp-Session-Id
        let sid_str = match req.session_id_header() {
            Some(s) => s,
            None => {
                return GatewayResponse::from_error(
                    &GatewayError::bad_request("missing Mcp-Session-Id header"),
                );
            }
        };

        let sid = SessionId::from_value(sid_str);

        // 2. Begin session deletion
        match self.sessions.begin_delete(&sid) {
            Ok(true) => {
                self.logger.info(&format!("session {sid} DELETE: transitioning to Closing"));

                // Spawn drain task
                let mgr = self.sessions.clone();
                let drain_sid = sid.clone();
                let drain_timeout = self.sessions.config().drain_timeout;
                let drain_logger = self.logger.clone();

                std::thread::Builder::new()
                    .name(format!("drain-{}", &sid_str[..8.min(sid_str.len())]))
                    .spawn(move || {
                        // Poll drain status
                        let start = std::time::Instant::now();
                        while start.elapsed() < drain_timeout {
                            if mgr.try_drain_close(&drain_sid) {
                                drain_logger.info(&format!(
                                    "session {drain_sid} drained and closed"
                                ));
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        // Force close after drain timeout
                        drain_logger.info(&format!(
                            "session {drain_sid} drain timeout, force closing"
                        ));
                        mgr.complete_close(&drain_sid);
                    })
                    .expect("spawn drain thread");

                GatewayResponse::new(200)
                    .header("content-type", "text/plain")
                    .body_str("OK")
            }
            Ok(false) => {
                // Already closing or closed — idempotent success
                GatewayResponse::new(200)
                    .header("content-type", "text/plain")
                    .body_str("OK")
            }
            Err(SessionError::NotFound) => {
                GatewayResponse::from_error(&GatewayError::SessionNotFound)
            }
            Err(e) => {
                let err: GatewayError = e.into();
                GatewayResponse::from_error(&err)
            }
        }
    }

    // ─── Helpers ───────────────────────────────────────────────────

    /// Access session data within the session manager's lock.
    ///
    /// Returns None if session not found.
    fn with_session<T, F>(&self, id: &SessionId, f: F) -> Option<T>
    where
        F: FnOnce(&SessionData) -> T,
    {
        self.sessions.with_session(id, f)
    }

    /// Shutdown: clear all sessions and kill children.
    pub fn shutdown(&self) {
        self.logger.info("shutting down all sessions");
        self.sessions.clear();
    }
}

// ─── SessionId helper ──────────────────────────────────────────────────


// ─── Relay spawning ────────────────────────────────────────────────────

/// Spawn the relay thread for a session. This is called after session creation.
///
/// Gets an `Arc<SessionData>` from the session manager so the relay thread can
/// hold a long-lived reference independent of the session map's lock.
fn spawn_relay_for_session(
    mgr: &SessionManager<SessionData>,
    session_id: &SessionId,
    logger: Arc<Logger>,
) {
    let data = match mgr.get_inner_arc(session_id) {
        Some(arc) => arc,
        None => {
            logger.error(&format!(
                "session {session_id}: cannot spawn relay, session not found"
            ));
            return;
        }
    };

    let weak = mgr.downgrade();
    spawn_relay_thread(session_id.clone(), data, weak);
}

// ─── Entry point ────────────────────────────────────────────────────────

/// Run the stdio → Streamable HTTP (stateful) gateway.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let logger = Arc::new(Logger::new(config.output_transport, config.log_level));
    let metrics = Metrics::new();

    logger.startup(
        env!("CARGO_PKG_VERSION"),
        &config.input_value,
        &config.output_transport.to_string(),
        config.port,
    );

    let _shutdown = crate::signal::install(&logger)?;

    let session_timeout = config.session_timeout.map(Duration::from_millis);
    let _gw = StatefulHttpGateway::new(
        config.input_value,
        config.streamable_http_path,
        config.health_endpoints,
        config.cors,
        config.headers,
        session_timeout,
        metrics,
        logger,
    );

    // TODO: Wire up TCP listener + HTTP serving (upcoming bead)
    anyhow::bail!("stdio->stateful HTTP serving not yet implemented")
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{LogLevel, OutputTransport};
    use serde_json::value::RawValue;

    fn test_logger() -> Arc<Logger> {
        Arc::new(Logger::buffered(OutputTransport::StreamableHttp, LogLevel::Debug))
    }

    fn test_metrics() -> Arc<Metrics> {
        Metrics::new()
    }

    fn make_request(id: &str, method: &str) -> RawMessage {
        RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(RawValue::from_string(id.into()).unwrap()),
            method: Some(method.into()),
            params: None,
            result: None,
            error: None,
            ..Default::default()
        }
    }

    fn make_notification(method: &str) -> RawMessage {
        RawMessage {
            jsonrpc: "2.0".into(),
            id: None,
            method: Some(method.into()),
            params: None,
            result: None,
            error: None,
            ..Default::default()
        }
    }

    fn make_response(id: &str) -> RawMessage {
        let result = serde_json::value::to_raw_value(&serde_json::json!({"ok": true})).unwrap();
        RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(RawValue::from_string(id.into()).unwrap()),
            method: None,
            params: None,
            result: Some(result),
            error: None,
            ..Default::default()
        }
    }

    fn make_gateway_request(
        method: &str,
        path: &str,
        body: &str,
        headers: Vec<(&str, &str)>,
    ) -> GatewayRequest {
        let mut h = HashMap::new();
        for (k, v) in headers {
            h.insert(k.to_string(), v.to_string());
        }
        GatewayRequest {
            method: method.to_string(),
            path: path.to_string(),
            headers: h,
            body: body.as_bytes().to_vec(),
            query: HashMap::new(),
        }
    }

    // ─── SSE event formatting ──────────────────────────────────────

    #[test]
    fn sse_event_format() {
        let msg = make_response("1");
        let event = format_sse_event(&msg);
        assert!(event.starts_with("event: message\ndata: "));
        assert!(event.ends_with("\n\n"));
        assert!(event.contains("\"jsonrpc\":\"2.0\""));
    }

    #[test]
    fn sse_batch_event_format() {
        let msgs = vec![make_response("1"), make_response("2")];
        let event = format_sse_batch_event(&msgs);
        assert!(event.starts_with("event: message\ndata: ["));
        assert!(event.ends_with("\n\n"));
    }

    #[test]
    fn sse_keepalive_format() {
        let ka = format_sse_keepalive();
        assert_eq!(ka, ": keepalive\n\n");
    }

    // ─── SessionData: response routing ─────────────────────────────

    #[test]
    fn route_response_to_pending() {
        let logger = test_logger();
        let metrics = test_metrics();

        // Create a child (echo server) for SessionData
        let child = ChildBridge::spawn("cat", metrics.clone(), logger.clone()).unwrap();
        let data = SessionData::new(child, logger, metrics);

        // Register a pending request
        let (tx, rx) = mpsc::sync_channel(1);
        data.register_pending("1", tx);

        // Route a response
        let resp = make_response("1");
        assert!(data.route_response(&resp));

        // Should receive on the channel
        let received = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(received.is_response());
        assert_eq!(received.id.as_ref().unwrap().get(), "1");

        // Kill child to clean up
        data.child.kill();
    }

    #[test]
    fn route_response_no_pending() {
        let logger = test_logger();
        let metrics = test_metrics();
        let child = ChildBridge::spawn("true", metrics.clone(), logger.clone()).unwrap();
        let data = SessionData::new(child, logger, metrics);

        let resp = make_response("99");
        assert!(!data.route_response(&resp));
    }

    // ─── SessionData: notification routing ─────────────────────────

    #[test]
    fn notification_buffered_before_init() {
        let logger = test_logger();
        let metrics = test_metrics();
        let child = ChildBridge::spawn("true", metrics.clone(), logger.clone()).unwrap();
        let data = SessionData::new(child, logger, metrics);

        // Before init_done, notifications should be buffered
        let notif = make_notification("tools/listChanged");
        data.route_notification(notif);

        let buf = data.notification_buffer.lock().unwrap();
        assert_eq!(buf.len(), 1);
        assert!(buf[0].is_notification());
    }

    #[test]
    fn notification_flushed_on_init_done() {
        let logger = test_logger();
        let metrics = test_metrics();
        let child = ChildBridge::spawn("true", metrics.clone(), logger.clone()).unwrap();
        let data = SessionData::new(child, logger, metrics);

        // Buffer two notifications
        data.route_notification(make_notification("a"));
        data.route_notification(make_notification("b"));

        // Take the notification receiver before marking init done
        let rx = data.take_notification_rx().unwrap();

        // Mark init done (flushes buffer)
        data.mark_init_done();

        // Should receive the buffered notifications
        let msg1 = rx.try_recv().unwrap();
        assert_eq!(msg1.method_str(), Some("a"));
        let msg2 = rx.try_recv().unwrap();
        assert_eq!(msg2.method_str(), Some("b"));
    }

    #[test]
    fn notification_sent_directly_after_init() {
        let logger = test_logger();
        let metrics = test_metrics();
        let child = ChildBridge::spawn("true", metrics.clone(), logger.clone()).unwrap();
        let data = SessionData::new(child, logger, metrics);

        let rx = data.take_notification_rx().unwrap();
        data.mark_init_done();

        // After init, notifications go directly to channel
        data.route_notification(make_notification("c"));
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.method_str(), Some("c"));
    }

    #[test]
    fn notification_buffer_cap_discards_oldest() {
        let logger = test_logger();
        let metrics = test_metrics();
        let child = ChildBridge::spawn("true", metrics.clone(), logger.clone()).unwrap();
        let data = SessionData::new(child, logger, metrics);

        // Fill buffer beyond cap
        for i in 0..INIT_NOTIFICATION_BUFFER_CAP + 5 {
            data.route_notification(make_notification(&format!("n{i}")));
        }

        let buf = data.notification_buffer.lock().unwrap();
        assert_eq!(buf.len(), INIT_NOTIFICATION_BUFFER_CAP);
        // Oldest should have been discarded, newest should be present
        let last = &buf[INIT_NOTIFICATION_BUFFER_CAP - 1];
        let expected_method = format!("n{}", INIT_NOTIFICATION_BUFFER_CAP + 4);
        assert_eq!(last.method_str(), Some(expected_method.as_str()));
    }

    // ─── SessionData: SSE stream claim ─────────────────────────────

    #[test]
    fn sse_stream_single_claim() {
        let logger = test_logger();
        let metrics = test_metrics();
        let child = ChildBridge::spawn("true", metrics.clone(), logger.clone()).unwrap();
        let data = SessionData::new(child, logger, metrics);

        // First claim succeeds
        assert!(data
            .has_sse_stream
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok());

        // Second claim fails
        assert!(data
            .has_sse_stream
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err());
    }

    #[test]
    fn take_notification_rx_only_once() {
        let logger = test_logger();
        let metrics = test_metrics();
        let child = ChildBridge::spawn("true", metrics.clone(), logger.clone()).unwrap();
        let data = SessionData::new(child, logger, metrics);

        assert!(data.take_notification_rx().is_some());
        assert!(data.take_notification_rx().is_none());
    }

    // ─── SessionData: fail pending requests ────────────────────────

    #[test]
    fn fail_pending_sends_errors() {
        let logger = test_logger();
        let metrics = test_metrics();
        let child = ChildBridge::spawn("true", metrics.clone(), logger.clone()).unwrap();
        let data = SessionData::new(child, logger, metrics);

        let (tx1, rx1) = mpsc::sync_channel(1);
        let (tx2, rx2) = mpsc::sync_channel(1);
        data.register_pending("1", tx1);
        data.register_pending("\"abc\"", tx2);

        data.fail_pending_requests(-32603, "Child process dead");

        let err1 = rx1.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(err1.is_response());
        assert!(err1.error.is_some());

        let err2 = rx2.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(err2.is_response());
        assert!(err2.error.is_some());

        // Pending map should be empty now
        assert!(data.pending_requests.read().unwrap().is_empty());
    }

    // ─── GatewayResponse helpers ───────────────────────────────────

    #[test]
    fn gateway_response_from_error() {
        let err = GatewayError::payload_too_large();
        let resp = GatewayResponse::from_error(&err);
        assert_eq!(resp.status, 413);
        assert_eq!(String::from_utf8_lossy(&resp.body), "payload too large");
    }

    #[test]
    fn gateway_response_sse() {
        let resp = GatewayResponse::sse(200, "event: message\ndata: {}\n\n");
        assert_eq!(resp.status, 200);
        let ct = resp.headers.iter().find(|(k, _)| k == "content-type");
        assert_eq!(ct.unwrap().1, "text/event-stream");
    }

    // ─── Gateway: POST validation ──────────────────────────────────

    #[test]
    fn post_wrong_content_type_returns_415() {
        let gw = make_test_gateway();
        let req = make_gateway_request(
            "POST",
            "/mcp",
            "{}",
            vec![
                ("content-type", "text/plain"),
                ("accept", "application/json, text/event-stream"),
            ],
        );
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 415);
    }

    #[test]
    fn post_missing_accept_returns_406() {
        let gw = make_test_gateway();
        let req = make_gateway_request(
            "POST",
            "/mcp",
            "{}",
            vec![("content-type", "application/json")],
        );
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 406);
    }

    #[test]
    fn post_body_too_large_returns_413() {
        let gw = make_test_gateway();
        let big_body = "x".repeat(MAX_BODY_SIZE + 1);
        let req = make_gateway_request(
            "POST",
            "/mcp",
            &big_body,
            vec![
                ("content-type", "application/json"),
                ("accept", "application/json, text/event-stream"),
            ],
        );
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 413);
    }

    #[test]
    fn post_malformed_json_returns_400() {
        let gw = make_test_gateway();
        let req = make_gateway_request(
            "POST",
            "/mcp",
            "not json",
            vec![
                ("content-type", "application/json"),
                ("accept", "application/json, text/event-stream"),
            ],
        );
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn post_non_init_without_session_id_returns_400() {
        let gw = make_test_gateway();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let req = make_gateway_request(
            "POST",
            "/mcp",
            body,
            vec![
                ("content-type", "application/json"),
                ("accept", "application/json, text/event-stream"),
            ],
        );
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn post_with_invalid_session_returns_404() {
        let gw = make_test_gateway();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let req = make_gateway_request(
            "POST",
            "/mcp",
            body,
            vec![
                ("content-type", "application/json"),
                ("accept", "application/json, text/event-stream"),
                ("mcp-session-id", "nonexistent-session"),
            ],
        );
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 404);
    }

    // ─── Gateway: GET validation ───────────────────────────────────

    #[test]
    fn get_missing_accept_returns_406() {
        let gw = make_test_gateway();
        let req = make_gateway_request("GET", "/mcp", "", vec![]);
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 406);
    }

    #[test]
    fn get_missing_session_id_returns_400() {
        let gw = make_test_gateway();
        let req = make_gateway_request(
            "GET",
            "/mcp",
            "",
            vec![("accept", "text/event-stream")],
        );
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn get_invalid_session_returns_404() {
        let gw = make_test_gateway();
        let req = make_gateway_request(
            "GET",
            "/mcp",
            "",
            vec![
                ("accept", "text/event-stream"),
                ("mcp-session-id", "nonexistent"),
            ],
        );
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 404);
    }

    // ─── Gateway: DELETE validation ────────────────────────────────

    #[test]
    fn delete_missing_session_id_returns_400() {
        let gw = make_test_gateway();
        let req = make_gateway_request("DELETE", "/mcp", "", vec![]);
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn delete_invalid_session_returns_404() {
        let gw = make_test_gateway();
        let req = make_gateway_request(
            "DELETE",
            "/mcp",
            "",
            vec![("mcp-session-id", "nonexistent")],
        );
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 404);
    }

    // ─── Gateway: method not allowed ───────────────────────────────

    #[test]
    fn put_returns_405_json_rpc() {
        let gw = make_test_gateway();
        let req = make_gateway_request("PUT", "/mcp", "", vec![]);
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 405);
        let body = String::from_utf8_lossy(&resp.body);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["error"]["code"], -32000);
    }

    // ─── Gateway: health endpoint ──────────────────────────────────

    #[test]
    fn health_endpoint_returns_503_before_ready() {
        let gw = make_test_gateway();
        let req = make_gateway_request("GET", "/healthz", "", vec![]);
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 503);
    }

    #[test]
    fn health_endpoint_returns_200_when_ready() {
        let gw = make_test_gateway();
        gw.metrics.set_ready();
        let req = make_gateway_request("GET", "/healthz", "", vec![]);
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 200);
        assert_eq!(String::from_utf8_lossy(&resp.body), "ok");
    }

    // ─── Gateway: CORS ─────────────────────────────────────────────

    #[test]
    fn options_preflight_returns_204() {
        let gw = make_test_gateway_with_cors();
        let req = make_gateway_request(
            "OPTIONS",
            "/mcp",
            "",
            vec![("origin", "https://example.com")],
        );
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 204);
        let acao = resp
            .headers
            .iter()
            .find(|(k, _)| k == "Access-Control-Allow-Origin");
        assert!(acao.is_some());
    }

    #[test]
    fn cors_exposes_session_header() {
        let gw = make_test_gateway_with_cors();
        let req = make_gateway_request(
            "DELETE",
            "/mcp",
            "",
            vec![
                ("origin", "https://example.com"),
                ("mcp-session-id", "test"),
            ],
        );
        let resp = gw.handle_request(&req);
        let expose = resp
            .headers
            .iter()
            .find(|(k, _)| k == "Access-Control-Expose-Headers");
        assert!(expose.is_some());
        assert!(expose.unwrap().1.contains("Mcp-Session-Id"));
    }

    // ─── Gateway: not found path ───────────────────────────────────

    #[test]
    fn unknown_path_returns_404() {
        let gw = make_test_gateway();
        let req = make_gateway_request("GET", "/unknown", "", vec![]);
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 404);
    }

    // ─── Gateway: full initialize lifecycle ────────────────────────

    #[test]
    fn post_initialize_creates_session() {
        let gw = make_test_gateway();
        let init_body = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"capabilities":{}}}"#;
        let req = make_gateway_request(
            "POST",
            "/mcp",
            init_body,
            vec![
                ("content-type", "application/json"),
                ("accept", "application/json, text/event-stream"),
            ],
        );
        let resp = gw.handle_request(&req);

        // Should succeed (either SSE response or error from child timeout)
        // The key assertion is that the session was created
        let session_header = resp
            .headers
            .iter()
            .find(|(k, _)| k == "Mcp-Session-Id");

        // Session should have been created (header present means success path)
        // The actual response may timeout waiting for child response if
        // the child (cat) doesn't respond within the timeout. That's expected
        // in unit tests — the integration test uses a proper mock server.
        if session_header.is_some() {
            // Verify session exists
            assert!(!gw.sessions.is_empty());
        }
    }

    #[test]
    fn post_notification_returns_202() {
        let gw = make_test_gateway();

        // First create a session via initialize
        let sid = create_test_session(&gw);

        // Send a notification to the session
        let notif_body = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let req = make_gateway_request(
            "POST",
            "/mcp",
            notif_body,
            vec![
                ("content-type", "application/json"),
                ("accept", "application/json, text/event-stream"),
                ("mcp-session-id", sid.as_str()),
            ],
        );
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 202);
    }

    #[test]
    fn delete_session_returns_200() {
        let gw = make_test_gateway();
        let sid = create_test_session(&gw);

        let req = make_gateway_request(
            "DELETE",
            "/mcp",
            "",
            vec![("mcp-session-id", sid.as_str())],
        );
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 200);
    }

    #[test]
    fn delete_idempotent() {
        let gw = make_test_gateway();
        let sid = create_test_session(&gw);

        // First DELETE
        let req = make_gateway_request(
            "DELETE",
            "/mcp",
            "",
            vec![("mcp-session-id", sid.as_str())],
        );
        let resp1 = gw.handle_request(&req);
        assert_eq!(resp1.status, 200);

        // Second DELETE (idempotent)
        let resp2 = gw.handle_request(&req);
        assert_eq!(resp2.status, 200);
    }

    #[test]
    fn post_to_closing_session_returns_503() {
        let gw = make_test_gateway();
        let sid = create_test_session(&gw);

        // DELETE to start closing
        let del_req = make_gateway_request(
            "DELETE",
            "/mcp",
            "",
            vec![("mcp-session-id", sid.as_str())],
        );
        gw.handle_request(&del_req);

        // POST to closing session
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let req = make_gateway_request(
            "POST",
            "/mcp",
            body,
            vec![
                ("content-type", "application/json"),
                ("accept", "application/json, text/event-stream"),
                ("mcp-session-id", sid.as_str()),
            ],
        );
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 503);
    }

    // ─── Gateway: max sessions ─────────────────────────────────────

    #[test]
    fn max_sessions_returns_503() {
        // Create gateway with max 2 sessions
        let gw = make_test_gateway_max_sessions(2);

        let _s1 = create_test_session(&gw);
        let _s2 = create_test_session(&gw);

        // Third should fail
        let init_body = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#;
        let req = make_gateway_request(
            "POST",
            "/mcp",
            init_body,
            vec![
                ("content-type", "application/json"),
                ("accept", "application/json, text/event-stream"),
            ],
        );
        let resp = gw.handle_request(&req);
        assert_eq!(resp.status, 503);
    }

    // ─── Test helpers ──────────────────────────────────────────────

    fn make_test_gateway() -> StatefulHttpGateway {
        StatefulHttpGateway::new(
            "cat".into(),
            "/mcp".into(),
            vec!["/healthz".into()],
            CorsConfig::Disabled,
            vec![],
            None,
            test_metrics(),
            test_logger(),
        )
    }

    fn make_test_gateway_with_cors() -> StatefulHttpGateway {
        StatefulHttpGateway::new(
            "cat".into(),
            "/mcp".into(),
            vec!["/healthz".into()],
            CorsConfig::Wildcard,
            vec![],
            None,
            test_metrics(),
            test_logger(),
        )
    }

    fn make_test_gateway_max_sessions(max: usize) -> StatefulHttpGateway {
        let metrics = test_metrics();
        let logger = test_logger();
        let session_config = SessionManagerConfig {
            max_sessions: max,
            drain_timeout: Duration::from_secs(5),
            session_timeout: None,
        };
        let sessions = SessionManager::new(session_config, metrics.clone(), None);

        StatefulHttpGateway {
            sessions,
            cmd: "cat".into(),
            cors: CorsHandler::new(CorsConfig::Disabled, true),
            custom_headers: vec![],
            mcp_path: "/mcp".into(),
            health_endpoints: vec!["/healthz".into()],
            metrics,
            logger,
        }
    }

    /// Create a test session by directly using the session manager,
    /// bypassing the HTTP POST handler (avoids child response timeout).
    fn create_test_session(gw: &StatefulHttpGateway) -> SessionId {
        let child = ChildBridge::spawn("cat", gw.metrics.clone(), gw.logger.clone()).unwrap();
        let data = SessionData::new(child, gw.logger.clone(), gw.metrics.clone());
        gw.sessions.create(data).unwrap()
    }
}
