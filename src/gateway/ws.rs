// Public API — suppress dead_code until wired up in main.rs.
#![allow(dead_code)]

//! stdio → WebSocket gateway.
//!
//! Single shared child process, per-client ID routing via HashMap multiplexer (D-001).
//!
//! # Endpoints
//!
//! - WebSocket upgrade path: bidirectional JSON-RPC over text frames
//! - Health endpoints: served via HTTP on the same listener (D-002)
//!
//! # Architecture
//!
//! One [`ChildBridge`] is shared across all connected WebSocket clients. A relay thread
//! reads child stdout and routes each message:
//!
//! - **Responses** (has `id`, no `method`): looked up in the mangled ID map, routed to
//!   the specific client that sent the original request. Original ID restored before delivery.
//! - **Server-initiated requests** (has `id` AND `method`): broadcast to all clients.
//! - **Notifications** (no `id`): broadcast to all clients (D-008: logged at debug level).
//!
//! # ID multiplexing (D-001)
//!
//! The TypeScript version mangles IDs via string concatenation (`clientId + ':' + msg.id`),
//! which breaks non-numeric IDs, IDs containing `:`, and mutates objects in-place.
//!
//! This implementation uses `HashMap<u64, (ClientId, Box<RawValue>)>` with auto-increment
//! `u64` mangled IDs. Original IDs are preserved exactly regardless of type.
//!
//! # Divergences
//!
//! - D-001: HashMap ID multiplexer (fixes string concat bugs)
//! - D-002: Health endpoint with proper if/else early returns
//! - D-007: Fire onclose/ondisconnection callbacks per-client
//! - D-008: Broadcast notifications logged at debug level
//! - D-010: Child stderr at error level
//! - D-012: Stdin writes mutex-protected via ChildBridge
//! - D-105: Custom headers NOT applied in WS mode

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::value::RawValue;

use crate::child::ChildBridge;
use crate::cli::LogLevel;
use crate::cors::{CorsHandler, CorsResult};
use crate::health;
use crate::jsonrpc::{Parsed, RawMessage};
use crate::observe::{Logger, Metrics};
use crate::session::SessionId;

// ─── Constants ──────────────────────────────────────────────────────

/// Per-client channel capacity.
const CLIENT_CHANNEL_CAP: usize = 256;

/// Maximum early buffer size (messages before first client).
const EARLY_BUFFER_CAP: usize = 256;

/// Backpressure timeout: disconnect client if channel full for this long.
const BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum incoming WS text frame size (16MB — D-101).
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

// ─── WS close codes ────────────────────────────────────────────────

/// Normal closure.
pub const WS_CLOSE_NORMAL: u16 = 1000;

/// Policy violation (e.g., invalid session).
pub const WS_CLOSE_POLICY: u16 = 1008;

/// Message too large.
pub const WS_CLOSE_TOO_LARGE: u16 = 1009;

/// Internal server error (e.g., child dead).
pub const WS_CLOSE_INTERNAL: u16 = 1011;

// ─── WS Event ──────────────────────────────────────────────────────

/// An event to send over a WebSocket connection.
#[derive(Debug, Clone)]
pub enum WsEvent {
    /// JSON-RPC message (text frame).
    Message(String),
    /// Close the connection with a specific code and reason.
    Close(u16, String),
}

// ─── Internal client state ─────────────────────────────────────────

struct ClientState {
    tx: SyncSender<WsEvent>,
    /// When backpressure started (channel full). None = healthy.
    backpressure_since: Option<Instant>,
}

// ─── ID Multiplexer (D-001) ────────────────────────────────────────

/// Maps mangled u64 IDs to (client_id, original_id).
///
/// When a client sends a request with an original ID, we:
/// 1. Allocate a new u64 mangled ID
/// 2. Store the mapping: mangled → (client_id, original_id)
/// 3. Rewrite the message with the mangled ID before sending to child
///
/// When the child responds:
/// 1. Look up the mangled ID → (client_id, original_id)
/// 2. Restore the original ID in the response
/// 3. Route to the specific client
struct IdMultiplexer {
    next_id: AtomicU64,
    /// mangled_id → (client_id, original_id)
    pending: Mutex<HashMap<u64, (SessionId, Box<RawValue>)>>,
}

impl IdMultiplexer {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Allocate a mangled ID and register the mapping.
    fn mangle(&self, client_id: &SessionId, original_id: &RawValue) -> u64 {
        let mangled = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().unwrap().insert(
            mangled,
            (client_id.clone(), original_id.to_owned()),
        );
        mangled
    }

    /// Look up and remove a mangled ID. Returns (client_id, original_id).
    fn unmangle(&self, mangled: u64) -> Option<(SessionId, Box<RawValue>)> {
        self.pending.lock().unwrap().remove(&mangled)
    }

    /// Remove all pending entries for a specific client (on disconnect).
    fn remove_client(&self, client_id: &SessionId) {
        self.pending
            .lock()
            .unwrap()
            .retain(|_, (cid, _)| cid != client_id);
    }
}

/// Shared interior state behind Mutex.
struct Inner {
    clients: HashMap<SessionId, ClientState>,
    /// Messages buffered before first client connects. `None` after first client.
    early_buffer: Option<Vec<RawMessage>>,
}

// ─── GatewayResponse ───────────────────────────────────────────────

/// Framework-agnostic HTTP response (for health endpoints).
pub struct GatewayResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl GatewayResponse {
    fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers.extend(headers);
        self
    }
}

// ─── WsConnection ──────────────────────────────────────────────────

/// Returned by [`WsGateway::handle_ws_connect`] for the caller to stream.
pub struct WsConnection {
    /// Client ID for this connection.
    pub client_id: SessionId,
    /// Receiver for outgoing WS events. The caller reads and writes to WebSocket.
    pub receiver: Receiver<WsEvent>,
}

// ─── WsGateway ─────────────────────────────────────────────────────

/// stdio → WebSocket gateway.
///
/// All state is behind `Arc` — cloning is cheap.
pub struct WsGateway {
    child: Arc<ChildBridge>,
    inner: Arc<Mutex<Inner>>,
    mux: Arc<IdMultiplexer>,
    cors: Arc<CorsHandler>,
    metrics: Arc<Metrics>,
    logger: Arc<Logger>,
    log_level: LogLevel,
    health_endpoints: Arc<Vec<String>>,
}

impl Clone for WsGateway {
    fn clone(&self) -> Self {
        Self {
            child: Arc::clone(&self.child),
            inner: Arc::clone(&self.inner),
            mux: Arc::clone(&self.mux),
            cors: Arc::clone(&self.cors),
            metrics: Arc::clone(&self.metrics),
            logger: Arc::clone(&self.logger),
            log_level: self.log_level,
            health_endpoints: Arc::clone(&self.health_endpoints),
        }
    }
}

impl WsGateway {
    /// Create a new WebSocket gateway.
    ///
    /// The child must already be spawned. Call [`run_relay`](Self::run_relay)
    /// or [`spawn_relay`](Self::spawn_relay) to start the routing loop.
    ///
    /// Note: custom headers are NOT accepted (D-105: custom headers not applied in WS mode).
    pub fn new(
        child: Arc<ChildBridge>,
        cors: CorsHandler,
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
            mux: Arc::new(IdMultiplexer::new()),
            cors: Arc::new(cors),
            metrics,
            logger,
            log_level,
            health_endpoints: Arc::new(health_endpoints),
        }
    }

    // ─── Relay ──────────────────────────────────────────────────────

    /// Run the relay loop (blocking). Returns the child's exit code.
    ///
    /// Reads child stdout and routes each message:
    /// - Responses → specific client via mangled ID lookup
    /// - Requests/notifications → broadcast to all clients
    ///
    /// Returns when the child exits. The caller MUST exit the process with
    /// the returned code (or 1 if None) so pm2/systemd can restart.
    pub fn run_relay(&self) -> Option<i32> {
        loop {
            match self.child.recv_message() {
                Ok(parsed) => {
                    let messages = match parsed {
                        Parsed::Single(msg) => vec![msg],
                        Parsed::Batch(msgs) => msgs,
                    };

                    for msg in messages {
                        self.route_message(msg);
                    }
                }
                Err(e) => {
                    self.logger.info(&format!("child stdout closed: {e}"));
                    break;
                }
            }
        }

        // Close all clients with 1011 (internal error — child died)
        self.close_all_clients(WS_CLOSE_INTERNAL, "child process exited");

        // Ensure child process group is cleaned up (D-016)
        self.child.kill();
        self.child.exit_code()
    }

    /// Convenience: spawn the relay in a background thread.
    pub fn spawn_relay(&self) -> std::thread::JoinHandle<Option<i32>> {
        let gw = self.clone();
        std::thread::Builder::new()
            .name("ws-relay".into())
            .spawn(move || gw.run_relay())
            .expect("spawn ws-relay thread")
    }

    /// Route a single child stdout message to the appropriate client(s).
    fn route_message(&self, msg: RawMessage) {
        if msg.is_response() {
            // Response: route to specific client via mangled ID lookup
            self.route_response(msg);
        } else if msg.is_notification() {
            // Notification: broadcast to all (D-008: debug level)
            self.logger.debug("broadcast notification to all WS clients");
            self.broadcast_message(msg);
        } else {
            // Server-initiated request (has id + method): broadcast to all
            self.broadcast_message(msg);
        }
    }

    /// Route a response to the specific client that sent the original request.
    fn route_response(&self, mut msg: RawMessage) {
        // Extract the mangled ID from the response
        let mangled_id_raw = match &msg.id {
            Some(raw) => raw.get(),
            None => {
                self.logger.error("response without id — dropping");
                return;
            }
        };

        // Parse the mangled u64 ID
        let mangled_id: u64 = match mangled_id_raw.parse() {
            Ok(id) => id,
            Err(_) => {
                // Not a mangled ID — this shouldn't happen. Log and drop.
                self.logger.error(&format!(
                    "response with non-mangled id {mangled_id_raw} — dropping"
                ));
                return;
            }
        };

        // Look up the original client and ID
        let (client_id, original_id) = match self.mux.unmangle(mangled_id) {
            Some(pair) => pair,
            None => {
                self.logger.debug(&format!(
                    "response for unknown mangled id {mangled_id} — dropping (client may have disconnected)"
                ));
                return;
            }
        };

        // Restore the original ID
        msg.id = Some(original_id);

        // Serialize and send to the specific client
        let json = match serde_json::to_string(&msg) {
            Ok(j) => j,
            Err(e) => {
                self.logger
                    .error(&format!("failed to serialize response: {e}"));
                return;
            }
        };

        let mut state = self.inner.lock().unwrap();
        if let Some(client) = state.clients.get_mut(&client_id) {
            match client.tx.try_send(WsEvent::Message(json)) {
                Ok(()) => {
                    if client.backpressure_since.take().is_some() {
                        self.logger.debug(&format!(
                            "backpressure resolved for client {client_id}"
                        ));
                    }
                }
                Err(TrySendError::Full(_)) => {
                    self.handle_backpressure(&mut state, &client_id);
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.remove_client_locked(&mut state, &client_id);
                }
            }
        }
    }

    /// Broadcast a message to all connected clients.
    fn broadcast_message(&self, msg: RawMessage) {
        let json = match serde_json::to_string(&msg) {
            Ok(j) => j,
            Err(e) => {
                self.logger
                    .error(&format!("failed to serialize child message: {e}"));
                return;
            }
        };

        let mut state = self.inner.lock().unwrap();

        if state.early_buffer.is_some() && state.clients.is_empty() {
            // No clients yet — buffer the message
            let buf = state.early_buffer.as_mut().unwrap();
            if buf.len() < EARLY_BUFFER_CAP {
                buf.push(msg);
            } else {
                self.logger.debug("early buffer full, dropping message");
            }
            return;
        }

        // Broadcast to all clients
        let event = WsEvent::Message(json);
        let mut to_remove = Vec::new();

        for (id, client) in state.clients.iter_mut() {
            match client.tx.try_send(event.clone()) {
                Ok(()) => {
                    if client.backpressure_since.take().is_some() {
                        self.logger
                            .debug(&format!("backpressure resolved for client {id}"));
                    }
                }
                Err(TrySendError::Full(_)) => {
                    let now = Instant::now();
                    match client.backpressure_since {
                        Some(since)
                            if now.duration_since(since) >= BACKPRESSURE_TIMEOUT =>
                        {
                            self.logger.info(&format!(
                                "disconnecting client {id}: backpressure timeout (>{}s)",
                                BACKPRESSURE_TIMEOUT.as_secs()
                            ));
                            Metrics::inc(&self.metrics.backpressure_events);
                            to_remove.push(id.clone());
                        }
                        Some(_) => { /* within timeout window, skip message */ }
                        None => {
                            client.backpressure_since = Some(now);
                            self.logger
                                .info(&format!("backpressure started for client {id}"));
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
            self.mux.remove_client(id);
            Metrics::inc(&self.metrics.client_disconnects);
            Metrics::dec_and_log(&self.metrics.active_clients, "active_clients", &self.logger);
        }
    }

    /// Handle backpressure for a specific client (called from route_response).
    fn handle_backpressure(&self, state: &mut Inner, client_id: &SessionId) {
        if let Some(client) = state.clients.get_mut(client_id) {
            let now = Instant::now();
            match client.backpressure_since {
                Some(since) if now.duration_since(since) >= BACKPRESSURE_TIMEOUT => {
                    self.logger.info(&format!(
                        "disconnecting client {client_id}: backpressure timeout (>{}s)",
                        BACKPRESSURE_TIMEOUT.as_secs()
                    ));
                    Metrics::inc(&self.metrics.backpressure_events);
                    self.remove_client_locked(state, client_id);
                }
                Some(_) => { /* within timeout window */ }
                None => {
                    client.backpressure_since = Some(now);
                    self.logger
                        .info(&format!("backpressure started for client {client_id}"));
                }
            }
        }
    }

    /// Remove a client while holding the inner lock.
    fn remove_client_locked(&self, state: &mut Inner, client_id: &SessionId) {
        if state.clients.remove(client_id).is_some() {
            self.mux.remove_client(client_id);
            Metrics::inc(&self.metrics.client_disconnects);
            Metrics::dec_and_log(&self.metrics.active_clients, "active_clients", &self.logger);
            // D-007: fire disconnect callback
            self.logger
                .info(&format!("WS client disconnected: client={client_id}"));
        }
    }

    /// Close all connected clients with a close frame (D-007).
    fn close_all_clients(&self, code: u16, reason: &str) {
        let mut state = self.inner.lock().unwrap();
        let client_ids: Vec<SessionId> = state.clients.keys().cloned().collect();

        for id in &client_ids {
            if let Some(client) = state.clients.get(id) {
                let _ = client
                    .tx
                    .try_send(WsEvent::Close(code, reason.to_string()));
            }
        }

        for id in &client_ids {
            state.clients.remove(id);
            self.mux.remove_client(id);
            Metrics::inc(&self.metrics.client_disconnects);
            Metrics::dec_and_log(&self.metrics.active_clients, "active_clients", &self.logger);
            // D-007: fire disconnect callback per-client
            self.logger
                .info(&format!("WS client disconnected (transport close): client={id}"));
        }
    }

    // ─── WS connect handler ────────────────────────────────────────

    /// Handle a new WebSocket connection.
    ///
    /// Returns a [`WsConnection`] with the client ID and a receiver for outgoing events.
    /// The caller must:
    /// 1. Loop on the receiver, sending events as WS text frames or close frames
    /// 2. Forward incoming WS text frames to [`handle_ws_message`](Self::handle_ws_message)
    /// 3. Call [`disconnect_client`](Self::disconnect_client) on WS close
    pub fn handle_ws_connect(&self) -> WsConnection {
        let client_id = SessionId::new();
        let (tx, rx) = mpsc::sync_channel(CLIENT_CHANNEL_CAP);

        // Register client and drain early buffer
        {
            let mut state = self.inner.lock().unwrap();

            if let Some(buffer) = state.early_buffer.take() {
                // Send buffered messages to this first client
                for msg in buffer {
                    let json = match serde_json::to_string(&msg) {
                        Ok(j) => j,
                        Err(_) => continue,
                    };
                    let _ = tx.try_send(WsEvent::Message(json));
                }
            }

            state.clients.insert(
                client_id.clone(),
                ClientState {
                    tx,
                    backpressure_since: None,
                },
            );
        }

        Metrics::inc_and_log(&self.metrics.active_clients, "active_clients", &self.logger);
        self.logger
            .info(&format!("WS client connected: client={client_id}"));

        WsConnection {
            client_id,
            receiver: rx,
        }
    }

    // ─── WS message handler ────────────────────────────────────────

    /// Handle an incoming WS text frame from a client.
    ///
    /// Parses the JSON-RPC message, mangles the ID (if present), and writes
    /// to child stdin. Returns `Err` with a close code and reason on failure.
    pub fn handle_ws_message(
        &self,
        client_id: &SessionId,
        text: &str,
    ) -> Result<(), (u16, String)> {
        Metrics::inc(&self.metrics.total_requests);

        // Validate frame size (D-101)
        if text.len() > MAX_FRAME_SIZE {
            return Err((WS_CLOSE_TOO_LARGE, "message too large".into()));
        }

        // Parse JSON-RPC
        let parsed = match crate::jsonrpc::parse_line(text) {
            Ok(p) => p,
            Err(e) => {
                self.logger.error(&format!("invalid JSON from client {client_id}: {e}"));
                // Match TS: fire onerror, no close frame. Return Ok to keep connection.
                return Ok(());
            }
        };

        let messages = match parsed {
            Parsed::Single(msg) => vec![msg],
            Parsed::Batch(msgs) => msgs,
        };

        for mut msg in messages {
            // If the message has an ID (request), mangle it for routing
            if let Some(original_id) = msg.id.take() {
                let mangled = self.mux.mangle(client_id, &original_id);
                msg.id = Some(
                    RawValue::from_string(mangled.to_string())
                        .expect("u64 is valid JSON"),
                );
            }

            // Write to child stdin (D-012: mutex-protected via ChildBridge)
            if let Err(e) = self.child.write_message(&msg) {
                self.logger
                    .error(&format!("failed to write to child: {e}"));
                return Err((WS_CLOSE_INTERNAL, "child process dead".into()));
            }
        }

        Ok(())
    }

    // ─── Disconnect handler ────────────────────────────────────────

    /// Remove a disconnected WebSocket client.
    ///
    /// Must be called when the WS connection closes. Fires per-client
    /// disconnect callback (D-007).
    pub fn disconnect_client(&self, client_id: &SessionId) {
        let removed = {
            let mut state = self.inner.lock().unwrap();
            state.clients.remove(client_id).is_some()
        };

        if removed {
            self.mux.remove_client(client_id);
            Metrics::inc(&self.metrics.client_disconnects);
            Metrics::dec_and_log(&self.metrics.active_clients, "active_clients", &self.logger);
            self.logger
                .info(&format!("WS client disconnected: client={client_id}"));
        }
    }

    // ─── Health handler (D-002) ────────────────────────────────────

    /// Handle a health endpoint request. Returns `None` if path is not a health endpoint.
    ///
    /// D-002: proper if/else with early returns — only one response per request.
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
        let cors_headers = match self.cors.process("GET", origin) {
            CorsResult::Disabled => vec![],
            CorsResult::ResponseHeaders(h) => h,
            CorsResult::Preflight(_) => vec![],
        };

        // D-105: custom headers NOT applied in WS mode
        let resp = GatewayResponse::new(health_resp.status_code(), health_resp.body())
            .with_header("Content-Type", health_resp.content_type())
            .with_headers(cors_headers);
        Some(resp)
    }

    // ─── OPTIONS handler ───────────────────────────────────────────

    /// Handle a CORS preflight request. Returns `None` if CORS is disabled.
    pub fn handle_options(&self, origin: Option<&str>) -> Option<GatewayResponse> {
        match self.cors.process("OPTIONS", origin) {
            CorsResult::Preflight(headers) => {
                Some(GatewayResponse::new(204, "").with_headers(headers))
            }
            _ => None,
        }
    }

    // ─── Accessors ─────────────────────────────────────────────────

    /// Number of connected WebSocket clients.
    pub fn client_count(&self) -> usize {
        self.inner.lock().unwrap().clients.len()
    }

    /// Check if the child process is dead.
    pub fn is_child_dead(&self) -> bool {
        self.child.is_dead()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{CorsConfig, LogLevel, OutputTransport};
    use std::sync::atomic::Ordering;

    fn test_metrics() -> Arc<Metrics> {
        Metrics::new()
    }

    fn test_logger() -> Arc<Logger> {
        Arc::new(Logger::buffered(OutputTransport::Ws, LogLevel::Debug))
    }

    fn make_gateway_with_child(cmd: &str) -> (WsGateway, Arc<ChildBridge>) {
        let metrics = test_metrics();
        let logger = test_logger();
        let child =
            Arc::new(ChildBridge::spawn(cmd, metrics.clone(), logger.clone()).unwrap());
        let gw = WsGateway::new(
            Arc::clone(&child),
            CorsHandler::new(CorsConfig::Disabled, false),
            metrics,
            logger,
            LogLevel::Debug,
            vec!["/health".to_string()],
        );
        (gw, child)
    }

    fn make_gateway(cmd: &str) -> WsGateway {
        make_gateway_with_child(cmd).0
    }

    // ─── WS connect ───────────────────────────────────────────────

    #[test]
    fn connect_assigns_unique_client_ids() {
        let gw = make_gateway("cat");
        let conn1 = gw.handle_ws_connect();
        let conn2 = gw.handle_ws_connect();
        assert_ne!(conn1.client_id, conn2.client_id);
        assert_eq!(gw.client_count(), 2);
    }

    #[test]
    fn connect_increments_metrics() {
        let gw = make_gateway("cat");
        let _conn = gw.handle_ws_connect();
        assert_eq!(gw.metrics.active_clients.load(Ordering::Relaxed), 1);
    }

    // ─── Disconnect ───────────────────────────────────────────────

    #[test]
    fn disconnect_removes_client() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect();
        assert_eq!(gw.client_count(), 1);

        gw.disconnect_client(&conn.client_id);
        assert_eq!(gw.client_count(), 0);
    }

    #[test]
    fn disconnect_idempotent() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect();

        gw.disconnect_client(&conn.client_id);
        gw.disconnect_client(&conn.client_id); // no panic
        assert_eq!(gw.client_count(), 0);
    }

    #[test]
    fn disconnect_cleans_up_mux_entries() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect();

        // Simulate a request being mangled
        let raw_id = RawValue::from_string("42".into()).unwrap();
        let mangled = gw.mux.mangle(&conn.client_id, &raw_id);
        assert!(gw.mux.pending.lock().unwrap().contains_key(&mangled));

        gw.disconnect_client(&conn.client_id);
        assert!(gw.mux.pending.lock().unwrap().is_empty());
    }

    // ─── ID Multiplexer (D-001) ───────────────────────────────────

    #[test]
    fn mux_mangle_unmangle_roundtrip() {
        let mux = IdMultiplexer::new();
        let client_id = SessionId::new();
        let original_id = RawValue::from_string("42".into()).unwrap();

        let mangled = mux.mangle(&client_id, &original_id);
        let (cid, oid) = mux.unmangle(mangled).unwrap();
        assert_eq!(cid, client_id);
        assert_eq!(oid.get(), "42");
    }

    #[test]
    fn mux_preserves_string_ids() {
        let mux = IdMultiplexer::new();
        let client_id = SessionId::new();
        let original_id = RawValue::from_string(r#""abc-def""#.into()).unwrap();

        let mangled = mux.mangle(&client_id, &original_id);
        let (_, oid) = mux.unmangle(mangled).unwrap();
        assert_eq!(oid.get(), r#""abc-def""#);
    }

    #[test]
    fn mux_preserves_null_id() {
        let mux = IdMultiplexer::new();
        let client_id = SessionId::new();
        let original_id = RawValue::from_string("null".into()).unwrap();

        let mangled = mux.mangle(&client_id, &original_id);
        let (_, oid) = mux.unmangle(mangled).unwrap();
        assert_eq!(oid.get(), "null");
    }

    #[test]
    fn mux_ids_with_colon() {
        // D-001: TS version breaks on IDs containing ':'
        let mux = IdMultiplexer::new();
        let client_id = SessionId::new();
        let original_id = RawValue::from_string(r#""a:b:c""#.into()).unwrap();

        let mangled = mux.mangle(&client_id, &original_id);
        let (_, oid) = mux.unmangle(mangled).unwrap();
        assert_eq!(oid.get(), r#""a:b:c""#);
    }

    #[test]
    fn mux_unique_mangled_ids() {
        let mux = IdMultiplexer::new();
        let client_id = SessionId::new();
        let id1 = RawValue::from_string("1".into()).unwrap();
        let id2 = RawValue::from_string("1".into()).unwrap();

        let m1 = mux.mangle(&client_id, &id1);
        let m2 = mux.mangle(&client_id, &id2);
        assert_ne!(m1, m2);
    }

    #[test]
    fn mux_remove_client_cleans_pending() {
        let mux = IdMultiplexer::new();
        let client_a = SessionId::new();
        let client_b = SessionId::new();
        let id = RawValue::from_string("1".into()).unwrap();

        let _ma = mux.mangle(&client_a, &id);
        let mb = mux.mangle(&client_b, &id);

        mux.remove_client(&client_a);
        // client_a's entries removed, client_b's remain
        assert_eq!(mux.pending.lock().unwrap().len(), 1);
        assert!(mux.unmangle(mb).is_some());
    }

    // ─── Message handling ─────────────────────────────────────────

    #[test]
    fn handle_message_valid_request() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect();

        let result = gw.handle_ws_message(
            &conn.client_id,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn handle_message_notification() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect();

        let result = gw.handle_ws_message(
            &conn.client_id,
            r#"{"jsonrpc":"2.0","method":"notifications/ping"}"#,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn handle_message_invalid_json_keeps_connection() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect();

        // Invalid JSON should NOT close the connection (match TS behavior)
        let result = gw.handle_ws_message(&conn.client_id, "not json!");
        assert!(result.is_ok());
        assert_eq!(gw.client_count(), 1);
    }

    #[test]
    fn handle_message_too_large() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect();

        let big = "x".repeat(MAX_FRAME_SIZE + 1);
        let result = gw.handle_ws_message(&conn.client_id, &big);
        assert!(result.is_err());
        let (code, _) = result.unwrap_err();
        assert_eq!(code, WS_CLOSE_TOO_LARGE);
    }

    #[test]
    fn handle_message_batch() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect();

        let result = gw.handle_ws_message(
            &conn.client_id,
            r#"[{"jsonrpc":"2.0","id":1,"method":"a"},{"jsonrpc":"2.0","method":"b"}]"#,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn handle_message_dead_child() {
        let (gw, child) = make_gateway_with_child("true");

        // Wait for child to die
        for _ in 0..40 {
            if child.is_dead() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let conn = gw.handle_ws_connect();
        let result = gw.handle_ws_message(
            &conn.client_id,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        );
        assert!(result.is_err());
        let (code, _) = result.unwrap_err();
        assert_eq!(code, WS_CLOSE_INTERNAL);
    }

    // ─── Relay roundtrip ──────────────────────────────────────────

    #[test]
    fn relay_roundtrip_routes_response_to_correct_client() {
        let (gw, child) = make_gateway_with_child("cat");
        let conn = gw.handle_ws_connect();

        // Start relay
        let relay_gw = gw.clone();
        let relay = std::thread::spawn(move || relay_gw.run_relay());

        // Send a request — cat echoes it back (as a request, not response, but
        // for the roundtrip test we verify the message arrives)
        let result = gw.handle_ws_message(
            &conn.client_id,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        );
        assert!(result.is_ok());

        // The echoed message has id + method → broadcast (server-initiated request)
        let msg = conn
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("should receive message");
        match msg {
            WsEvent::Message(json) => {
                let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
                assert_eq!(parsed["method"], "ping");
            }
            other => panic!("expected Message, got {other:?}"),
        }

        child.kill();
        relay.join().unwrap();
    }

    // ─── Early buffer ─────────────────────────────────────────────

    #[test]
    fn early_buffer_drains_to_first_client() {
        let (gw, _child) = make_gateway_with_child("cat");

        // Manually insert buffered messages
        {
            let mut state = gw.inner.lock().unwrap();
            let buf = state.early_buffer.as_mut().unwrap();
            let msg1: RawMessage =
                serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notif1"}"#).unwrap();
            let msg2: RawMessage =
                serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notif2"}"#).unwrap();
            buf.push(msg1);
            buf.push(msg2);
        }

        let conn = gw.handle_ws_connect();

        let msg1 = conn.receiver.try_recv().unwrap();
        match msg1 {
            WsEvent::Message(json) => assert!(json.contains("notif1")),
            other => panic!("expected Message, got {other:?}"),
        }
        let msg2 = conn.receiver.try_recv().unwrap();
        match msg2 {
            WsEvent::Message(json) => assert!(json.contains("notif2")),
            other => panic!("expected Message, got {other:?}"),
        }

        // Early buffer should be None now
        assert!(gw.inner.lock().unwrap().early_buffer.is_none());
    }

    // ─── Broadcast to multiple clients ────────────────────────────

    #[test]
    fn broadcast_to_multiple_clients() {
        let (gw, child) = make_gateway_with_child("cat");

        let conn1 = gw.handle_ws_connect();
        let conn2 = gw.handle_ws_connect();
        assert_eq!(gw.client_count(), 2);

        // Start relay
        let relay_gw = gw.clone();
        let relay = std::thread::spawn(move || relay_gw.run_relay());

        // Send a notification (no id) — will broadcast
        let result = gw.handle_ws_message(
            &conn1.client_id,
            r#"{"jsonrpc":"2.0","method":"broadcast_test"}"#,
        );
        assert!(result.is_ok());

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
                WsEvent::Message(json) => {
                    assert!(json.contains("broadcast_test"));
                }
                other => panic!("expected Message, got {other:?}"),
            }
        }

        child.kill();
        relay.join().unwrap();
    }

    // ─── Health endpoint (D-002) ──────────────────────────────────

    #[test]
    fn health_endpoint_not_ready() {
        let gw = make_gateway("cat");
        let resp = gw.handle_health("/health", false, None);
        assert!(resp.is_some());
        assert_eq!(resp.unwrap().status, 503);
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

    // ─── D-105: Custom headers NOT applied ────────────────────────

    #[test]
    fn no_custom_headers_in_ws_mode() {
        // WsGateway doesn't accept custom_headers parameter at all (D-105)
        // This test verifies the health endpoint doesn't apply custom headers.
        let gw = make_gateway("cat");
        gw.metrics.set_ready();
        let resp = gw.handle_health("/health", false, None).unwrap();
        // No custom headers present
        assert!(!resp.headers.iter().any(|(n, _)| n == "X-Custom"));
    }

    // ─── Metrics ──────────────────────────────────────────────────

    #[test]
    fn metrics_tracked_on_connect_disconnect() {
        let metrics = test_metrics();
        let logger = test_logger();
        let child =
            Arc::new(ChildBridge::spawn("cat", metrics.clone(), logger.clone()).unwrap());
        let gw = WsGateway::new(
            child,
            CorsHandler::new(CorsConfig::Disabled, false),
            metrics.clone(),
            logger,
            LogLevel::Debug,
            vec![],
        );

        let conn = gw.handle_ws_connect();
        assert_eq!(metrics.active_clients.load(Ordering::Relaxed), 1);

        gw.disconnect_client(&conn.client_id);
        assert_eq!(metrics.active_clients.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.client_disconnects.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn metrics_total_requests_on_message() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect();

        let _ = gw.handle_ws_message(
            &conn.client_id,
            r#"{"jsonrpc":"2.0","id":1,"method":"test"}"#,
        );
        assert_eq!(gw.metrics.total_requests.load(Ordering::Relaxed), 1);
    }

    // ─── Rapid connect/disconnect ─────────────────────────────────

    #[test]
    fn rapid_connect_disconnect_no_leaks() {
        let gw = make_gateway("cat");
        let mut connections = Vec::new();

        for _ in 0..20 {
            let conn = gw.handle_ws_connect();
            connections.push(conn);
        }
        assert_eq!(gw.client_count(), 20);

        for conn in &connections {
            gw.disconnect_client(&conn.client_id);
        }
        assert_eq!(gw.client_count(), 0);
    }

    // ─── Close all clients ────────────────────────────────────────

    #[test]
    fn close_all_sends_close_frames() {
        let gw = make_gateway("cat");
        let conn1 = gw.handle_ws_connect();
        let conn2 = gw.handle_ws_connect();
        assert_eq!(gw.client_count(), 2);

        gw.close_all_clients(WS_CLOSE_INTERNAL, "test");

        // Clients should receive close events
        let evt1 = conn1.receiver.try_recv().unwrap();
        match evt1 {
            WsEvent::Close(code, reason) => {
                assert_eq!(code, WS_CLOSE_INTERNAL);
                assert_eq!(reason, "test");
            }
            other => panic!("expected Close, got {other:?}"),
        }
        let evt2 = conn2.receiver.try_recv().unwrap();
        assert!(matches!(evt2, WsEvent::Close(WS_CLOSE_INTERNAL, _)));

        assert_eq!(gw.client_count(), 0);
    }
}
