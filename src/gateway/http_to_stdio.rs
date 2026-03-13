
//! Streamable HTTP → stdio client gateway.
//!
//! Connects to a remote MCP server via Streamable HTTP, bridging stdin/stdout:
//! - POST JSON-RPC messages to remote URL
//! - Response Content-Type `application/json` → single JSON-RPC response → stdout
//! - Response Content-Type `text/event-stream` → parse SSE events → stdout
//! - `Mcp-Session-Id` tracked automatically by HttpClient
//!
//! # Init Dance
//!
//! On first stdin message:
//! - If `initialize` → passthrough with protocol version interception (D-014)
//! - If non-`initialize` → synthetic init with fallback identity (D-004),
//!   absorb init response, send `initialized`, then forward original
//!
//! # Error Normalization
//!
//! - Client-mode error code: -32000 ("Internal error"), NOT -32603
//! - Strip `"MCP error <code>: "` prefix from error messages
//!
//! # Signal Handling
//!
//! DELETE to remote server on SIGINT/SIGTERM to close session cleanly.
//! Exit code 0 on signal.

use std::sync::{Arc, Mutex};

use crate::cli::Header;
use crate::gateway::sse_to_stdio::{
    build_fallback_init, build_initialized_notification, generate_init_id,
    intercept_protocol_version,
};
use crate::jsonrpc::{self, Parsed, RawMessage};
use crate::observe::{Logger, Metrics};

pub use crate::gateway::sse_to_stdio::write_stdout;

// ─── Init Phase ───────────────────────────────────────────────────

/// Tracks the MCP initialization handshake state.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
enum InitPhase {
    /// No messages exchanged yet.
    Pending,
    /// Synthetic init sent, waiting for response. String is the init request ID.
    WaitingSyntheticInit(String),
    /// Passthrough init forwarded, waiting for response. String is the init request ID.
    WaitingPassthroughInit { init_id: String },
    /// Initialization complete, forwarding normally.
    Ready,
}

// ─── Stdin Action ─────────────────────────────────────────────────

/// Action returned by [`HttpToStdioGateway::handle_stdin_message`].
#[derive(Debug)]
#[allow(dead_code)]
pub enum StdinAction {
    /// POST these messages to the server. Pass each response to
    /// [`handle_response_messages`](HttpToStdioGateway::handle_response_messages).
    Post(Vec<RawMessage>),
    /// Synthetic init: POST this init message. Pass response to
    /// [`handle_response_messages`](HttpToStdioGateway::handle_response_messages)
    /// which returns follow-up POSTs (initialized + pending).
    PostInit(RawMessage),
    /// Message buffered during synthetic init wait.
    Buffered,
}

// ─── Response Result ──────────────────────────────────────────────

/// Result of processing a POST response.
#[allow(dead_code)]
pub struct ResponseResult {
    /// Messages to write to stdout.
    pub stdout: Vec<RawMessage>,
    /// Follow-up messages to POST (e.g., initialized + pending after init).
    pub post: Vec<RawMessage>,
}

// ─── Response Type ────────────────────────────────────────────────

/// Response type determined from Content-Type header.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum ResponseType {
    /// `application/json` — single JSON response body.
    Json,
    /// `text/event-stream` — SSE stream of events.
    Sse,
    /// Unknown or missing Content-Type.
    Unknown(String),
}

/// Classify a Content-Type header value.
#[allow(dead_code)]
pub fn classify_content_type(content_type: Option<&str>) -> ResponseType {
    match content_type {
        Some(ct) if ct.contains("application/json") => ResponseType::Json,
        Some(ct) if ct.contains("text/event-stream") => ResponseType::Sse,
        Some(ct) => ResponseType::Unknown(ct.to_owned()),
        None => ResponseType::Unknown(String::new()),
    }
}

// ─── Gateway ──────────────────────────────────────────────────────

/// Streamable HTTP → stdio client gateway.
///
/// Manages the MCP initialization handshake and message routing for
/// Streamable HTTP client mode. The caller is responsible for HTTP
/// transport (use [`HttpClient`](crate::client::http::HttpClient)).
#[allow(dead_code)]
pub struct HttpToStdioGateway {
    /// Remote Streamable HTTP URL for POST.
    url: String,
    /// MCP protocol version for init requests.
    protocol_version: String,
    /// Custom headers applied to outgoing requests.
    #[allow(dead_code)]
    headers: Vec<Header>,
    logger: Arc<Logger>,
    #[allow(dead_code)]
    metrics: Arc<Metrics>,
    /// Init dance state machine.
    init_phase: Mutex<InitPhase>,
    /// Messages queued during synthetic init (forwarded after init completes).
    init_pending: Mutex<Vec<RawMessage>>,
}

#[allow(dead_code)]
impl HttpToStdioGateway {
    /// Create a new Streamable HTTP → stdio gateway.
    #[allow(dead_code)]
    pub fn new(
        url: String,
        protocol_version: String,
        headers: Vec<Header>,
        logger: Arc<Logger>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            url,
            protocol_version,
            headers,
            logger,
            metrics,
            init_phase: Mutex::new(InitPhase::Pending),
            init_pending: Mutex::new(Vec::new()),
        }
    }

    /// The remote server URL.
    #[allow(dead_code)]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Whether initialization is complete.
    #[allow(dead_code)]
    pub fn is_initialized(&self) -> bool {
        *self.init_phase.lock().unwrap() == InitPhase::Ready
    }

    // ─── Stdin handling ─────────────────────────────────────────────

    /// Process a stdin message.
    ///
    /// Returns an action indicating what to do with the message:
    /// - `Post`: POST these messages to the server
    /// - `PostInit`: POST synthetic init, then handle response
    /// - `Buffered`: message queued during init wait
    #[allow(dead_code)]
    pub fn handle_stdin_message(&self, msg: RawMessage) -> StdinAction {
        let mut phase = self.init_phase.lock().unwrap();

        match *phase {
            InitPhase::Pending => {
                if msg.is_initialize_request() {
                    // Passthrough with protocol version interception (D-014).
                    let init_id = msg
                        .id
                        .as_ref()
                        .map(|id| id.get().trim_matches('"').to_owned())
                        .unwrap_or_default();
                    let intercepted =
                        intercept_protocol_version(msg, &self.protocol_version);
                    *phase = InitPhase::WaitingPassthroughInit { init_id };
                    self.logger.info("Stdio → HTTP: passthrough initialize");
                    StdinAction::Post(vec![intercepted])
                } else {
                    // Fallback init (D-004).
                    let init_id = generate_init_id();
                    let init_msg =
                        build_fallback_init(&init_id, &self.protocol_version);
                    *phase = InitPhase::WaitingSyntheticInit(init_id);
                    self.init_pending.lock().unwrap().push(msg);
                    self.logger.info("Stdio → HTTP: fallback init (D-004)");
                    StdinAction::PostInit(init_msg)
                }
            }
            InitPhase::WaitingSyntheticInit(_)
            | InitPhase::WaitingPassthroughInit { .. } => {
                // Buffer until init completes.
                self.init_pending.lock().unwrap().push(msg);
                StdinAction::Buffered
            }
            InitPhase::Ready => {
                StdinAction::Post(vec![msg])
            }
        }
    }

    // ─── Response handling ──────────────────────────────────────────

    /// Process messages from a POST response.
    ///
    /// Call with parsed JSON-RPC messages from the response body,
    /// whether from a JSON response or parsed from SSE events.
    ///
    /// Returns messages for stdout and follow-up messages to POST.
    #[allow(dead_code)]
    pub fn handle_response_messages(
        &self,
        messages: Vec<RawMessage>,
    ) -> ResponseResult {
        let mut phase = self.init_phase.lock().unwrap();

        match *phase {
            InitPhase::WaitingSyntheticInit(ref init_id) => {
                let init_id = init_id.clone();
                let mut stdout = Vec::new();
                let mut found_init_response = false;

                for msg in messages {
                    if !found_init_response && msg.is_response() {
                        if let Some(ref id) = msg.id {
                            if id.get().trim_matches('"') == init_id {
                                // Absorb synthetic init response.
                                self.logger
                                    .debug("HTTP: synthetic init response received");
                                found_init_response = true;
                                continue;
                            }
                        }
                    }
                    stdout.push(msg);
                }

                if found_init_response {
                    *phase = InitPhase::Ready;
                    drop(phase);
                    let mut post = vec![build_initialized_notification()];
                    post.extend(self.init_pending.lock().unwrap().drain(..));
                    return ResponseResult { stdout, post };
                }

                ResponseResult {
                    stdout,
                    post: Vec::new(),
                }
            }
            InitPhase::WaitingPassthroughInit { ref init_id } => {
                let init_id = init_id.clone();
                let mut found_init_response = false;

                for msg in &messages {
                    if msg.is_response() {
                        if let Some(ref id) = msg.id {
                            if id.get().trim_matches('"') == init_id {
                                found_init_response = true;
                                break;
                            }
                        }
                    }
                }

                if found_init_response {
                    *phase = InitPhase::Ready;
                    drop(phase);
                    let mut post = Vec::new();
                    post.extend(self.init_pending.lock().unwrap().drain(..));
                    return ResponseResult {
                        stdout: messages,
                        post,
                    };
                }
                // Not the init response yet — forward but stay in WaitingPassthroughInit.
                ResponseResult {
                    stdout: messages,
                    post: Vec::new(),
                }
            }
            InitPhase::Ready | InitPhase::Pending => ResponseResult {
                stdout: messages,
                post: Vec::new(),
            },
        }
    }

    // ─── Response parsing ───────────────────────────────────────────

    /// Parse a JSON response body into JSON-RPC messages.
    ///
    /// Handles both single messages and batches.
    #[allow(dead_code)]
    pub fn parse_json_response(&self, body: &[u8]) -> Vec<RawMessage> {
        let text = match std::str::from_utf8(body) {
            Ok(s) => s,
            Err(e) => {
                self.logger
                    .error(&format!("HTTP response: invalid UTF-8: {e}"));
                return Vec::new();
            }
        };

        match jsonrpc::parse_line(text) {
            Ok(Parsed::Single(msg)) => vec![msg],
            Ok(Parsed::Batch(msgs)) => msgs,
            Err(e) => {
                self.logger
                    .error(&format!("HTTP response: JSON-RPC parse error: {e}"));
                Vec::new()
            }
        }
    }

    /// Parse an SSE event's data field into JSON-RPC messages.
    ///
    /// Only processes "message" events. Returns empty for other types.
    #[allow(dead_code)]
    pub fn parse_sse_event_data(
        &self,
        event_type: &str,
        data: &str,
    ) -> Vec<RawMessage> {
        if event_type != "message" {
            self.logger
                .debug(&format!("HTTP SSE: ignoring event type '{event_type}'"));
            return Vec::new();
        }

        match jsonrpc::parse_line(data) {
            Ok(Parsed::Single(msg)) => vec![msg],
            Ok(Parsed::Batch(msgs)) => msgs,
            Err(e) => {
                self.logger
                    .error(&format!("HTTP SSE: JSON-RPC parse error: {e}"));
                Vec::new()
            }
        }
    }
}

// ─── B-004: Exit behavior ────────────────────────────────────────────────

/// Handle an HTTP client error per B-004 spec.
///
/// - Connection closed by remote (HTTP 0, empty response, EOF) → `process::exit(1)`
///   for pm2/systemd restart semantics
/// - Transport/network errors → log at error level, return to allow retry
#[allow(dead_code)]
pub fn handle_client_error(err: &crate::client::http::HttpClientError, logger: &Logger) {
    use crate::client::http::HttpClientError;
    match err {
        // EOF / connection reset = remote closed cleanly → exit(1)
        HttpClientError::Io(msg) if msg.contains("EOF") || msg.contains("connection reset") => {
            logger.info("remote HTTP connection closed (B-004), exiting with code 1");
            std::process::exit(1);
        }
        other => {
            logger.error(&format!("HTTP transport error: {other}"));
            // Do NOT exit — allow retry
        }
    }
}

// ─── Entry point ────────────────────────────────────────────────────────

/// Run the Streamable HTTP → stdio client gateway.
#[allow(dead_code)]
pub async fn run(_cx: &asupersync::Cx, config: crate::cli::Config) -> anyhow::Result<()> {
    let logger = Arc::new(Logger::new(config.output_transport, config.log_level));
    let metrics = Metrics::new();

    logger.startup(
        env!("CARGO_PKG_VERSION"),
        &config.input_value,
        &config.output_transport.to_string(),
        config.port,
    );

    let shutdown = crate::signal::install(&logger)?;

    let gw = Arc::new(HttpToStdioGateway::new(
        config.input_value.clone(),
        config.protocol_version,
        config.headers.clone(),
        logger.clone(),
        metrics,
    ));

    let custom_headers: Vec<(String, String)> = config.headers.iter()
        .map(|h| (h.name.clone(), h.value.clone()))
        .collect();

    // Track Mcp-Session-Id for stateful sessions
    let session_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Stdin reader loop: read JSON-RPC, POST to remote, write responses to stdout
    use std::io::BufRead;
    let stdin_handle = {
        let gw = Arc::clone(&gw);
        let session_id = Arc::clone(&session_id);
        let custom_headers = custom_headers.clone();
        let url = config.input_value.clone();

        std::thread::Builder::new()
            .name("http-stdin-reader".into())
            .spawn(move || {
                let stdin = std::io::stdin();
                for line in stdin.lock().lines() {
                    let line = match line {
                        Ok(l) => l,
                        Err(_) => break,
                    };
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }

                    let msg = match crate::jsonrpc::parse_line(&line) {
                        Ok(crate::jsonrpc::Parsed::Single(m)) => m,
                        Ok(crate::jsonrpc::Parsed::Batch(_)) => continue,
                        Err(_) => continue,
                    };

                    let to_post = match gw.handle_stdin_message(msg) {
                        StdinAction::Post(msgs) => msgs,
                        StdinAction::PostInit(msg) => vec![msg],
                        StdinAction::Buffered => continue,
                    };

                    // Build headers including session ID if available
                    let mut req_headers = custom_headers.clone();
                    if let Some(ref sid) = *session_id.lock().unwrap() {
                        req_headers.push(("Mcp-Session-Id".to_string(), sid.clone()));
                    }

                    for msg in to_post {
                        let body = match serde_json::to_vec(&msg) {
                            Ok(b) => b,
                            Err(_) => continue,
                        };

                        match crate::serve::http_post(&url, &req_headers, &body) {
                            Ok(resp) => {
                                // Track Mcp-Session-Id from response
                                if let Some(sid) = resp.header("mcp-session-id") {
                                    *session_id.lock().unwrap() = Some(sid.to_string());
                                    // Update req_headers for this batch
                                    let sid = sid.to_string();
                                    req_headers.retain(|(k, _)| k.to_lowercase() != "mcp-session-id");
                                    req_headers.push(("Mcp-Session-Id".to_string(), sid));
                                }

                                // Parse response body into messages
                                let ct = resp.header("content-type").unwrap_or("").to_string();
                                let response_type = classify_content_type(Some(&ct));

                                let messages = match response_type {
                                    ResponseType::Json => gw.parse_json_response(&resp.body),
                                    ResponseType::Sse => {
                                        // Parse SSE events from body
                                        let text = match std::str::from_utf8(&resp.body) {
                                            Ok(s) => s.to_string(),
                                            Err(_) => continue,
                                        };
                                        let mut parser = crate::client::sse::SseParser::new();
                                        let events = parser.feed(&text);
                                        let mut msgs = Vec::new();
                                        for event in &events {
                                            msgs.extend(gw.parse_sse_event_data(
                                                &event.event_type,
                                                &event.data,
                                            ));
                                        }
                                        msgs
                                    }
                                    ResponseType::Unknown(_) => {
                                        if resp.status == 202 || resp.status == 204 {
                                            // Accepted notification — no response body expected
                                            Vec::new()
                                        } else {
                                            gw.parse_json_response(&resp.body)
                                        }
                                    }
                                };

                                let result = gw.handle_response_messages(messages);
                                for out_msg in result.stdout {
                                    let _ = write_stdout(&out_msg);
                                }

                                // Follow-up POSTs (initialized + pending)
                                for follow_up in result.post {
                                    let body = match serde_json::to_vec(&follow_up) {
                                        Ok(b) => b,
                                        Err(_) => continue,
                                    };
                                    match crate::serve::http_post(&url, &req_headers, &body) {
                                        Ok(resp2) => {
                                            let ct2 = resp2.header("content-type").unwrap_or("").to_string();
                                            let rt2 = classify_content_type(Some(&ct2));
                                            let msgs2 = match rt2 {
                                                ResponseType::Json => gw.parse_json_response(&resp2.body),
                                                ResponseType::Sse => {
                                                    let text = match std::str::from_utf8(&resp2.body) {
                                                        Ok(s) => s.to_string(),
                                                        Err(_) => String::new(),
                                                    };
                                                    let mut parser2 = crate::client::sse::SseParser::new();
                                                    let events2 = parser2.feed(&text);
                                                    let mut m2 = Vec::new();
                                                    for ev in &events2 {
                                                        m2.extend(gw.parse_sse_event_data(&ev.event_type, &ev.data));
                                                    }
                                                    m2
                                                }
                                                _ => Vec::new(),
                                            };
                                            let r2 = gw.handle_response_messages(msgs2);
                                            for out_msg in r2.stdout {
                                                let _ = write_stdout(&out_msg);
                                            }
                                        }
                                        Err(e) => {
                                            handle_client_error(&crate::client::http::HttpClientError::Io(e.to_string()), &gw.logger);
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                handle_client_error(&crate::client::http::HttpClientError::Io(e.to_string()), &gw.logger);
                            }
                        }
                    }
                }
            })
            .expect("spawn http-stdin-reader thread")
    };

    // Block until signal (SIGINT/SIGTERM/SIGHUP)
    let sig = shutdown.wait();
    logger.info(&format!("received {}, shutting down", crate::signal::signal_name(sig)));

    // D-016: exit code 0 on signal
    // Optionally send DELETE to close session
    if let Some(sid) = session_id.lock().unwrap().clone() {
        let mut del_headers = custom_headers.clone();
        del_headers.push(("Mcp-Session-Id".to_string(), sid));
        // Send DELETE (best effort)
        let _ = crate::serve::http_delete(&config.input_value, &del_headers);
    }

    drop(stdin_handle);
    std::process::exit(0);
}

// ─── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{LogLevel, OutputTransport};
    use crate::gateway::sse_to_stdio::{extract_error_code, make_error_response, CLIENT_ERROR_CODE, CLIENT_ERROR_MESSAGE};
    use serde_json::value::RawValue;

    #[allow(dead_code)]
    fn test_logger() -> Arc<Logger> {
        Arc::new(Logger::buffered(OutputTransport::Stdio, LogLevel::Debug))
    }

    #[allow(dead_code)]
    fn test_metrics() -> Arc<Metrics> {
        Metrics::new()
    }

    #[allow(dead_code)]
    fn make_gateway() -> HttpToStdioGateway {
        HttpToStdioGateway::new(
            "http://localhost:8080/mcp".into(),
            "2024-11-05".into(),
            Vec::new(),
            test_logger(),
            test_metrics(),
        )
    }

    #[allow(dead_code)]
    fn raw(s: &str) -> Box<RawValue> {
        RawValue::from_string(s.into()).unwrap()
    }

    // ─── classify_content_type ──────────────────────────────────────

    #[test]
    fn classify_json() {
        assert_eq!(
            classify_content_type(Some("application/json")),
            ResponseType::Json
        );
        assert_eq!(
            classify_content_type(Some("application/json; charset=utf-8")),
            ResponseType::Json
        );
    }

    #[test]
    fn classify_sse() {
        assert_eq!(
            classify_content_type(Some("text/event-stream")),
            ResponseType::Sse
        );
    }

    #[test]
    fn classify_unknown() {
        assert!(matches!(
            classify_content_type(Some("text/plain")),
            ResponseType::Unknown(_)
        ));
        assert!(matches!(
            classify_content_type(None),
            ResponseType::Unknown(_)
        ));
    }

    // ─── Gateway construction ───────────────────────────────────────

    #[test]
    fn gateway_url() {
        let gw = make_gateway();
        assert_eq!(gw.url(), "http://localhost:8080/mcp");
    }

    #[test]
    fn not_initialized_initially() {
        let gw = make_gateway();
        assert!(!gw.is_initialized());
    }

    // ─── handle_stdin_message: first message is init ────────────────

    #[test]
    fn stdin_init_passthrough() {
        let gw = make_gateway();
        let msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("0")),
            method: Some("initialize".into()),
            params: Some(
                serde_json::value::to_raw_value(&serde_json::json!({
                    "protocolVersion": "2024-01-01",
                    "capabilities": {}
                }))
                .unwrap(),
            ),
            result: None,
            error: None,
            ..Default::default()
        };
        let action = gw.handle_stdin_message(msg);

        match action {
            StdinAction::Post(msgs) => {
                assert_eq!(msgs.len(), 1);
                assert!(msgs[0].is_initialize_request());
                let params: serde_json::Value =
                    serde_json::from_str(msgs[0].params.as_ref().unwrap().get())
                        .unwrap();
                // Protocol version intercepted (D-014).
                assert_eq!(params["protocolVersion"], "2024-11-05");
            }
            _ => panic!("expected Post action"),
        }
        let phase = gw.init_phase.lock().unwrap();
        match &*phase {
            InitPhase::WaitingPassthroughInit { init_id } => {
                assert_eq!(init_id, "0");
            }
            other => panic!("expected WaitingPassthroughInit, got {:?}", other),
        }
    }

    // ─── handle_stdin_message: first message is non-init ────────────

    #[test]
    fn stdin_fallback_init() {
        let gw = make_gateway();
        let msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("1")),
            method: Some("tools/list".into()),
            params: None,
            result: None,
            error: None,
            ..Default::default()
        };
        let action = gw.handle_stdin_message(msg);

        match action {
            StdinAction::PostInit(init_msg) => {
                assert!(init_msg.is_initialize_request());
                let params: serde_json::Value =
                    serde_json::from_str(init_msg.params.as_ref().unwrap().get())
                        .unwrap();
                assert_eq!(params["protocolVersion"], "2024-11-05");
                assert_eq!(params["clientInfo"]["name"], "supergateway");
            }
            _ => panic!("expected PostInit action"),
        }
        assert_eq!(gw.init_pending.lock().unwrap().len(), 1);
    }

    // ─── handle_stdin_message: buffered during init ─────────────────

    #[test]
    fn stdin_buffered_during_init() {
        let gw = make_gateway();
        *gw.init_phase.lock().unwrap() =
            InitPhase::WaitingSyntheticInit("some-id".into());

        let msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("2")),
            method: Some("tools/call".into()),
            params: None,
            result: None,
            error: None,
            ..Default::default()
        };
        let action = gw.handle_stdin_message(msg);
        assert!(matches!(action, StdinAction::Buffered));
        assert_eq!(gw.init_pending.lock().unwrap().len(), 1);
    }

    // ─── handle_stdin_message: forwarded when ready ─────────────────

    #[test]
    fn stdin_forward_when_ready() {
        let gw = make_gateway();
        *gw.init_phase.lock().unwrap() = InitPhase::Ready;

        let msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("3")),
            method: Some("tools/list".into()),
            params: None,
            result: None,
            error: None,
            ..Default::default()
        };
        let action = gw.handle_stdin_message(msg);

        match action {
            StdinAction::Post(msgs) => {
                assert_eq!(msgs.len(), 1);
                assert_eq!(msgs[0].method_str(), Some("tools/list"));
            }
            _ => panic!("expected Post action"),
        }
    }

    // ─── handle_response_messages: passthrough init ─────────────────

    #[test]
    fn response_passthrough_init() {
        let gw = make_gateway();
        *gw.init_phase.lock().unwrap() = InitPhase::WaitingPassthroughInit {
            init_id: "0".into(),
        };

        let resp_msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("0")),
            method: None,
            params: None,
            result: Some(
                serde_json::value::to_raw_value(&serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {}
                }))
                .unwrap(),
            ),
            error: None,
            ..Default::default()
        };

        let result = gw.handle_response_messages(vec![resp_msg]);
        // Response forwarded to stdout (passthrough).
        assert_eq!(result.stdout.len(), 1);
        assert!(result.stdout[0].is_response());
        assert!(result.post.is_empty());
        assert!(gw.is_initialized());
    }

    // ─── handle_response_messages: synthetic init absorbed ──────────

    #[test]
    fn response_synthetic_init_absorbed() {
        let gw = make_gateway();
        *gw.init_phase.lock().unwrap() =
            InitPhase::WaitingSyntheticInit("test-init-id".into());

        // Queue a pending message.
        gw.init_pending.lock().unwrap().push(RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("42")),
            method: Some("tools/list".into()),
            params: None,
            result: None,
            error: None,
            ..Default::default()
        });

        let resp_msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(RawValue::from_string("\"test-init-id\"".into()).unwrap()),
            method: None,
            params: None,
            result: Some(
                serde_json::value::to_raw_value(&serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {}
                }))
                .unwrap(),
            ),
            error: None,
            ..Default::default()
        };

        let result = gw.handle_response_messages(vec![resp_msg]);
        // Init response absorbed — not in stdout.
        assert!(result.stdout.is_empty());
        // Follow-up: initialized + pending.
        assert_eq!(result.post.len(), 2);
        assert_eq!(
            result.post[0].method_str(),
            Some("notifications/initialized")
        );
        assert_eq!(result.post[1].method_str(), Some("tools/list"));
        assert!(gw.is_initialized());
    }

    // ─── handle_response_messages: ready mode ───────────────────────

    #[test]
    fn response_forwarded_when_ready() {
        let gw = make_gateway();
        *gw.init_phase.lock().unwrap() = InitPhase::Ready;

        let resp_msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("1")),
            method: None,
            params: None,
            result: Some(
                serde_json::value::to_raw_value(&serde_json::json!({"tools": []}))
                    .unwrap(),
            ),
            error: None,
            ..Default::default()
        };

        let result = gw.handle_response_messages(vec![resp_msg]);
        assert_eq!(result.stdout.len(), 1);
        assert!(result.stdout[0].is_response());
        assert!(result.post.is_empty());
    }

    // ─── handle_response_messages: notification during init ─────────

    #[test]
    fn response_notification_during_synthetic_init() {
        let gw = make_gateway();
        *gw.init_phase.lock().unwrap() =
            InitPhase::WaitingSyntheticInit("test-init-id".into());

        // Server sends a notification (not the init response).
        let notif = RawMessage {
            jsonrpc: "2.0".into(),
            id: None,
            method: Some("notifications/progress".into()),
            params: Some(
                serde_json::value::to_raw_value(&serde_json::json!({"token": "x"}))
                    .unwrap(),
            ),
            result: None,
            error: None,
            ..Default::default()
        };

        let result = gw.handle_response_messages(vec![notif]);
        // Notification forwarded to stdout.
        assert_eq!(result.stdout.len(), 1);
        assert!(result.stdout[0].is_notification());
        // Still waiting for init.
        assert!(!gw.is_initialized());
    }

    // ─── parse_json_response ────────────────────────────────────────

    #[test]
    fn parse_json_single() {
        let gw = make_gateway();
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let msgs = gw.parse_json_response(body);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].is_response());
    }

    #[test]
    fn parse_json_batch() {
        let gw = make_gateway();
        let body =
            br#"[{"jsonrpc":"2.0","id":1,"result":{}},{"jsonrpc":"2.0","method":"notify"}]"#;
        let msgs = gw.parse_json_response(body);
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].is_response());
        assert!(msgs[1].is_notification());
    }

    #[test]
    fn parse_json_invalid() {
        let gw = make_gateway();
        let msgs = gw.parse_json_response(b"not json");
        assert!(msgs.is_empty());
    }

    #[test]
    fn parse_json_invalid_utf8() {
        let gw = make_gateway();
        let msgs = gw.parse_json_response(&[0xff, 0xfe]);
        assert!(msgs.is_empty());
    }

    // ─── parse_sse_event_data ───────────────────────────────────────

    #[test]
    fn parse_sse_message_event() {
        let gw = make_gateway();
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let msgs = gw.parse_sse_event_data("message", json);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].is_response());
    }

    #[test]
    fn parse_sse_non_message_ignored() {
        let gw = make_gateway();
        let msgs = gw.parse_sse_event_data("endpoint", "/message");
        assert!(msgs.is_empty());
    }

    #[test]
    fn parse_sse_invalid_json() {
        let gw = make_gateway();
        let msgs = gw.parse_sse_event_data("message", "not json");
        assert!(msgs.is_empty());
    }

    // ─── Full init dance: passthrough ───────────────────────────────

    #[test]
    fn full_passthrough_init_dance() {
        let gw = make_gateway();

        // Step 1: Init message from stdin.
        let init_msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("0")),
            method: Some("initialize".into()),
            params: Some(
                serde_json::value::to_raw_value(&serde_json::json!({
                    "protocolVersion": "2024-01-01",
                    "capabilities": { "roots": { "listChanged": true } }
                }))
                .unwrap(),
            ),
            result: None,
            error: None,
            ..Default::default()
        };
        let action = gw.handle_stdin_message(init_msg);
        match action {
            StdinAction::Post(msgs) => {
                assert_eq!(msgs.len(), 1);
                let params: serde_json::Value =
                    serde_json::from_str(msgs[0].params.as_ref().unwrap().get())
                        .unwrap();
                assert_eq!(params["protocolVersion"], "2024-11-05");
                // Other params preserved.
                assert!(params["capabilities"]["roots"]["listChanged"]
                    .as_bool()
                    .unwrap());
            }
            _ => panic!("expected Post"),
        }

        // Step 2: Server JSON response.
        let resp_msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("0")),
            method: None,
            params: None,
            result: Some(
                serde_json::value::to_raw_value(&serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {}
                }))
                .unwrap(),
            ),
            error: None,
            ..Default::default()
        };
        let result = gw.handle_response_messages(vec![resp_msg]);
        assert_eq!(result.stdout.len(), 1);
        assert!(result.stdout[0].is_response());
        assert!(result.post.is_empty());
        assert!(gw.is_initialized());

        // Step 3: Normal message forwarded.
        let msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("1")),
            method: Some("tools/list".into()),
            params: None,
            result: None,
            error: None,
            ..Default::default()
        };
        let action = gw.handle_stdin_message(msg);
        match action {
            StdinAction::Post(msgs) => {
                assert_eq!(msgs.len(), 1);
                assert_eq!(msgs[0].method_str(), Some("tools/list"));
            }
            _ => panic!("expected Post"),
        }
    }

    // ─── Full init dance: synthetic ─────────────────────────────────

    #[test]
    fn full_synthetic_init_dance() {
        let gw = make_gateway();

        // Step 1: Non-init stdin message arrives.
        let stdin_msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("1")),
            method: Some("tools/list".into()),
            params: None,
            result: None,
            error: None,
            ..Default::default()
        };
        let action = gw.handle_stdin_message(stdin_msg);
        let init_msg = match action {
            StdinAction::PostInit(m) => m,
            _ => panic!("expected PostInit"),
        };
        let init_id = init_msg
            .id
            .as_ref()
            .unwrap()
            .get()
            .trim_matches('"')
            .to_owned();

        // Step 2: Second stdin message buffered.
        let stdin_msg2 = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("2")),
            method: Some("resources/list".into()),
            params: None,
            result: None,
            error: None,
            ..Default::default()
        };
        let action2 = gw.handle_stdin_message(stdin_msg2);
        assert!(matches!(action2, StdinAction::Buffered));

        // Step 3: Server init response.
        let resp_msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(RawValue::from_string(format!("\"{init_id}\"")).unwrap()),
            method: None,
            params: None,
            result: Some(
                serde_json::value::to_raw_value(&serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {}
                }))
                .unwrap(),
            ),
            error: None,
            ..Default::default()
        };
        let result = gw.handle_response_messages(vec![resp_msg]);

        // Init response absorbed.
        assert!(result.stdout.is_empty());
        // Follow-up: initialized + 2 pending messages.
        assert_eq!(result.post.len(), 3);
        assert_eq!(
            result.post[0].method_str(),
            Some("notifications/initialized")
        );
        assert_eq!(result.post[1].method_str(), Some("tools/list"));
        assert_eq!(result.post[2].method_str(), Some("resources/list"));
        assert!(gw.is_initialized());

        // Step 4: Subsequent responses forwarded normally.
        let resp_msg2 = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("1")),
            method: None,
            params: None,
            result: Some(
                serde_json::value::to_raw_value(&serde_json::json!({"tools": []}))
                    .unwrap(),
            ),
            error: None,
            ..Default::default()
        };
        let result2 = gw.handle_response_messages(vec![resp_msg2]);
        assert_eq!(result2.stdout.len(), 1);
        assert!(result2.post.is_empty());
    }

    // ─── Re-exported utilities ──────────────────────────────────────

    #[test]
    fn error_response_accessible() {
        let resp = make_error_response(Some(raw("5")), "MCP error -32600: Invalid", None);
        assert!(resp.is_response());
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("-32000")); // CLIENT_ERROR_CODE
        assert!(s.contains("Invalid")); // Prefix stripped
    }

    #[test]
    fn error_response_preserves_code() {
        let resp = make_error_response(Some(raw("5")), "Invalid", Some(-32600));
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("-32600")); // Original code preserved
        assert!(!s.contains("-32000"));
    }
}
