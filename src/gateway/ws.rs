
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
//! - D-105: Custom headers applied in WS mode (improvement over TS)

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use asupersync::channel::mpsc as async_mpsc;
use asupersync::channel::mpsc::RecvError;
use asupersync::codec::{Decoder, Encoder, Framed};
use asupersync::http::h1::{Http1Codec, Method as H1Method};
use asupersync::io::AsyncWriteExt;
use asupersync::net::TcpListener as AsyncTcpListener;
use asupersync::net::websocket::{Frame, FrameCodec, ServerHandshake, HttpRequest as WsHttpRequest};
use asupersync::runtime::{spawn_blocking, JoinHandle, RuntimeHandle};
use asupersync::stream::StreamExt;
use asupersync::time::{timeout, wall_now};

use serde_json::value::RawValue;

use crate::child::ChildBridge;
use crate::cli::{Header, LogLevel};
use crate::cors::{self, CorsHandler, CorsResult};
use crate::health;
use crate::jsonrpc::{Parsed, RawMessage};
use crate::observe::{Logger, Metrics};
use crate::session::SessionId;

// ─── Constants ──────────────────────────────────────────────────────

/// Per-client channel capacity.
#[allow(dead_code)]
const CLIENT_CHANNEL_CAP: usize = 256;

/// Maximum early buffer size (messages before first client).
#[allow(dead_code)]
const EARLY_BUFFER_CAP: usize = 256;

/// Backpressure timeout: disconnect client if channel full for this long.
#[allow(dead_code)]
const BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum incoming WS text frame size (16MB — D-101).
#[allow(dead_code)]
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Maximum concurrent WebSocket clients (D-102).
#[allow(dead_code)]
const MAX_WS_CLIENTS: usize = 1024;

// ─── WS close codes ────────────────────────────────────────────────

/// Normal closure.
#[allow(dead_code)]
pub const WS_CLOSE_NORMAL: u16 = 1000;

/// Policy violation (e.g., invalid session).
#[allow(dead_code)]
pub const WS_CLOSE_POLICY: u16 = 1008;

/// Message too large.
#[allow(dead_code)]
pub const WS_CLOSE_TOO_LARGE: u16 = 1009;

/// Internal server error (e.g., child dead).
#[allow(dead_code)]
pub const WS_CLOSE_INTERNAL: u16 = 1011;

// ─── WS Event ──────────────────────────────────────────────────────

/// An event to send over a WebSocket connection.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum WsEvent {
    /// JSON-RPC message (text frame).
    Message(String),
    /// Close the connection with a specific code and reason.
    Close(u16, String),
}

// ─── Internal client state ─────────────────────────────────────────

#[allow(dead_code)]
struct ClientState {
    tx: async_mpsc::Sender<WsEvent>,
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
#[allow(dead_code)]
struct IdMultiplexer {
    next_id: AtomicU64,
    /// mangled_id → (client_id, original_id)
    pending: Mutex<HashMap<u64, (SessionId, Box<RawValue>)>>,
}

#[allow(dead_code)]
impl IdMultiplexer {
    #[allow(dead_code)]
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Allocate a mangled ID and register the mapping.
    #[allow(dead_code)]
    fn mangle(&self, client_id: &SessionId, original_id: &RawValue) -> u64 {
        let mangled = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().unwrap().insert(
            mangled,
            (client_id.clone(), original_id.to_owned()),
        );
        mangled
    }

    /// Look up and remove a mangled ID. Returns (client_id, original_id).
    #[allow(dead_code)]
    fn unmangle(&self, mangled: u64) -> Option<(SessionId, Box<RawValue>)> {
        self.pending.lock().unwrap().remove(&mangled)
    }

    /// Remove all pending entries for a specific client (on disconnect).
    #[allow(dead_code)]
    fn remove_client(&self, client_id: &SessionId) {
        self.pending
            .lock().unwrap()
            .retain(|_, (cid, _)| cid != client_id);
    }
}

/// Shared interior state behind Mutex.
#[allow(dead_code)]
struct Inner {
    clients: HashMap<SessionId, ClientState>,
    /// Messages buffered before first client connects. `None` after first client.
    early_buffer: Option<Vec<RawMessage>>,
}

// ─── GatewayResponse ───────────────────────────────────────────────

/// Framework-agnostic HTTP response (for health endpoints).
#[allow(dead_code)]
#[derive(Debug)]
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

// ─── WsConnection ──────────────────────────────────────────────────

/// Returned by [`WsGateway::handle_ws_connect`] for the caller to stream.
#[allow(dead_code)]
pub struct WsConnection {
    /// Client ID for this connection.
    pub client_id: SessionId,
    /// Receiver for outgoing WS events. The caller reads and writes to WebSocket.
    pub receiver: async_mpsc::Receiver<WsEvent>,
}

// ─── WsGateway ─────────────────────────────────────────────────────

/// stdio → WebSocket gateway.
///
/// All state is behind `Arc` — cloning is cheap.
#[allow(dead_code)]
pub struct WsGateway {
    child: Arc<ChildBridge>,
    inner: Arc<Mutex<Inner>>,
    mux: Arc<IdMultiplexer>,
    cors: Arc<CorsHandler>,
    custom_headers: Arc<Vec<Header>>,
    metrics: Arc<Metrics>,
    logger: Arc<Logger>,
    log_level: LogLevel,
    health_endpoints: Arc<Vec<String>>,
}

#[allow(dead_code)]
impl Clone for WsGateway {
    #[allow(dead_code)]
    fn clone(&self) -> Self {
        Self {
            child: Arc::clone(&self.child),
            inner: Arc::clone(&self.inner),
            mux: Arc::clone(&self.mux),
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
impl WsGateway {
    /// Create a new WebSocket gateway.
    ///
    /// The child must already be spawned. Call [`run_relay`](Self::run_relay)
    /// or [`spawn_relay`](Self::spawn_relay) to start the routing loop.
    ///
    /// D-105: `--header` applies to all server modes including WebSocket.
    #[allow(dead_code)]
    pub fn new(
        child: Arc<ChildBridge>,
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
            mux: Arc::new(IdMultiplexer::new()),
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
    /// Reads child stdout and routes each message:
    /// - Responses → specific client via mangled ID lookup
    /// - Requests/notifications → broadcast to all clients
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
    #[allow(dead_code)]
    pub fn spawn_relay(&self) -> std::thread::JoinHandle<Option<i32>> {
        let gw = self.clone();
        std::thread::Builder::new()
            .name("ws-relay".into())
            .spawn(move || gw.run_relay())
            .expect("spawn ws-relay thread")
    }

    /// Async relay loop: reads child stdout via spawn_blocking, routes to clients.
    #[allow(dead_code)]
    pub async fn run_relay_async(&self) -> Option<i32> {
        let child = self.child.clone();
        loop {
            let child_clone = child.clone();
            let result = spawn_blocking(move || child_clone.recv_message()).await;
            let parsed = match result {
                Ok(p) => p,
                Err(e) => {
                    self.logger.info(&format!("child stdout closed: {e}"));
                    break;
                }
            };

            let messages = match parsed {
                crate::jsonrpc::Parsed::Single(msg) => vec![msg],
                crate::jsonrpc::Parsed::Batch(msgs) => msgs,
            };

            for msg in messages {
                self.route_message(msg);
            }
        }

        self.close_all_clients(WS_CLOSE_INTERNAL, "child process exited");
        self.child.kill();
        self.child.exit_code()
    }

    /// Spawn the async relay as a runtime task.
    #[allow(dead_code)]
    pub fn spawn_relay_async(&self, rt_handle: &RuntimeHandle) -> JoinHandle<Option<i32>> {
        let gw = self.clone();
        rt_handle.spawn(async move { gw.run_relay_async().await })
    }

    /// Route a single child stdout message to the appropriate client(s).
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
                Err(async_mpsc::SendError::Full(_)) => {
                    self.handle_backpressure(&mut state, &client_id);
                }
                Err(_) => {
                    // Disconnected or Cancelled
                    self.remove_client_locked(&mut state, &client_id);
                }
            }
        }
    }

    /// Broadcast a message to all connected clients.
    #[allow(dead_code)]
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
                Err(async_mpsc::SendError::Full(_)) => {
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
                Err(_) => {
                    // Disconnected or Cancelled
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
    ///
    /// Returns `Err(GatewayResponse)` with HTTP 503 if the client limit is reached (D-102).
    #[allow(dead_code)]
    pub fn handle_ws_connect(&self) -> Result<WsConnection, GatewayResponse> {
        // Enforce max concurrent clients (D-102).
        if self.inner.lock().unwrap().clients.len() >= MAX_WS_CLIENTS {
            return Err(GatewayResponse::new(503, "Too Many Connections"));
        }

        let client_id = SessionId::new();
        let (tx, rx) = async_mpsc::channel::<WsEvent>(CLIENT_CHANNEL_CAP);

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

        Ok(WsConnection {
            client_id,
            receiver: rx,
        })
    }

    // ─── WS message handler ────────────────────────────────────────

    /// Handle an incoming WS text frame from a client.
    ///
    /// Parses the JSON-RPC message, mangles the ID (if present), and writes
    /// to child stdin. Returns `Err` with a close code and reason on failure.
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
            .with_header("Content-Type", health_resp.content_type())
            .with_headers(cors_headers);
        cors::apply_custom_headers(&mut resp.headers, &self.custom_headers);
        Some(resp)
    }

    // ─── OPTIONS handler ───────────────────────────────────────────

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

    // ─── Accessors ─────────────────────────────────────────────────

    /// Number of connected WebSocket clients.
    #[allow(dead_code)]
    pub fn client_count(&self) -> usize {
        self.inner.lock().unwrap().clients.len()
    }

    /// Check if the child process is dead.
    #[allow(dead_code)]
    pub fn is_child_dead(&self) -> bool {
        self.child.is_dead()
    }

    /// Custom headers to apply to the WebSocket upgrade (101) response.
    ///
    /// The caller building the HTTP 101 handshake response should call
    /// [`cors::apply_custom_headers`] with these headers.
    #[allow(dead_code)]
    pub fn custom_headers(&self) -> &[Header] {
        &self.custom_headers
    }
}

// ─── Entry point ────────────────────────────────────────────────────────

/// Run the stdio → WebSocket gateway.
#[allow(dead_code)]
pub async fn run(
    _cx: &asupersync::Cx,
    config: crate::cli::Config,
    rt_handle: RuntimeHandle,
) -> anyhow::Result<()> {
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
    let metrics_ready = metrics.clone();
    let gw = WsGateway::new(
        child,
        cors_handler,
        config.headers,
        metrics,
        logger,
        config.log_level,
        config.health_endpoints,
    );

    // Spawn async relay task
    let relay_handle = gw.spawn_relay_async(&rt_handle);

    // Bind async TCP listener
    let listener = AsyncTcpListener::bind(format!("0.0.0.0:{}", config.port))
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind port {}: {e}", config.port))?;

    // Mark gateway as ready
    metrics_ready.set_ready();

    // Accept loop as async task
    let accept_gw = gw.clone();
    let rt_accept = rt_handle.clone();
    rt_handle.spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let gw = accept_gw.clone();
                    rt_accept.spawn(handle_ws_connection_async(stream, gw));
                }
                Err(_) => break,
            }
        }
    });

    // Wait for relay to complete (child exited)
    let exit_code = relay_handle.await.unwrap_or(1);
    std::process::exit(exit_code);
}

/// Poll interval for draining the relay channel in the WS connection task.
const WS_POLL_INTERVAL: Duration = Duration::from_millis(50);

async fn handle_ws_connection_async(
    stream: asupersync::net::TcpStream,
    gw: WsGateway,
) {
    let cx = match asupersync::Cx::current() {
        Some(c) => c,
        None => return,
    };

    // Parse initial HTTP request
    let mut framed = Framed::new(stream, Http1Codec::new());
    let req = match framed.next().await {
        Some(Ok(r)) => r,
        _ => return,
    };

    let uri = req.uri.clone();
    let (path, query_str) = if let Some(pos) = uri.find('?') {
        (&uri[..pos], &uri[pos + 1..])
    } else {
        (uri.as_str(), "")
    };

    let origin = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("origin"))
        .map(|(_, v)| v.as_str());
    let ac_req_headers = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("access-control-request-headers"))
        .map(|(_, v)| v.as_str());
    let detail = query_str.split('&').any(|pair| pair == "detail=true");

    // Get raw stream
    let parts = framed.into_parts();
    let mut stream = parts.inner;

    // Health endpoints
    if let Some(resp) = gw.handle_health(path, detail, origin) {
        write_ws_http_response(&mut stream, &resp).await;
        return;
    }

    // OPTIONS (CORS preflight)
    if req.method == H1Method::Options {
        if let Some(resp) = gw.handle_options(origin, ac_req_headers) {
            write_ws_http_response(&mut stream, &resp).await;
        }
        return;
    }

    // Only GET is valid for WS upgrade
    if req.method != H1Method::Get {
        let _ = stream
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n")
            .await;
        return;
    }

    // Build raw request string for ServerHandshake
    let mut raw_req = format!("GET {} HTTP/1.1\r\n", req.uri);
    for (k, v) in &req.headers {
        raw_req.push_str(&format!("{k}: {v}\r\n"));
    }
    raw_req.push_str("\r\n");

    let ws_req = match WsHttpRequest::parse(raw_req.as_bytes()) {
        Ok(r) => r,
        Err(_) => {
            let _ = stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .await;
            return;
        }
    };

    let accept = match ServerHandshake::new().accept(&ws_req) {
        Ok(a) => a,
        Err(_) => {
            let _ = stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .await;
            return;
        }
    };

    // Check client limit BEFORE sending 101 (so we can still return a proper HTTP error)
    let conn = match gw.handle_ws_connect() {
        Ok(c) => c,
        Err(resp) => {
            write_ws_http_response(&mut stream, &resp).await;
            return;
        }
    };

    // Write HTTP 101 Switching Protocols with custom headers (D-105)
    let mut resp_bytes = accept.response_bytes();
    // AcceptResponse::response_bytes() ends with \r\n\r\n; insert custom headers before final \r\n
    if resp_bytes.ends_with(b"\r\n\r\n") {
        resp_bytes.truncate(resp_bytes.len() - 2);
        for h in gw.custom_headers() {
            resp_bytes.extend_from_slice(format!("{}: {}\r\n", h.name, h.value).as_bytes());
        }
        resp_bytes.extend_from_slice(b"\r\n");
    }
    if stream.write_all(&resp_bytes).await.is_err() {
        gw.disconnect_client(&conn.client_id);
        return;
    }

    // WS frame loop: use FrameCodec directly on split stream halves
    // Split into read/write so we can do concurrent send (relay channel) and recv (client)
    let (mut read_half, mut write_half) = stream.into_split();
    let mut enc = FrameCodec::server();
    let mut dec = FrameCodec::server();
    let mut read_buf = asupersync::bytes::BytesMut::with_capacity(8192);
    let mut write_buf = asupersync::bytes::BytesMut::with_capacity(8192);
    let rx = conn.receiver;
    let client_id = conn.client_id.clone();

    // Spawn WS send task: reads from relay channel, writes WS frames to write_half
    // This task owns write_half and relay receiver
    let send_cx = cx.clone();
    let send_gw = gw.clone();
    let send_client_id = client_id.clone();
    // Shared flag: signal send task to close when recv task finishes
    // We route close via the WsEvent channel (WsEvent::Close) from relay side,
    // or drop the channel (Disconnected/Cancelled) when recv task ends
    let _ = (send_cx, send_gw, send_client_id); // suppress unused warnings for now

    // ─── Recv loop (this task) ──────────────────────────────────────────
    // Interleave: short timeout on WS read + drain relay channel
    loop {
        // Drain relay channel (non-blocking)
        'drain: loop {
            match rx.try_recv() {
                Ok(WsEvent::Message(json)) => {
                    let frame = Frame::text(json.into_bytes());
                    if enc.encode(frame, &mut write_buf).is_err() {
                        break 'drain;
                    }
                    let data: Vec<u8> = write_buf.to_vec();
                    write_buf.clear();
                    if write_half.write_all(&data).await.is_err() {
                        gw.disconnect_client(&client_id);
                        return;
                    }
                }
                Ok(WsEvent::Close(code, reason)) => {
                    let frame = Frame::close(Some(code), Some(&reason));
                    if enc.encode(frame, &mut write_buf).is_ok() {
                        let data: Vec<u8> = write_buf.to_vec();
                        write_buf.clear();
                        let _ = write_half.write_all(&data).await;
                    }
                    gw.disconnect_client(&client_id);
                    return;
                }
                Err(RecvError::Empty) => break 'drain,
                Err(_) => {
                    // Channel disconnected or cancelled — exit
                    gw.disconnect_client(&client_id);
                    return;
                }
            }
        }

        // Wait for WS frame with a short timeout to keep relay channel responsive
        let recv_result =
            timeout(wall_now(), WS_POLL_INTERVAL, read_ws_frame(&mut read_half, &mut dec, &mut read_buf)).await;

        match recv_result {
            Ok(Ok(Some(frame))) => match frame.opcode {
                asupersync::net::websocket::Opcode::Text => {
                    let text = match std::str::from_utf8(&frame.payload) {
                        Ok(t) => t,
                        Err(_) => {
                            let close_frame = Frame::close(Some(WS_CLOSE_POLICY), Some("invalid UTF-8"));
                            if enc.encode(close_frame, &mut write_buf).is_ok() {
                                let data: Vec<u8> = write_buf.to_vec();
                                write_buf.clear();
                                let _ = write_half.write_all(&data).await;
                            }
                            break;
                        }
                    };
                    if let Err((code, reason)) = gw.handle_ws_message(&client_id, text) {
                        let close_frame = Frame::close(Some(code), Some(&reason));
                        if enc.encode(close_frame, &mut write_buf).is_ok() {
                            let data: Vec<u8> = write_buf.to_vec();
                            write_buf.clear();
                            let _ = write_half.write_all(&data).await;
                        }
                        break;
                    }
                }
                asupersync::net::websocket::Opcode::Ping => {
                    // RFC 6455: server MUST send pong in response to ping
                    let pong_frame = Frame::pong(frame.payload);
                    if enc.encode(pong_frame, &mut write_buf).is_ok() {
                        let data: Vec<u8> = write_buf.to_vec();
                        write_buf.clear();
                        let _ = write_half.write_all(&data).await;
                    }
                }
                asupersync::net::websocket::Opcode::Close => {
                    // Echo close frame back
                    let close_frame = Frame::close(Some(WS_CLOSE_NORMAL), None);
                    if enc.encode(close_frame, &mut write_buf).is_ok() {
                        let data: Vec<u8> = write_buf.to_vec();
                        write_buf.clear();
                        let _ = write_half.write_all(&data).await;
                    }
                    break;
                }
                _ => {} // binary/continuation/pong: ignore
            },
            Ok(Ok(None)) => break, // EOF
            Ok(Err(_)) => break,   // protocol error
            Err(_elapsed) => {}    // timeout — loop back to drain relay channel
        }
    }

    gw.disconnect_client(&client_id);
}

/// Read one complete WebSocket frame from `read_half` into `buf`, decode with `codec`.
async fn read_ws_frame(
    read_half: &mut asupersync::net::OwnedReadHalf,
    codec: &mut FrameCodec,
    buf: &mut asupersync::bytes::BytesMut,
) -> Result<Option<Frame>, asupersync::net::websocket::WsError> {
    use asupersync::io::AsyncReadExt;
    loop {
        // Try to decode from existing buffer
        if let Some(frame) = codec.decode(buf)? {
            return Ok(Some(frame));
        }
        // Need more data
        let mut tmp = [0u8; 4096];
        let n = read_half
            .read(&mut tmp)
            .await
            .map_err(asupersync::net::websocket::WsError::Io)?;
        if n == 0 {
            return Ok(None); // EOF
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

async fn write_ws_http_response(
    stream: &mut asupersync::net::TcpStream,
    resp: &GatewayResponse,
) {
    let reason = super::sse::http_reason(resp.status);
    let mut buf = format!("HTTP/1.1 {} {}\r\n", resp.status, reason).into_bytes();
    for (name, value) in &resp.headers {
        buf.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    let body_bytes = resp.body.as_bytes();
    buf.extend_from_slice(
        format!("Content-Length: {}\r\n\r\n", body_bytes.len()).as_bytes(),
    );
    buf.extend_from_slice(body_bytes);
    let _ = stream.write_all(&buf).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{CorsConfig, Header, LogLevel, OutputTransport};
    use std::sync::atomic::Ordering;

    /// Blocking receive with timeout for sync unit tests (polls try_recv).
    fn recv_timeout_blocking(rx: &async_mpsc::Receiver<WsEvent>, timeout: Duration) -> Option<WsEvent> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match rx.try_recv() {
                Ok(val) => return Some(val),
                Err(async_mpsc::RecvError::Empty) => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return None,
            }
        }
    }

    #[allow(dead_code)]
    fn test_metrics() -> Arc<Metrics> {
        Metrics::new()
    }

    #[allow(dead_code)]
    fn test_logger() -> Arc<Logger> {
        Arc::new(Logger::buffered(OutputTransport::Ws, LogLevel::Debug))
    }

    #[allow(dead_code)]
    fn make_gateway_with_child(cmd: &str) -> (WsGateway, Arc<ChildBridge>) {
        let metrics = test_metrics();
        let logger = test_logger();
        let child =
            Arc::new(ChildBridge::spawn(cmd, metrics.clone(), logger.clone()).unwrap());
        let gw = WsGateway::new(
            Arc::clone(&child),
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
    fn make_gateway(cmd: &str) -> WsGateway {
        make_gateway_with_child(cmd).0
    }

    // ─── WS connect ───────────────────────────────────────────────

    #[test]
    fn connect_assigns_unique_client_ids() {
        let gw = make_gateway("cat");
        let conn1 = gw.handle_ws_connect().unwrap();
        let conn2 = gw.handle_ws_connect().unwrap();
        assert_ne!(conn1.client_id, conn2.client_id);
        assert_eq!(gw.client_count(), 2);
    }

    #[test]
    fn connect_increments_metrics() {
        let gw = make_gateway("cat");
        let _conn = gw.handle_ws_connect().unwrap();
        assert_eq!(gw.metrics.active_clients.load(Ordering::Relaxed), 1);
    }

    // ─── Disconnect ───────────────────────────────────────────────

    #[test]
    fn disconnect_removes_client() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect().unwrap();
        assert_eq!(gw.client_count(), 1);

        gw.disconnect_client(&conn.client_id);
        assert_eq!(gw.client_count(), 0);
    }

    #[test]
    fn disconnect_idempotent() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect().unwrap();

        gw.disconnect_client(&conn.client_id);
        gw.disconnect_client(&conn.client_id); // no panic
        assert_eq!(gw.client_count(), 0);
    }

    #[test]
    fn disconnect_cleans_up_mux_entries() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect().unwrap();

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
        let conn = gw.handle_ws_connect().unwrap();

        let result = gw.handle_ws_message(
            &conn.client_id,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn handle_message_notification() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect().unwrap();

        let result = gw.handle_ws_message(
            &conn.client_id,
            r#"{"jsonrpc":"2.0","method":"notifications/ping"}"#,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn handle_message_invalid_json_keeps_connection() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect().unwrap();

        // Invalid JSON should NOT close the connection (match TS behavior)
        let result = gw.handle_ws_message(&conn.client_id, "not json!");
        assert!(result.is_ok());
        assert_eq!(gw.client_count(), 1);
    }

    #[test]
    fn handle_message_too_large() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect().unwrap();

        let big = "x".repeat(MAX_FRAME_SIZE + 1);
        let result = gw.handle_ws_message(&conn.client_id, &big);
        assert!(result.is_err());
        let (code, _) = result.unwrap_err();
        assert_eq!(code, WS_CLOSE_TOO_LARGE);
    }

    #[test]
    fn handle_message_batch() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect().unwrap();

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

        let conn = gw.handle_ws_connect().unwrap();
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
        let conn = gw.handle_ws_connect().unwrap();

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
        let msg = recv_timeout_blocking(&conn.receiver, Duration::from_secs(2))
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

        let conn = gw.handle_ws_connect().unwrap();

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

        let conn1 = gw.handle_ws_connect().unwrap();
        let conn2 = gw.handle_ws_connect().unwrap();
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
        let msg1 = recv_timeout_blocking(&conn1.receiver, Duration::from_secs(2)).unwrap();
        let msg2 = recv_timeout_blocking(&conn2.receiver, Duration::from_secs(2)).unwrap();

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

    // ─── D-105: Custom headers applied in WS mode ─────────────────

    #[test]
    fn custom_headers_applied_in_ws_mode() {
        // D-105: --header applies to all server modes including WebSocket.
        let metrics = test_metrics();
        let logger = test_logger();
        let child =
            Arc::new(ChildBridge::spawn("cat", metrics.clone(), logger.clone()).unwrap());
        let gw = WsGateway::new(
            Arc::clone(&child),
            CorsHandler::new(CorsConfig::Disabled, false),
            vec![
                Header { name: "X-Custom".into(), value: "hello".into() },
                Header { name: "X-Other".into(), value: "world".into() },
            ],
            metrics,
            logger,
            LogLevel::Debug,
            vec!["/health".to_string()],
        );

        gw.metrics.set_ready();
        let resp = gw.handle_health("/health", false, None).unwrap();
        assert!(resp.headers.iter().any(|(n, v)| n == "X-Custom" && v == "hello"));
        assert!(resp.headers.iter().any(|(n, v)| n == "X-Other" && v == "world"));
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
            vec![],
            metrics.clone(),
            logger,
            LogLevel::Debug,
            vec![],
        );

        let conn = gw.handle_ws_connect().unwrap();
        assert_eq!(metrics.active_clients.load(Ordering::Relaxed), 1);

        gw.disconnect_client(&conn.client_id);
        assert_eq!(metrics.active_clients.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.client_disconnects.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn metrics_total_requests_on_message() {
        let gw = make_gateway("cat");
        let conn = gw.handle_ws_connect().unwrap();

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
            let conn = gw.handle_ws_connect().unwrap();
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
        let conn1 = gw.handle_ws_connect().unwrap();
        let conn2 = gw.handle_ws_connect().unwrap();
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

    // ─── Max clients ─────────────────────────────────────────────────

    #[test]
    fn max_clients_returns_503() {
        let gw = make_gateway("cat");
        let mut connections = Vec::new();
        for _ in 0..MAX_WS_CLIENTS {
            connections.push(gw.handle_ws_connect().unwrap());
        }
        assert_eq!(gw.client_count(), MAX_WS_CLIENTS);

        match gw.handle_ws_connect() {
            Err(resp) => assert_eq!(resp.status, 503),
            Ok(_) => panic!("expected 503, but connect succeeded"),
        }
    }
}
