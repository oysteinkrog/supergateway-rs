// Public API consumed by downstream gateway beads — suppress dead_code until wired up.
#![allow(dead_code)]

//! Gateway error types with HTTP response mapping.
//!
//! Each [`GatewayError`] variant maps to a specific HTTP status code and response
//! body format. Transport-level errors produce plain text bodies; application-level
//! errors produce JSON-RPC 2.0 error response bodies.

use crate::child::ChildError;
use crate::jsonrpc::RawMessage;
use crate::session::SessionError;

/// Standard JSON-RPC 2.0 error codes used by the gateway.
pub mod codes {
    /// Invalid JSON-RPC request structure.
    pub const INVALID_REQUEST: i32 = -32600;
    /// Server error: session closing, method not allowed.
    pub const SERVER_ERROR: i32 = -32000;
    /// Internal error: child dead, auto-init timeout, I/O error.
    pub const INTERNAL_ERROR: i32 = -32603;
}

/// Unified error type for all gateway transport modes.
///
/// Variants are split into two categories:
/// - **Transport-level** (no JSON-RPC code): return plain text body
/// - **Application-level** (with JSON-RPC code): return JSON-RPC error body
///
/// Exception: [`MethodNotAllowed`](GatewayError::MethodNotAllowed) returns 405
/// with a JSON-RPC body per the Streamable HTTP spec.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    // ─── Transport-level errors (plain text response body) ──────────

    /// Malformed JSON body. HTTP 400 plain text.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Missing required Accept header (Streamable HTTP). HTTP 406 plain text.
    #[error("not acceptable: {0}")]
    NotAcceptable(String),

    /// Wrong Content-Type (Streamable HTTP). HTTP 415 plain text.
    #[error("unsupported media type: {0}")]
    UnsupportedMediaType(String),

    /// Session not found or invalid session ID. HTTP 404 plain text (D-015).
    #[error("Invalid or missing session ID")]
    SessionNotFound,

    /// SSE stream already open for this session. HTTP 409 plain text.
    #[error("conflict: {0}")]
    Conflict(String),

    /// Request body too large. HTTP 413 plain text.
    #[error("payload too large")]
    PayloadTooLarge,

    /// Request headers too large. HTTP 431 plain text.
    #[error("request header fields too large")]
    HeaderTooLarge,

    /// Max sessions reached or service not ready. HTTP 503 plain text.
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    // ─── Application-level errors (JSON-RPC error response body) ────

    /// Valid JSON but invalid JSON-RPC structure. HTTP 200 + JSON-RPC -32600.
    ///
    /// HTTP 200 because the HTTP request itself succeeded — the error is
    /// at the JSON-RPC application level, not the transport level.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// GET/DELETE on stateless endpoint. HTTP 405 + JSON-RPC -32000.
    ///
    /// Exception to the plain-text rule: per Streamable HTTP spec, 405
    /// responses carry a JSON-RPC error body.
    #[error("Method not allowed.")]
    MethodNotAllowed,

    /// Session is closing/draining, rejecting new requests. HTTP 503 + JSON-RPC -32000.
    #[error("session closing")]
    SessionClosing,

    /// Child MCP server process is dead. HTTP 502 + JSON-RPC -32603.
    #[error("child process dead")]
    ChildDead,

    /// Auto-init (stateless mode) timed out. HTTP 502 + JSON-RPC -32603.
    #[error("auto-init timeout")]
    AutoInitTimeout,

    /// Internal catch-all error. HTTP 500 + JSON-RPC -32603.
    #[error("internal error: {0}")]
    Internal(String),

    // ─── Wrapped errors ─────────────────────────────────────────────

    /// Child process communication error. HTTP 502 + JSON-RPC -32603.
    #[error("child error: {0}")]
    Child(#[from] ChildError),

    /// I/O error. HTTP 500 + JSON-RPC -32603.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Maps [`SessionError`] variants to specific [`GatewayError`] variants.
impl From<SessionError> for GatewayError {
    fn from(err: SessionError) -> Self {
        match err {
            SessionError::NotFound => GatewayError::SessionNotFound,
            SessionError::Closing => GatewayError::SessionClosing,
            SessionError::MaxSessionsReached => {
                GatewayError::ServiceUnavailable("max sessions reached".into())
            }
        }
    }
}

// ─── HTTP response mapping ──────────────────────────────────────────────

impl GatewayError {
    /// HTTP status code for this error.
    pub fn status_code(&self) -> u16 {
        match self {
            Self::BadRequest(_) => 400,
            Self::SessionNotFound => 404,
            Self::MethodNotAllowed => 405,
            Self::NotAcceptable(_) => 406,
            Self::Conflict(_) => 409,
            Self::PayloadTooLarge => 413,
            Self::UnsupportedMediaType(_) => 415,
            Self::HeaderTooLarge => 431,
            Self::InvalidRequest(_) => 200,
            Self::SessionClosing | Self::ServiceUnavailable(_) => 503,
            Self::ChildDead | Self::AutoInitTimeout | Self::Child(_) => 502,
            Self::Internal(_) | Self::Io(_) => 500,
        }
    }

    /// JSON-RPC error code, or `None` for transport-level errors (plain text).
    pub fn json_rpc_code(&self) -> Option<i32> {
        match self {
            Self::InvalidRequest(_) => Some(codes::INVALID_REQUEST),
            Self::MethodNotAllowed | Self::SessionClosing => Some(codes::SERVER_ERROR),
            Self::ChildDead
            | Self::AutoInitTimeout
            | Self::Internal(_)
            | Self::Child(_)
            | Self::Io(_) => Some(codes::INTERNAL_ERROR),
            _ => None,
        }
    }

    /// Content-Type header for the HTTP response.
    pub fn content_type(&self) -> &'static str {
        if self.json_rpc_code().is_some() {
            "application/json"
        } else {
            "text/plain"
        }
    }

    /// Build the HTTP response body string.
    ///
    /// For errors with a JSON-RPC code: a JSON-RPC 2.0 error response with `id: null`.
    /// For transport-level errors: the plain text error message.
    pub fn body(&self) -> String {
        if let Some(code) = self.json_rpc_code() {
            let msg = RawMessage::error_response(None, code, &self.json_rpc_message());
            serde_json::to_string(&msg).expect("JSON-RPC error serialization")
        } else {
            self.to_string()
        }
    }

    /// Message text for the JSON-RPC error object.
    fn json_rpc_message(&self) -> String {
        match self {
            Self::InvalidRequest(msg) => format!("Invalid request: {msg}"),
            Self::MethodNotAllowed => "Method not allowed.".into(),
            Self::SessionClosing => "Session closing".into(),
            Self::ChildDead => "Child process dead".into(),
            Self::AutoInitTimeout => "Auto-init timeout".into(),
            Self::Internal(msg) => format!("Internal error: {msg}"),
            Self::Child(e) => format!("Child error: {e}"),
            Self::Io(e) => format!("I/O error: {e}"),
            _ => self.to_string(),
        }
    }
}

// ─── Helper constructors ────────────────────────────────────────────────

impl GatewayError {
    pub fn bad_request(detail: &str) -> Self {
        Self::BadRequest(detail.into())
    }

    pub fn invalid_request(msg: &str) -> Self {
        Self::InvalidRequest(msg.into())
    }

    pub fn invalid_session() -> Self {
        Self::SessionNotFound
    }

    pub fn session_closing() -> Self {
        Self::SessionClosing
    }

    pub fn child_dead() -> Self {
        Self::ChildDead
    }

    pub fn max_sessions() -> Self {
        Self::ServiceUnavailable("max sessions reached".into())
    }

    pub fn not_ready() -> Self {
        Self::ServiceUnavailable("not ready".into())
    }

    pub fn missing_accept() -> Self {
        Self::NotAcceptable("missing required Accept header".into())
    }

    pub fn wrong_content_type(expected: &str, got: &str) -> Self {
        Self::UnsupportedMediaType(format!("expected {expected}, got {got}"))
    }

    pub fn payload_too_large() -> Self {
        Self::PayloadTooLarge
    }

    pub fn header_too_large() -> Self {
        Self::HeaderTooLarge
    }

    pub fn method_not_allowed() -> Self {
        Self::MethodNotAllowed
    }

    pub fn sse_stream_conflict() -> Self {
        Self::Conflict("SSE stream already open for this session".into())
    }

    pub fn auto_init_timeout() -> Self {
        Self::AutoInitTimeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Status code mapping ────────────────────────────────────────

    #[test]
    fn bad_request_400_plain_text() {
        let e = GatewayError::bad_request("malformed JSON");
        assert_eq!(e.status_code(), 400);
        assert_eq!(e.content_type(), "text/plain");
        assert!(e.json_rpc_code().is_none());
        assert_eq!(e.body(), "bad request: malformed JSON");
    }

    #[test]
    fn invalid_request_200_json_rpc() {
        let e = GatewayError::invalid_request("missing method field");
        assert_eq!(e.status_code(), 200);
        assert_eq!(e.content_type(), "application/json");
        assert_eq!(e.json_rpc_code(), Some(codes::INVALID_REQUEST));

        let body = e.body();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["error"]["code"], -32600);
        assert!(parsed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing method field"));
        assert!(parsed["id"].is_null());
    }

    #[test]
    fn session_not_found_404_plain_text() {
        let e = GatewayError::invalid_session();
        assert_eq!(e.status_code(), 404);
        assert_eq!(e.content_type(), "text/plain");
        assert!(e.json_rpc_code().is_none());
        assert_eq!(e.body(), "Invalid or missing session ID");
    }

    #[test]
    fn session_closing_503_json_rpc() {
        let e = GatewayError::session_closing();
        assert_eq!(e.status_code(), 503);
        assert_eq!(e.content_type(), "application/json");
        assert_eq!(e.json_rpc_code(), Some(codes::SERVER_ERROR));

        let body = e.body();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["error"]["code"], -32000);
        assert_eq!(parsed["error"]["message"], "Session closing");
    }

    #[test]
    fn method_not_allowed_405_json_rpc() {
        let e = GatewayError::method_not_allowed();
        assert_eq!(e.status_code(), 405);
        assert_eq!(e.content_type(), "application/json");
        assert_eq!(e.json_rpc_code(), Some(codes::SERVER_ERROR));

        let body = e.body();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["error"]["code"], -32000);
        assert_eq!(parsed["error"]["message"], "Method not allowed.");
    }

    #[test]
    fn child_dead_502_json_rpc() {
        let e = GatewayError::child_dead();
        assert_eq!(e.status_code(), 502);
        assert_eq!(e.content_type(), "application/json");
        assert_eq!(e.json_rpc_code(), Some(codes::INTERNAL_ERROR));

        let body = e.body();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["error"]["code"], -32603);
    }

    #[test]
    fn auto_init_timeout_502_json_rpc() {
        let e = GatewayError::auto_init_timeout();
        assert_eq!(e.status_code(), 502);
        assert_eq!(e.json_rpc_code(), Some(codes::INTERNAL_ERROR));
    }

    #[test]
    fn internal_error_500_json_rpc() {
        let e = GatewayError::Internal("something broke".into());
        assert_eq!(e.status_code(), 500);
        assert_eq!(e.content_type(), "application/json");
        assert_eq!(e.json_rpc_code(), Some(codes::INTERNAL_ERROR));
    }

    #[test]
    fn service_unavailable_503_plain_text() {
        let e = GatewayError::max_sessions();
        assert_eq!(e.status_code(), 503);
        assert_eq!(e.content_type(), "text/plain");
        assert!(e.json_rpc_code().is_none());
        assert!(e.body().contains("max sessions"));
    }

    #[test]
    fn not_ready_503_plain_text() {
        let e = GatewayError::not_ready();
        assert_eq!(e.status_code(), 503);
        assert_eq!(e.content_type(), "text/plain");
        assert!(e.body().contains("not ready"));
    }

    #[test]
    fn not_acceptable_406() {
        let e = GatewayError::missing_accept();
        assert_eq!(e.status_code(), 406);
        assert_eq!(e.content_type(), "text/plain");
    }

    #[test]
    fn unsupported_media_type_415() {
        let e = GatewayError::wrong_content_type("application/json", "text/html");
        assert_eq!(e.status_code(), 415);
        assert_eq!(e.content_type(), "text/plain");
        assert!(e.body().contains("application/json"));
        assert!(e.body().contains("text/html"));
    }

    #[test]
    fn conflict_409() {
        let e = GatewayError::sse_stream_conflict();
        assert_eq!(e.status_code(), 409);
        assert_eq!(e.content_type(), "text/plain");
    }

    #[test]
    fn payload_too_large_413() {
        let e = GatewayError::payload_too_large();
        assert_eq!(e.status_code(), 413);
        assert_eq!(e.content_type(), "text/plain");
    }

    #[test]
    fn header_too_large_431() {
        let e = GatewayError::header_too_large();
        assert_eq!(e.status_code(), 431);
        assert_eq!(e.content_type(), "text/plain");
    }

    // ─── From<SessionError> mapping ─────────────────────────────────

    #[test]
    fn session_error_not_found_maps_to_404() {
        let e: GatewayError = SessionError::NotFound.into();
        assert_eq!(e.status_code(), 404);
        assert!(matches!(e, GatewayError::SessionNotFound));
    }

    #[test]
    fn session_error_closing_maps_to_503_json_rpc() {
        let e: GatewayError = SessionError::Closing.into();
        assert_eq!(e.status_code(), 503);
        assert!(matches!(e, GatewayError::SessionClosing));
        assert_eq!(e.json_rpc_code(), Some(codes::SERVER_ERROR));
    }

    #[test]
    fn session_error_max_sessions_maps_to_503_plain() {
        let e: GatewayError = SessionError::MaxSessionsReached.into();
        assert_eq!(e.status_code(), 503);
        assert!(e.json_rpc_code().is_none());
        assert!(matches!(e, GatewayError::ServiceUnavailable(_)));
    }

    // ─── From<ChildError> ───────────────────────────────────────────

    #[test]
    fn child_error_maps_to_502() {
        let e: GatewayError = ChildError::BrokenPipe.into();
        assert_eq!(e.status_code(), 502);
        assert_eq!(e.json_rpc_code(), Some(codes::INTERNAL_ERROR));
    }

    // ─── From<io::Error> ───────────────────────────────────────────

    #[test]
    fn io_error_maps_to_500() {
        let e: GatewayError =
            std::io::Error::new(std::io::ErrorKind::Other, "disk full").into();
        assert_eq!(e.status_code(), 500);
        assert_eq!(e.json_rpc_code(), Some(codes::INTERNAL_ERROR));
    }

    // ─── JSON-RPC response body format ──────────────────────────────

    #[test]
    fn json_rpc_body_has_null_id() {
        let e = GatewayError::child_dead();
        let body = e.body();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert!(parsed["id"].is_null());
        assert!(parsed["error"].is_object());
        assert!(parsed["result"].is_null()); // absent
    }

    #[test]
    fn plain_text_body_is_display_string() {
        let e = GatewayError::PayloadTooLarge;
        assert_eq!(e.body(), "payload too large");
        assert_eq!(e.body(), e.to_string());
    }
}
