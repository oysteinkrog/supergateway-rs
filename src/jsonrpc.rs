use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

/// A JSON-RPC 2.0 message in wire format.
///
/// This is a flat struct (not an enum) because the same shape carries requests,
/// responses, notifications, and errors. Classification is done via helper methods.
/// All payload fields use `Box<RawValue>` for opaque pass-through — we never
/// interpret params, result, or error contents.
///
/// Extension fields (e.g. `_meta`) are captured in [`extra`](Self::extra) via
/// `#[serde(flatten)]` so they survive round-trip serialization (protocol transparency).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawMessage {
    pub jsonrpc: String,

    /// Absent → notification. Present (even if JSON null) → expects a response.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_raw",
        serialize_with = "serialize_optional_raw"
    )]
    pub id: Option<Box<RawValue>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Box<RawValue>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Box<RawValue>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Box<RawValue>>,

    /// Extension fields not part of the core JSON-RPC spec (e.g. `_meta`).
    /// Captured via flatten to preserve protocol transparency on round-trip.
    #[serde(flatten)]
    pub extra: HashMap<String, Box<RawValue>>,
}

impl Default for RawMessage {
    fn default() -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: None,
            method: None,
            params: None,
            result: None,
            error: None,
            extra: HashMap::new(),
        }
    }
}

// ─── id field: distinguish absent vs null vs value ──────────────────────────
//
// serde default for Option<Box<RawValue>> collapses absent and JSON null into
// None. We use a sentinel wrapper so that:
//   - field absent in JSON  →  id = None
//   - "id": null            →  id = Some(RawValue("null"))
//   - "id": 42              →  id = Some(RawValue("42"))

fn deserialize_optional_raw<'de, D>(deserializer: D) -> Result<Option<Box<RawValue>>, D::Error>
where
    D: Deserializer<'de>,
{
    // If the deserializer calls us, the field IS present in the JSON.
    // Deserialize the raw value (which can be null, number, string, etc.).
    let val = Box::<RawValue>::deserialize(deserializer)?;
    Ok(Some(val))
}

fn serialize_optional_raw<S>(
    val: &Option<Box<RawValue>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match val {
        Some(v) => v.serialize(serializer),
        None => serializer.serialize_none(),
    }
}

impl RawMessage {
    /// Request: has id + method.
    pub fn is_request(&self) -> bool {
        self.id.is_some() && self.method.is_some()
    }

    /// Response: has id + (result or error), no method.
    pub fn is_response(&self) -> bool {
        self.id.is_some() && (self.result.is_some() || self.error.is_some()) && self.method.is_none()
    }

    /// Notification: has method, id field is absent (not null).
    pub fn is_notification(&self) -> bool {
        self.method.is_some() && self.id.is_none()
    }

    /// Initialize request: method == "initialize" and is_request().
    pub fn is_initialize_request(&self) -> bool {
        self.is_request() && self.method.as_deref() == Some("initialize")
    }

    /// Convenience accessor for the method field.
    pub fn method_str(&self) -> Option<&str> {
        self.method.as_deref()
    }

    /// Build a JSON-RPC error response.
    ///
    /// If `id` is None (notification that errored), the response uses `null` for id.
    pub fn error_response(id: Option<Box<RawValue>>, code: i32, message: &str) -> Self {
        let error_obj = serde_json::json!({
            "code": code,
            "message": message,
        });
        let error_raw =
            serde_json::value::to_raw_value(&error_obj).expect("error object serialization");
        let response_id = id.unwrap_or_else(|| {
            RawValue::from_string("null".into()).expect("null raw value")
        });
        Self {
            jsonrpc: "2.0".into(),
            id: Some(response_id),
            method: None,
            params: None,
            result: None,
            error: Some(error_raw),
            ..Default::default()
        }
    }
}

/// Result of parsing a JSON-RPC line.
#[derive(Debug)]
pub enum Parsed {
    Single(RawMessage),
    Batch(Vec<RawMessage>),
}

/// Parse a JSON line into a single message or a batch.
///
/// Uses first non-whitespace byte to decide: `{` → single, `[` → batch.
pub fn parse_line(line: &str) -> Result<Parsed, serde_json::Error> {
    let first = line.as_bytes().iter().find(|b| !b.is_ascii_whitespace());
    match first {
        Some(b'[') => {
            let batch: Vec<RawMessage> = serde_json::from_str(line)?;
            Ok(Parsed::Batch(batch))
        }
        _ => {
            let msg: RawMessage = serde_json::from_str(line)?;
            Ok(Parsed::Single(msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(s: &str) -> Box<RawValue> {
        RawValue::from_string(s.to_string()).unwrap()
    }

    // ─── Serialization round-trips ──────────────────────────────────────

    #[test]
    fn roundtrip_request() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":"abc"}}"#;
        let msg: RawMessage = serde_json::from_str(json).unwrap();
        assert!(msg.is_request());
        assert!(!msg.is_response());
        assert!(!msg.is_notification());
        assert_eq!(msg.method_str(), Some("tools/list"));
        assert_eq!(msg.id.as_ref().unwrap().get(), "1");
        assert!(msg.params.as_ref().unwrap().get().contains("cursor"));

        let reserialized = serde_json::to_string(&msg).unwrap();
        assert!(reserialized.contains(r#""method":"tools/list""#));
        assert!(reserialized.contains(r#""cursor":"abc""#));
    }

    #[test]
    fn roundtrip_response() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"echo"}]}}"#;
        let msg: RawMessage = serde_json::from_str(json).unwrap();
        assert!(msg.is_response());
        assert!(!msg.is_request());
        assert!(msg.result.as_ref().unwrap().get().contains("echo"));
    }

    #[test]
    fn roundtrip_notification() {
        let json = r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"token":"x"}}"#;
        let msg: RawMessage = serde_json::from_str(json).unwrap();
        assert!(msg.is_notification());
        assert!(!msg.is_request());
        assert!(msg.id.is_none());
    }

    #[test]
    fn roundtrip_error_response() {
        let json = r#"{"jsonrpc":"2.0","id":5,"error":{"code":-32601,"message":"Method not found"}}"#;
        let msg: RawMessage = serde_json::from_str(json).unwrap();
        assert!(msg.is_response());
        assert!(msg.error.is_some());
        assert!(msg.result.is_none());
    }

    // ─── id semantics: absent vs null vs value ──────────────────────────

    #[test]
    fn id_absent_is_notification() {
        let json = r#"{"jsonrpc":"2.0","method":"ping"}"#;
        let msg: RawMessage = serde_json::from_str(json).unwrap();
        assert!(msg.id.is_none());
        assert!(msg.is_notification());
    }

    #[test]
    fn id_null_is_request_not_notification() {
        let json = r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#;
        let msg: RawMessage = serde_json::from_str(json).unwrap();
        assert!(msg.id.is_some());
        assert_eq!(msg.id.as_ref().unwrap().get(), "null");
        assert!(msg.is_request());
        assert!(!msg.is_notification());
    }

    #[test]
    fn id_numeric() {
        let json = r#"{"jsonrpc":"2.0","id":42,"method":"foo"}"#;
        let msg: RawMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id.as_ref().unwrap().get(), "42");
    }

    #[test]
    fn id_string() {
        let json = r#"{"jsonrpc":"2.0","id":"abc-123","method":"foo"}"#;
        let msg: RawMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.id.as_ref().unwrap().get(), r#""abc-123""#);
    }

    #[test]
    fn serialize_absent_id_omits_field() {
        let msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: None,
            method: Some("ping".into()),
            params: None,
            result: None,
            error: None,
            ..Default::default()
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(!s.contains("\"id\""));
    }

    #[test]
    fn serialize_null_id_includes_field() {
        let msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(raw("null")),
            method: Some("ping".into()),
            params: None,
            result: None,
            error: None,
            ..Default::default()
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(s.contains(r#""id":null"#));
    }

    // ─── Initialize request detection ───────────────────────────────────

    #[test]
    fn is_initialize_request() {
        let json = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"capabilities":{}}}"#;
        let msg: RawMessage = serde_json::from_str(json).unwrap();
        assert!(msg.is_initialize_request());
    }

    #[test]
    fn not_initialize_notification() {
        let json = r#"{"jsonrpc":"2.0","method":"initialize","params":{}}"#;
        let msg: RawMessage = serde_json::from_str(json).unwrap();
        assert!(!msg.is_initialize_request()); // no id → notification
    }

    // ─── error_response() ───────────────────────────────────────────────

    #[test]
    fn error_response_with_id() {
        let resp = RawMessage::error_response(Some(raw("7")), -32601, "Method not found");
        assert!(resp.is_response());
        assert_eq!(resp.id.as_ref().unwrap().get(), "7");
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("-32601"));
        assert!(s.contains("Method not found"));
    }

    #[test]
    fn error_response_without_id_uses_null() {
        let resp = RawMessage::error_response(None, -32700, "Parse error");
        assert_eq!(resp.id.as_ref().unwrap().get(), "null");
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains(r#""id":null"#));
    }

    // ─── Unknown methods pass through ───────────────────────────────────

    #[test]
    fn unknown_method_accepted() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"custom/fooBar","params":{"x":true}}"#;
        let msg: RawMessage = serde_json::from_str(json).unwrap();
        assert!(msg.is_request());
        assert_eq!(msg.method_str(), Some("custom/fooBar"));
    }

    // ─── Extension fields preserved ─────────────────────────────────────

    #[test]
    fn extension_fields_preserved() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"foo","_meta":{"token":"x"}}"#;
        let msg: RawMessage = serde_json::from_str(json).unwrap();
        assert!(msg.is_request());
        // Extension field must be captured in extra
        assert!(msg.extra.contains_key("_meta"));
        assert_eq!(msg.extra["_meta"].get(), r#"{"token":"x"}"#);
        // Roundtrip: extension fields survive serialization
        let reserialized = serde_json::to_string(&msg).unwrap();
        assert!(reserialized.contains(r#""_meta":{"token":"x"}"#));
    }

    // ─── Large params ───────────────────────────────────────────────────

    #[test]
    fn large_params_roundtrip() {
        let big_array: Vec<i32> = (0..10_000).collect();
        let params_json = serde_json::to_string(&big_array).unwrap();
        let json = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"big","params":{params_json}}}"#
        );
        let msg: RawMessage = serde_json::from_str(&json).unwrap();
        let rt = serde_json::to_string(&msg).unwrap();
        assert!(rt.contains("9999"));
    }

    // ─── Batch support ──────────────────────────────────────────────────

    #[test]
    fn parse_single() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let parsed = parse_line(line).unwrap();
        assert!(matches!(parsed, Parsed::Single(m) if m.is_request()));
    }

    #[test]
    fn parse_batch() {
        let line = r#"[{"jsonrpc":"2.0","id":1,"method":"a"},{"jsonrpc":"2.0","method":"b"}]"#;
        let parsed = parse_line(line).unwrap();
        match parsed {
            Parsed::Batch(msgs) => {
                assert_eq!(msgs.len(), 2);
                assert!(msgs[0].is_request());
                assert!(msgs[1].is_notification());
            }
            _ => panic!("expected Batch"),
        }
    }

    #[test]
    fn parse_malformed_returns_error() {
        assert!(parse_line("not json at all").is_err());
        assert!(parse_line("").is_err());
    }

    #[test]
    fn parse_batch_whitespace_prefix() {
        let line = r#"  [{"jsonrpc":"2.0","id":1,"method":"a"}]"#;
        let parsed = parse_line(line).unwrap();
        assert!(matches!(parsed, Parsed::Batch(_)));
    }
}
