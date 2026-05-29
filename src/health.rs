
use crate::cli::LogLevel;
use crate::observe::Metrics;
use std::sync::Arc;

/// Health check response.
#[allow(dead_code)]
pub enum HealthResponse {
    /// 200 OK with plain text "ok".
    Ok,
    /// 200 OK with JSON metrics detail.
    OkDetail(String),
    /// 503 Service Unavailable — not yet ready.
    NotReady,
}

#[allow(dead_code)]
impl HealthResponse {
    #[allow(dead_code)]
    pub fn status_code(&self) -> u16 {
        match self {
            Self::Ok | Self::OkDetail(_) => 200,
            Self::NotReady => 503,
        }
    }

    #[allow(dead_code)]
    pub fn content_type(&self) -> &'static str {
        match self {
            Self::Ok | Self::NotReady => "text/plain",
            Self::OkDetail(_) => "application/json",
        }
    }

    #[allow(dead_code)]
    pub fn body(&self) -> &str {
        match self {
            Self::Ok => "ok",
            Self::OkDetail(json) => json,
            Self::NotReady => "Service starting",
        }
    }
}

/// Handle a health endpoint request.
///
/// - Returns 503 until `metrics.set_ready()` has been called.
/// - Returns JSON detail (including per-transport counters) when `detail=true`.
/// - Otherwise returns plain text "ok".
#[allow(dead_code)]
pub fn check_health(
    metrics: &Arc<Metrics>,
    detail: bool,
    _log_level: LogLevel,
) -> HealthResponse {
    if !metrics.is_ready() {
        return HealthResponse::NotReady;
    }

    if detail {
        HealthResponse::OkDetail(metrics.snapshot_json_detail())
    } else {
        HealthResponse::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_not_ready() {
        let m = Metrics::new();
        let resp = check_health(&m, false, LogLevel::Info);
        assert_eq!(resp.status_code(), 503);
        assert_eq!(resp.content_type(), "text/plain");
        assert_eq!(resp.body(), "Service starting");
    }

    #[test]
    fn test_health_ready_ok() {
        let m = Metrics::new();
        m.set_ready();
        let resp = check_health(&m, false, LogLevel::Info);
        assert_eq!(resp.status_code(), 200);
        assert_eq!(resp.content_type(), "text/plain");
        assert_eq!(resp.body(), "ok");
    }

    #[test]
    fn test_health_detail_returns_json() {
        let m = Metrics::new();
        m.set_ready();
        m.total_requests.store(5, std::sync::atomic::Ordering::Relaxed);

        // detail=true at any log level → JSON
        let resp = check_health(&m, true, LogLevel::Info);
        assert_eq!(resp.status_code(), 200);
        assert_eq!(resp.content_type(), "application/json");

        let parsed: serde_json::Value = serde_json::from_str(resp.body()).unwrap();
        assert_eq!(parsed["total_requests"], 5);
        assert_eq!(parsed["ready"], true);
    }

    #[test]
    fn test_health_detail_not_ready_returns_503() {
        let m = Metrics::new();
        // Even with detail=true and debug, should still 503 if not ready.
        let resp = check_health(&m, true, LogLevel::Debug);
        assert_eq!(resp.status_code(), 503);
    }

    #[test]
    fn test_health_detail_all_fields_present() {
        let m = Metrics::new();
        m.set_ready();
        let resp = check_health(&m, true, LogLevel::Info);
        let parsed: serde_json::Value = serde_json::from_str(resp.body()).unwrap();

        // Verify all expected counter fields exist.
        let expected_fields = [
            "active_sessions",
            "active_children",
            "active_clients",
            "total_requests",
            "total_spawns",
            "spawn_failures",
            "session_timeouts",
            "forced_kills",
            "client_disconnects",
            "backpressure_events",
            "queue_depth_max",
            "decode_errors",
            "ready",
        ];
        for field in &expected_fields {
            assert!(
                parsed.get(field).is_some(),
                "missing field in health detail: {field}"
            );
        }

        // Verify transports sub-object is present with all 6 transports
        let transports = parsed.get("transports").expect("missing transports field");
        for transport in &["sse", "ws", "stateful_http", "stateless_http", "sse_to_stdio", "http_to_stdio"] {
            let t = transports.get(transport).unwrap_or_else(|| panic!("missing transport: {transport}"));
            assert!(t.get("requests").is_some(), "missing requests in {transport}");
            assert!(t.get("errors").is_some(), "missing errors in {transport}");
            assert!(t.get("latency_p50_ms").is_some(), "missing latency_p50_ms in {transport}");
        }
    }

    #[test]
    fn test_health_detail_transport_counters_reflected() {
        use crate::observe::{record_error, record_latency, record_request, Transport};

        let m = Metrics::new();
        m.set_ready();

        record_request(&m, Transport::Sse);
        record_request(&m, Transport::Sse);
        record_error(&m, Transport::Sse);
        record_latency(&m, Transport::Sse, 5);

        record_request(&m, Transport::Ws);

        let resp = check_health(&m, true, LogLevel::Info);
        let parsed: serde_json::Value = serde_json::from_str(resp.body()).unwrap();

        let sse = &parsed["transports"]["sse"];
        assert_eq!(sse["requests"], 2);
        assert_eq!(sse["errors"], 1);

        let ws = &parsed["transports"]["ws"];
        assert_eq!(ws["requests"], 1);
        assert_eq!(ws["errors"], 0);
    }
}
