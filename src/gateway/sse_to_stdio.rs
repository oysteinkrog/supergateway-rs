// Public API — suppress dead_code until wired up in main.rs.
#![allow(dead_code)]

//! SSE → stdio client gateway.
//!
//! Connects to a remote MCP server via SSE, bridging stdin/stdout:
//! - GET SSE URL → event stream. First `endpoint` event gives POST URL.
//! - `message` SSE events → JSON-RPC messages → stdout
//! - stdin JSON-RPC messages → POST to endpoint URL
//!
//! # Init Dance
//!
//! On first stdin message:
//! - If `initialize` → passthrough with protocol version interception (D-014)
//! - If non-`initialize` → synthetic init with fallback identity (D-004),
//!   wait for init response, send `initialized`, then forward original
//!
//! # Error Normalization
//!
//! - Client-mode error code: -32000 ("Internal error"), NOT -32603
//! - Strip `"MCP error <code>: "` prefix from error messages
//!
//! # Reconnection (D-013)
//!
//! Exponential backoff via [`ReconnectState`](crate::client::sse::ReconnectState),
//! 256-message outgoing buffer during disconnect via
//! [`OutgoingBuffer`](crate::client::sse::OutgoingBuffer).
//!
//! # Signal Handling (D-016)
//!
//! Exit code 0 on signal. No explicit transport close.

use std::sync::{Arc, Mutex};

use serde_json::value::RawValue;

use crate::cli::Header;
use crate::client::sse::{resolve_endpoint_url, SseEvent};
use crate::jsonrpc::{self, Parsed, RawMessage};
use crate::observe::{Logger, Metrics};

// ─── Constants ─────────────────────────────────────────────────────

/// Fallback client name for synthetic init (D-004).
const FALLBACK_CLIENT_NAME: &str = "supergateway";

/// Error fallback code for client-mode errors.
pub const CLIENT_ERROR_CODE: i32 = -32000;

/// Error fallback message.
pub const CLIENT_ERROR_MESSAGE: &str = "Internal error";

// ─── Init Phase ───────────────────────────────────────────────────

/// Tracks the MCP initialization handshake state.
#[derive(Debug, Clone, PartialEq)]
enum InitPhase {
    /// No messages exchanged yet.
    Pending,
    /// Synthetic init sent, waiting for response. String is the init request ID.
    WaitingSyntheticInit(String),
    /// Passthrough init forwarded, waiting for response.
    WaitingPassthroughInit,
    /// Initialization complete, forwarding normally.
    Ready,
}

// ─── SSE Event Result ─────────────────────────────────────────────

/// Result of processing an SSE event.
pub struct SseEventResult {
    /// Messages to write to stdout.
    pub stdout: Vec<RawMessage>,
    /// Messages to POST to the endpoint (e.g., initialized notification +
    /// pending messages after synthetic init completes).
    pub post: Vec<RawMessage>,
}

// ─── Stdin Action ─────────────────────────────────────────────────

/// Action returned by [`SseToStdioGateway::handle_stdin_message`].
#[derive(Debug)]
pub enum StdinAction {
    /// POST these messages to the endpoint.
    Post(Vec<RawMessage>),
    /// Synthetic init dance: POST this init message, then wait for SSE response.
    /// After response, [`handle_sse_event`](SseToStdioGateway::handle_sse_event)
    /// will return the follow-up POSTs.
    PostInit(RawMessage),
    /// Message buffered internally (during synthetic init wait).
    Buffered,
}

// ─── Gateway ──────────────────────────────────────────────────────

/// SSE→stdio client gateway.
///
/// Connects to a remote MCP server via SSE, bridging stdin/stdout.
pub struct SseToStdioGateway {
    /// Remote SSE server URL (for GET connection).
    sse_url: String,
    /// MCP protocol version for init requests.
    protocol_version: String,
    /// Custom headers applied to both SSE GET and endpoint POST.
    #[allow(dead_code)]
    headers: Vec<Header>,
    logger: Arc<Logger>,
    #[allow(dead_code)]
    metrics: Arc<Metrics>,
    /// Discovered POST endpoint URL (from SSE `endpoint` event).
    endpoint_url: Mutex<Option<String>>,
    /// Init dance state machine.
    init_phase: Mutex<InitPhase>,
    /// Messages queued during synthetic init (forwarded after init completes).
    init_pending: Mutex<Vec<RawMessage>>,
}

impl SseToStdioGateway {
    /// Create a new SSE→stdio gateway.
    pub fn new(
        sse_url: String,
        protocol_version: String,
        headers: Vec<Header>,
        logger: Arc<Logger>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            sse_url,
            protocol_version,
            headers,
            logger,
            metrics,
            endpoint_url: Mutex::new(None),
            init_phase: Mutex::new(InitPhase::Pending),
            init_pending: Mutex::new(Vec::new()),
        }
    }

    /// Get the discovered endpoint URL.
    pub fn endpoint_url(&self) -> Option<String> {
        self.endpoint_url.lock().unwrap().clone()
    }

    /// Whether initialization is complete.
    pub fn is_initialized(&self) -> bool {
        *self.init_phase.lock().unwrap() == InitPhase::Ready
    }

    /// Process an SSE event from the remote server.
    ///
    /// Returns messages for stdout and messages to POST to the endpoint.
    pub fn handle_sse_event(&self, event: &SseEvent) -> SseEventResult {
        // Handle endpoint discovery.
        if event.is_endpoint() {
            let url = resolve_endpoint_url(&self.sse_url, &event.data);
            self.logger
                .info(&format!("SSE → Stdio: endpoint discovered: {url}"));
            *self.endpoint_url.lock().unwrap() = Some(url);
            return SseEventResult {
                stdout: Vec::new(),
                post: Vec::new(),
            };
        }

        // Only process "message" events.
        if event.event_type != "message" {
            self.logger
                .debug(&format!("SSE: ignoring event type '{}'", event.event_type));
            return SseEventResult {
                stdout: Vec::new(),
                post: Vec::new(),
            };
        }

        // Parse JSON-RPC from event data.
        let messages = match jsonrpc::parse_line(&event.data) {
            Ok(Parsed::Single(msg)) => vec![msg],
            Ok(Parsed::Batch(msgs)) => msgs,
            Err(e) => {
                self.logger
                    .error(&format!("SSE: JSON-RPC parse error: {e}"));
                return SseEventResult {
                    stdout: Vec::new(),
                    post: Vec::new(),
                };
            }
        };

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
                                // Absorb synthetic init response — don't forward.
                                self.logger.debug("SSE: synthetic init response received");
                                found_init_response = true;
                                continue;
                            }
                        }
                    }
                    // Forward other messages (e.g., server notifications).
                    stdout.push(msg);
                }

                if found_init_response {
                    *phase = InitPhase::Ready;
                    drop(phase);
                    // Build follow-up: initialized notification + pending messages.
                    let mut post = vec![build_initialized_notification()];
                    post.extend(self.init_pending.lock().unwrap().drain(..));
                    return SseEventResult { stdout, post };
                }

                SseEventResult {
                    stdout,
                    post: Vec::new(),
                }
            }
            InitPhase::WaitingPassthroughInit => {
                let has_response = messages.iter().any(|m| m.is_response());
                if has_response {
                    *phase = InitPhase::Ready;
                }
                // Passthrough: forward all messages to stdout.
                SseEventResult {
                    stdout: messages,
                    post: Vec::new(),
                }
            }
            InitPhase::Ready | InitPhase::Pending => SseEventResult {
                stdout: messages,
                post: Vec::new(),
            },
        }
    }

    /// Process a stdin message.
    ///
    /// Returns an action indicating what to do with the message.
    pub fn handle_stdin_message(&self, msg: RawMessage) -> StdinAction {
        let mut phase = self.init_phase.lock().unwrap();

        match *phase {
            InitPhase::Pending => {
                if msg.is_initialize_request() {
                    // Passthrough with protocol version interception (D-014).
                    let intercepted =
                        intercept_protocol_version(msg, &self.protocol_version);
                    *phase = InitPhase::WaitingPassthroughInit;
                    self.logger.info("Stdio → SSE: passthrough initialize");
                    StdinAction::Post(vec![intercepted])
                } else {
                    // Fallback init (D-004).
                    let init_id = generate_init_id();
                    let init_msg =
                        build_fallback_init(&init_id, &self.protocol_version);
                    *phase = InitPhase::WaitingSyntheticInit(init_id);
                    self.init_pending.lock().unwrap().push(msg);
                    self.logger.info("Stdio → SSE: fallback init (D-004)");
                    StdinAction::PostInit(init_msg)
                }
            }
            InitPhase::WaitingSyntheticInit(_) => {
                // Buffer until init completes.
                self.init_pending.lock().unwrap().push(msg);
                StdinAction::Buffered
            }
            InitPhase::WaitingPassthroughInit | InitPhase::Ready => {
                StdinAction::Post(vec![msg])
            }
        }
    }
}

// ─── Pure Functions ───────────────────────────────────────────────

/// Strip `"MCP error <code>: "` prefix from error message.
///
/// Pattern: `/^MCP error -?\d+: /`
///
/// Example: `"MCP error -32600: Invalid request"` → `"Invalid request"`
pub fn strip_error_prefix(message: &str) -> &str {
    let rest = match message.strip_prefix("MCP error ") {
        Some(r) => r,
        None => return message,
    };
    // Optional minus sign.
    let start = if rest.starts_with('-') { 1 } else { 0 };
    // One or more digits.
    let bytes = rest.as_bytes();
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    // Must have at least one digit.
    if i == start {
        return message;
    }
    // Must be followed by ": ".
    if rest[i..].starts_with(": ") {
        &rest[i + 2..]
    } else {
        message
    }
}

/// Build a fallback initialize request (D-004).
///
/// Used when the first stdin message is NOT an `initialize` request.
/// Default identity: `name="supergateway"`, `version=<crate version>`.
/// Default capabilities: `{ "roots": { "listChanged": true }, "sampling": {} }`.
pub fn build_fallback_init(id: &str, protocol_version: &str) -> RawMessage {
    let params = serde_json::json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "roots": { "listChanged": true },
            "sampling": {}
        },
        "clientInfo": {
            "name": FALLBACK_CLIENT_NAME,
            "version": env!("CARGO_PKG_VERSION")
        }
    });
    let params_raw =
        serde_json::value::to_raw_value(&params).expect("fallback init params serialization");
    let id_raw =
        RawValue::from_string(format!("\"{id}\"")).expect("init id raw value");
    RawMessage {
        jsonrpc: "2.0".into(),
        id: Some(id_raw),
        method: Some("initialize".into()),
        params: Some(params_raw),
        result: None,
        error: None,
        ..Default::default()
    }
}

/// Build a `notifications/initialized` notification.
pub fn build_initialized_notification() -> RawMessage {
    RawMessage {
        jsonrpc: "2.0".into(),
        id: None,
        method: Some("notifications/initialized".into()),
        params: None,
        result: None,
        error: None,
        ..Default::default()
    }
}

/// Generate a unique init ID: `init_<timestamp_ms>_<random_base36_9chars>`.
pub fn generate_init_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let rand = random_base36(9);
    format!("init_{ts}_{rand}")
}

/// Generate a random base-36 string of given length using `/dev/urandom`.
fn random_base36(len: usize) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = vec![0u8; len];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    }
    buf.iter()
        .map(|b| CHARS[(*b as usize) % CHARS.len()] as char)
        .collect()
}

/// Intercept and modify the protocol version in an initialize request (D-014).
///
/// Replaces `params.protocolVersion` with the configured version.
pub fn intercept_protocol_version(msg: RawMessage, version: &str) -> RawMessage {
    if !msg.is_initialize_request() {
        return msg;
    }
    let Some(ref params_raw) = msg.params else {
        return msg;
    };
    let Ok(mut params) = serde_json::from_str::<serde_json::Value>(params_raw.get()) else {
        return msg;
    };
    if let Some(obj) = params.as_object_mut() {
        obj.insert(
            "protocolVersion".into(),
            serde_json::Value::String(version.into()),
        );
        if let Ok(new_params) = serde_json::value::to_raw_value(&params) {
            return RawMessage {
                params: Some(new_params),
                ..msg
            };
        }
    }
    msg
}

/// Extract the error code from a JSON-RPC error response's `error` field.
///
/// Parses the opaque `error` RawValue looking for `{"code": <number>}`.
/// Returns `None` if the message has no error field or code can't be parsed.
pub fn extract_error_code(msg: &RawMessage) -> Option<i32> {
    let error_raw = msg.error.as_ref()?;
    let error_val: serde_json::Value = serde_json::from_str(error_raw.get()).ok()?;
    error_val.get("code")?.as_i64().map(|c| c as i32)
}

/// Build a client-mode error response.
///
/// When `code` is `Some`, uses that error code (preserving the original).
/// When `None`, defaults to -32000 (NOT -32603). Strips "MCP error <code>:"
/// prefix from the message. Falls back to "Internal error" if message is empty.
pub fn make_error_response(
    id: Option<Box<RawValue>>,
    message: &str,
    code: Option<i32>,
) -> RawMessage {
    let stripped = strip_error_prefix(message);
    let msg = if stripped.is_empty() {
        CLIENT_ERROR_MESSAGE
    } else {
        stripped
    };
    RawMessage::error_response(id, code.unwrap_or(CLIENT_ERROR_CODE), msg)
}

/// Write a JSON-RPC message to stdout as a newline-delimited JSON line.
pub fn write_stdout(msg: &RawMessage) -> std::io::Result<()> {
    use std::io::Write;
    let json = serde_json::to_string(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(json.as_bytes())?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

// ─── Entry point ────────────────────────────────────────────────────────

/// Run the SSE → stdio client gateway.
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

    let _gw = SseToStdioGateway::new(
        config.input_value,
        config.protocol_version,
        config.headers,
        logger,
        metrics,
    );

    // TODO: Wire up SSE client connection + stdin/stdout bridge (upcoming bead)
    anyhow::bail!("SSE->stdio client not yet implemented")
}

// ─── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{LogLevel, OutputTransport};

    fn test_logger() -> Arc<Logger> {
        Arc::new(Logger::buffered(OutputTransport::Stdio, LogLevel::Debug))
    }

    fn test_metrics() -> Arc<Metrics> {
        Metrics::new()
    }

    fn make_gateway() -> SseToStdioGateway {
        SseToStdioGateway::new(
            "http://localhost:8080/sse".into(),
            "2024-11-05".into(),
            Vec::new(),
            test_logger(),
            test_metrics(),
        )
    }

    fn raw(s: &str) -> Box<RawValue> {
        RawValue::from_string(s.into()).unwrap()
    }

    // ─── strip_error_prefix ───────────────────────────────────────

    #[test]
    fn strip_prefix_negative_code() {
        assert_eq!(
            strip_error_prefix("MCP error -32600: Invalid request"),
            "Invalid request"
        );
    }

    #[test]
    fn strip_prefix_positive_code() {
        assert_eq!(
            strip_error_prefix("MCP error 500: Server error"),
            "Server error"
        );
    }

    #[test]
    fn strip_prefix_no_match_missing_prefix() {
        assert_eq!(
            strip_error_prefix("Some other error"),
            "Some other error"
        );
    }

    #[test]
    fn strip_prefix_no_match_no_digits() {
        assert_eq!(
            strip_error_prefix("MCP error abc: not a code"),
            "MCP error abc: not a code"
        );
    }

    #[test]
    fn strip_prefix_no_match_no_colon_space() {
        assert_eq!(
            strip_error_prefix("MCP error -32600 no colon"),
            "MCP error -32600 no colon"
        );
    }

    #[test]
    fn strip_prefix_empty_message_after_strip() {
        assert_eq!(strip_error_prefix("MCP error 0: "), "");
    }

    // ─── build_fallback_init ──────────────────────────────────────

    #[test]
    fn fallback_init_structure() {
        let msg = build_fallback_init("test-id", "2024-11-05");
        assert!(msg.is_initialize_request());
        assert_eq!(msg.id.as_ref().unwrap().get(), "\"test-id\"");
        let params: serde_json::Value =
            serde_json::from_str(msg.params.as_ref().unwrap().get()).unwrap();
        assert_eq!(params["protocolVersion"], "2024-11-05");
        assert_eq!(params["clientInfo"]["name"], FALLBACK_CLIENT_NAME);
        assert!(params["capabilities"]["roots"]["listChanged"]
            .as_bool()
            .unwrap());
        assert!(params["capabilities"]["sampling"].is_object());
    }

    // ─── build_initialized_notification ───────────────────────────

    #[test]
    fn initialized_notification_structure() {
        let msg = build_initialized_notification();
        assert!(msg.is_notification());
        assert_eq!(msg.method_str(), Some("notifications/initialized"));
        assert!(msg.id.is_none());
    }

    // ─── generate_init_id ─────────────────────────────────────────

    #[test]
    fn init_id_format() {
        let id = generate_init_id();
        assert!(id.starts_with("init_"));
        let parts: Vec<&str> = id.splitn(3, '_').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "init");
        assert!(parts[1].chars().all(|c| c.is_ascii_digit()));
        assert_eq!(parts[2].len(), 9);
        assert!(parts[2]
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()));
    }

    #[test]
    fn init_id_unique() {
        let id1 = generate_init_id();
        let id2 = generate_init_id();
        assert_ne!(id1, id2);
    }

    // ─── intercept_protocol_version ───────────────────────────────

    #[test]
    fn intercept_sets_version() {
        let msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("1")),
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
        let result = intercept_protocol_version(msg, "2024-11-05");
        let params: serde_json::Value =
            serde_json::from_str(result.params.as_ref().unwrap().get()).unwrap();
        assert_eq!(params["protocolVersion"], "2024-11-05");
        assert!(params["capabilities"].is_object());
    }

    #[test]
    fn intercept_noop_for_non_init() {
        let msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("1")),
            method: Some("tools/list".into()),
            params: None,
            result: None,
            error: None,
            ..Default::default()
        };
        let result = intercept_protocol_version(msg, "2024-11-05");
        assert_eq!(result.method_str(), Some("tools/list"));
    }

    #[test]
    fn intercept_preserves_other_params() {
        let msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("0")),
            method: Some("initialize".into()),
            params: Some(
                serde_json::value::to_raw_value(&serde_json::json!({
                    "protocolVersion": "old",
                    "capabilities": { "roots": { "listChanged": true } },
                    "clientInfo": { "name": "test", "version": "1.0" }
                }))
                .unwrap(),
            ),
            result: None,
            error: None,
            ..Default::default()
        };
        let result = intercept_protocol_version(msg, "2024-11-05");
        let params: serde_json::Value =
            serde_json::from_str(result.params.as_ref().unwrap().get()).unwrap();
        assert_eq!(params["protocolVersion"], "2024-11-05");
        assert_eq!(params["clientInfo"]["name"], "test");
        assert!(params["capabilities"]["roots"]["listChanged"]
            .as_bool()
            .unwrap());
    }

    // ─── make_error_response ──────────────────────────────────────

    #[test]
    fn error_response_with_prefix_stripping() {
        let resp = make_error_response(
            Some(raw("5")),
            "MCP error -32600: Invalid request",
            None,
        );
        assert!(resp.is_response());
        assert_eq!(resp.id.as_ref().unwrap().get(), "5");
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("-32000"));
        assert!(s.contains("Invalid request"));
        assert!(!s.contains("MCP error"));
    }

    #[test]
    fn error_response_fallback_message() {
        let resp = make_error_response(Some(raw("1")), "", None);
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains(CLIENT_ERROR_MESSAGE));
    }

    #[test]
    fn error_response_no_prefix() {
        let resp = make_error_response(Some(raw("1")), "custom error", None);
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("custom error"));
        assert!(s.contains("-32000"));
    }

    #[test]
    fn error_response_null_id() {
        let resp = make_error_response(None, "fail", None);
        assert_eq!(resp.id.as_ref().unwrap().get(), "null");
    }

    #[test]
    fn error_response_preserves_original_code() {
        let resp = make_error_response(
            Some(raw("5")),
            "Method not found",
            Some(-32601),
        );
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("-32601"));
        assert!(s.contains("Method not found"));
        assert!(!s.contains("-32000"));
    }

    // ─── extract_error_code ──────────────────────────────────────

    #[test]
    fn extract_code_from_error_response() {
        let msg: RawMessage = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#,
        )
        .unwrap();
        assert_eq!(extract_error_code(&msg), Some(-32601));
    }

    #[test]
    fn extract_code_no_error_field() {
        let msg: RawMessage = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
        )
        .unwrap();
        assert_eq!(extract_error_code(&msg), None);
    }

    #[test]
    fn extract_code_malformed_error() {
        let msg: RawMessage = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":1,"error":{"message":"no code"}}"#,
        )
        .unwrap();
        assert_eq!(extract_error_code(&msg), None);
    }

    // ─── handle_sse_event: endpoint discovery ─────────────────────

    #[test]
    fn sse_event_endpoint_discovery() {
        let gw = make_gateway();
        let event = SseEvent {
            event_type: "endpoint".into(),
            data: "/message?sessionId=abc".into(),
            id: String::new(),
        };
        let result = gw.handle_sse_event(&event);
        assert!(result.stdout.is_empty());
        assert!(result.post.is_empty());
        assert_eq!(
            gw.endpoint_url(),
            Some("http://localhost:8080/message?sessionId=abc".into())
        );
    }

    #[test]
    fn sse_event_absolute_endpoint() {
        let gw = make_gateway();
        let event = SseEvent {
            event_type: "endpoint".into(),
            data: "http://other-host:9090/msg".into(),
            id: String::new(),
        };
        gw.handle_sse_event(&event);
        assert_eq!(
            gw.endpoint_url(),
            Some("http://other-host:9090/msg".into())
        );
    }

    // ─── handle_sse_event: message forwarding ─────────────────────

    #[test]
    fn sse_event_message_forwarded_when_ready() {
        let gw = make_gateway();
        *gw.init_phase.lock().unwrap() = InitPhase::Ready;

        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let event = SseEvent {
            event_type: "message".into(),
            data: json.into(),
            id: String::new(),
        };
        let result = gw.handle_sse_event(&event);
        assert_eq!(result.stdout.len(), 1);
        assert!(result.stdout[0].is_response());
    }

    #[test]
    fn sse_event_unknown_type_ignored() {
        let gw = make_gateway();
        let event = SseEvent {
            event_type: "keepalive".into(),
            data: String::new(),
            id: String::new(),
        };
        let result = gw.handle_sse_event(&event);
        assert!(result.stdout.is_empty());
        assert!(result.post.is_empty());
    }

    #[test]
    fn sse_event_invalid_json_ignored() {
        let gw = make_gateway();
        *gw.init_phase.lock().unwrap() = InitPhase::Ready;

        let event = SseEvent {
            event_type: "message".into(),
            data: "not valid json".into(),
            id: String::new(),
        };
        let result = gw.handle_sse_event(&event);
        assert!(result.stdout.is_empty());
    }

    // ─── handle_sse_event: synthetic init response ────────────────

    #[test]
    fn sse_event_synthetic_init_response_absorbed() {
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

        // Server sends init response.
        let json = concat!(
            r#"{"jsonrpc":"2.0","id":"test-init-id","#,
            r#""result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#,
        );
        let event = SseEvent {
            event_type: "message".into(),
            data: json.into(),
            id: String::new(),
        };
        let result = gw.handle_sse_event(&event);

        // Init response NOT forwarded to stdout.
        assert!(result.stdout.is_empty());
        // Initialized notification + pending message queued for POST.
        assert_eq!(result.post.len(), 2);
        assert_eq!(
            result.post[0].method_str(),
            Some("notifications/initialized")
        );
        assert_eq!(result.post[1].method_str(), Some("tools/list"));
        assert!(gw.is_initialized());
    }

    #[test]
    fn sse_event_notification_during_synthetic_init() {
        let gw = make_gateway();
        *gw.init_phase.lock().unwrap() =
            InitPhase::WaitingSyntheticInit("test-init-id".into());

        let json = r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"token":"x"}}"#;
        let event = SseEvent {
            event_type: "message".into(),
            data: json.into(),
            id: String::new(),
        };
        let result = gw.handle_sse_event(&event);

        // Notification forwarded to stdout.
        assert_eq!(result.stdout.len(), 1);
        assert!(result.stdout[0].is_notification());
        // Still waiting for init.
        assert!(!gw.is_initialized());
    }

    // ─── handle_sse_event: passthrough init ───────────────────────

    #[test]
    fn sse_event_passthrough_init_response_forwarded() {
        let gw = make_gateway();
        *gw.init_phase.lock().unwrap() = InitPhase::WaitingPassthroughInit;

        let json = concat!(
            r#"{"jsonrpc":"2.0","id":0,"#,
            r#""result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#,
        );
        let event = SseEvent {
            event_type: "message".into(),
            data: json.into(),
            id: String::new(),
        };
        let result = gw.handle_sse_event(&event);

        // Response forwarded to stdout (passthrough).
        assert_eq!(result.stdout.len(), 1);
        assert!(result.stdout[0].is_response());
        assert!(gw.is_initialized());
    }

    // ─── handle_stdin_message: first message is init ──────────────

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
                assert_eq!(params["protocolVersion"], "2024-11-05");
            }
            _ => panic!("expected Post action"),
        }
        assert_eq!(
            *gw.init_phase.lock().unwrap(),
            InitPhase::WaitingPassthroughInit
        );
    }

    // ─── handle_stdin_message: first message is non-init ──────────

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
                assert_eq!(params["clientInfo"]["name"], FALLBACK_CLIENT_NAME);
            }
            _ => panic!("expected PostInit action"),
        }
        assert_eq!(gw.init_pending.lock().unwrap().len(), 1);
    }

    // ─── handle_stdin_message: buffered during init ───────────────

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

    // ─── handle_stdin_message: forwarded when ready ───────────────

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

    // ─── Full init dance: synthetic ───────────────────────────────

    #[test]
    fn full_synthetic_init_dance() {
        let gw = make_gateway();

        // Step 1: Endpoint discovered.
        let ep_event = SseEvent {
            event_type: "endpoint".into(),
            data: "/message".into(),
            id: String::new(),
        };
        gw.handle_sse_event(&ep_event);
        assert!(gw.endpoint_url().is_some());

        // Step 2: Non-init stdin message arrives.
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

        // Step 3: Second stdin message arrives (buffered).
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

        // Step 4: Server sends init response via SSE.
        let init_resp_json = format!(
            concat!(
                r#"{{"jsonrpc":"2.0","id":"{}","#,
                r#""result":{{"protocolVersion":"2024-11-05","capabilities":{{}}}}}}"#,
            ),
            init_id
        );
        let sse_event = SseEvent {
            event_type: "message".into(),
            data: init_resp_json,
            id: String::new(),
        };
        let result = gw.handle_sse_event(&sse_event);

        // Init response absorbed (not in stdout).
        assert!(result.stdout.is_empty());
        // Follow-up POSTs: initialized + 2 pending messages.
        assert_eq!(result.post.len(), 3);
        assert_eq!(
            result.post[0].method_str(),
            Some("notifications/initialized")
        );
        assert_eq!(result.post[1].method_str(), Some("tools/list"));
        assert_eq!(result.post[2].method_str(), Some("resources/list"));
        assert!(gw.is_initialized());

        // Step 5: Subsequent SSE messages forwarded to stdout.
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let msg_event = SseEvent {
            event_type: "message".into(),
            data: json.into(),
            id: String::new(),
        };
        let result2 = gw.handle_sse_event(&msg_event);
        assert_eq!(result2.stdout.len(), 1);
    }

    // ─── Full init dance: passthrough ─────────────────────────────

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
                assert!(params["capabilities"]["roots"]["listChanged"]
                    .as_bool()
                    .unwrap());
            }
            _ => panic!("expected Post"),
        }

        // Step 2: Server sends init response.
        let json = concat!(
            r#"{"jsonrpc":"2.0","id":0,"#,
            r#""result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#,
        );
        let event = SseEvent {
            event_type: "message".into(),
            data: json.into(),
            id: String::new(),
        };
        let result = gw.handle_sse_event(&event);

        // Response forwarded to stdout (passthrough).
        assert_eq!(result.stdout.len(), 1);
        assert!(result.stdout[0].is_response());
        assert!(gw.is_initialized());
    }
}
