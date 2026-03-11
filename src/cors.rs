
use crate::cli::{CorsConfig, CorsOrigin, Header};

/// Maximum allowed request header size in bytes (8KB).
#[allow(dead_code)]
pub const MAX_HEADER_SIZE: usize = 8192;

/// Allowed methods for CORS preflight.
#[allow(dead_code)]
const ALLOW_METHODS: &str = "GET, POST, DELETE, OPTIONS";

/// Allowed request headers for CORS preflight.
#[allow(dead_code)]
const ALLOW_HEADERS: &str = "Content-Type, Accept, Mcp-Session-Id, Last-Event-ID";

/// Default max-age for CORS preflight cache (24 hours).
#[allow(dead_code)]
const MAX_AGE: &str = "86400";

// ─── CorsResult ─────────────────────────────────────────────────────

/// Result of processing a request through CORS.
#[derive(Debug)]
#[allow(dead_code)]
pub enum CorsResult {
    /// CORS disabled — no headers to add, don't handle OPTIONS specially.
    Disabled,
    /// Normal response — add these headers to the response.
    ResponseHeaders(Vec<(String, String)>),
    /// OPTIONS preflight — return 204 No Content with these headers.
    Preflight(Vec<(String, String)>),
}

// ─── CorsHandler ────────────────────────────────────────────────────

/// Processes CORS based on the CLI configuration.
///
/// Composable with Asupersync's web framework: gateway handlers call
/// `process()` and either short-circuit (preflight 204) or inject
/// the returned headers into the response.
#[allow(dead_code)]
pub struct CorsHandler {
    config: CorsConfig,
    expose_session_header: bool,
}

#[allow(dead_code)]
impl CorsHandler {
    /// Create a new CORS handler.
    ///
    /// `expose_session_header`: when true, adds `Access-Control-Expose-Headers: Mcp-Session-Id`
    /// to responses (used in stateful Streamable HTTP mode).
    #[allow(dead_code)]
    pub fn new(config: CorsConfig, expose_session_header: bool) -> Self {
        Self {
            config,
            expose_session_header,
        }
    }

    /// Whether CORS handling is active.
    #[allow(dead_code)]
    pub fn is_enabled(&self) -> bool {
        !matches!(self.config, CorsConfig::Disabled)
    }

    /// Process a request. The gateway should:
    /// - `Disabled`: do nothing
    /// - `ResponseHeaders`: add headers to the response
    /// - `Preflight`: return 204 with these headers immediately
    #[allow(dead_code)]
    pub fn process(&self, method: &str, request_origin: Option<&str>) -> CorsResult {
        match &self.config {
            CorsConfig::Disabled => CorsResult::Disabled,
            _ if method.eq_ignore_ascii_case("OPTIONS") => {
                CorsResult::Preflight(self.build_preflight(request_origin))
            }
            _ => CorsResult::ResponseHeaders(self.build_response(request_origin)),
        }
    }

    // ─── Internal ───────────────────────────────────────────────────

    #[allow(dead_code)]
    fn origin_allowed(&self, origin: &str) -> bool {
        match &self.config {
            CorsConfig::Disabled => false,
            CorsConfig::Wildcard => true,
            CorsConfig::Origins(origins) => origins.iter().any(|o| match o {
                CorsOrigin::Literal(s) => s == origin,
                CorsOrigin::Regex(re) => re.is_match(origin),
            }),
        }
    }

    #[allow(dead_code)]
    fn build_response(&self, request_origin: Option<&str>) -> Vec<(String, String)> {
        match &self.config {
            CorsConfig::Disabled => vec![],
            CorsConfig::Wildcard => {
                // Wildcard: never combine with Allow-Credentials.
                let mut h = vec![header("Access-Control-Allow-Origin", "*")];
                if self.expose_session_header {
                    h.push(header(
                        "Access-Control-Expose-Headers",
                        "Mcp-Session-Id",
                    ));
                }
                h
            }
            CorsConfig::Origins(_) => {
                let mut h = Vec::new();
                if let Some(origin) = request_origin {
                    if self.origin_allowed(origin) {
                        h.push(header("Access-Control-Allow-Origin", origin));
                        if self.expose_session_header {
                            h.push(header(
                                "Access-Control-Expose-Headers",
                                "Mcp-Session-Id",
                            ));
                        }
                    }
                }
                // Always Vary: Origin in AllowList mode (even when not matched).
                h.push(header("Vary", "Origin"));
                h
            }
        }
    }

    #[allow(dead_code)]
    fn build_preflight(&self, request_origin: Option<&str>) -> Vec<(String, String)> {
        match &self.config {
            CorsConfig::Disabled => vec![],
            CorsConfig::Wildcard => {
                let mut h = vec![
                    header("Access-Control-Allow-Origin", "*"),
                    header("Access-Control-Allow-Methods", ALLOW_METHODS),
                    header("Access-Control-Allow-Headers", ALLOW_HEADERS),
                    header("Access-Control-Max-Age", MAX_AGE),
                ];
                if self.expose_session_header {
                    h.push(header(
                        "Access-Control-Expose-Headers",
                        "Mcp-Session-Id",
                    ));
                }
                h
            }
            CorsConfig::Origins(_) => {
                let mut h = Vec::new();
                if let Some(origin) = request_origin {
                    if self.origin_allowed(origin) {
                        h.push(header("Access-Control-Allow-Origin", origin));
                        h.push(header("Access-Control-Allow-Methods", ALLOW_METHODS));
                        h.push(header("Access-Control-Allow-Headers", ALLOW_HEADERS));
                        h.push(header("Access-Control-Max-Age", MAX_AGE));
                        if self.expose_session_header {
                            h.push(header(
                                "Access-Control-Expose-Headers",
                                "Mcp-Session-Id",
                            ));
                        }
                    }
                }
                // Always Vary: Origin in AllowList mode.
                h.push(header("Vary", "Origin"));
                h
            }
        }
    }
}

#[allow(dead_code)]
fn header(name: &str, value: &str) -> (String, String) {
    (name.to_string(), value.to_string())
}

// ─── Header utilities ───────────────────────────────────────────────

/// Check if a single request header exceeds the maximum allowed size (8KB).
/// Returns `true` if the header is within limits.
#[allow(dead_code)]
pub fn header_within_limit(name: &str, value: &str) -> bool {
    // name + ": " + value + "\r\n" = name.len() + value.len() + 4
    name.len() + value.len() + 4 <= MAX_HEADER_SIZE
}

/// Apply custom `--header` values to a response header list.
///
/// Uses last-wins semantics for duplicate header names (case-insensitive).
#[allow(dead_code)]
pub fn apply_custom_headers(
    response_headers: &mut Vec<(String, String)>,
    custom: &[Header],
) {
    for h in custom {
        // Remove any existing header with the same name (case-insensitive).
        response_headers.retain(|(name, _)| !name.eq_ignore_ascii_case(&h.name));
        response_headers.push((h.name.clone(), h.value.clone()));
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    fn disabled() -> CorsHandler {
        CorsHandler::new(CorsConfig::Disabled, false)
    }

    #[allow(dead_code)]
    fn wildcard(expose_session: bool) -> CorsHandler {
        CorsHandler::new(CorsConfig::Wildcard, expose_session)
    }

    #[allow(dead_code)]
    fn allow_list(origins: Vec<CorsOrigin>, expose_session: bool) -> CorsHandler {
        CorsHandler::new(CorsConfig::Origins(origins), expose_session)
    }

    #[allow(dead_code)]
    fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    // ─── CORS disabled ─────────────────────────────────────────────

    #[test]
    fn disabled_no_headers_on_get() {
        let h = disabled();
        assert!(!h.is_enabled());
        assert!(matches!(
            h.process("GET", Some("https://example.com")),
            CorsResult::Disabled
        ));
    }

    #[test]
    fn disabled_no_options_handling() {
        let h = disabled();
        assert!(matches!(
            h.process("OPTIONS", Some("https://example.com")),
            CorsResult::Disabled
        ));
    }

    // ─── CORS wildcard ──────────────────────────────────────────────

    #[test]
    fn wildcard_allow_all_on_response() {
        let h = wildcard(false);
        assert!(h.is_enabled());
        let result = h.process("POST", Some("https://any-origin.com"));
        match result {
            CorsResult::ResponseHeaders(headers) => {
                assert_eq!(find_header(&headers, "Access-Control-Allow-Origin"), Some("*"));
                // Wildcard must NOT have Allow-Credentials
                assert!(find_header(&headers, "Access-Control-Allow-Credentials").is_none());
                // No Vary in wildcard mode
                assert!(find_header(&headers, "Vary").is_none());
            }
            _ => panic!("expected ResponseHeaders"),
        }
    }

    #[test]
    fn wildcard_options_preflight() {
        let h = wildcard(false);
        let result = h.process("OPTIONS", Some("https://any-origin.com"));
        match result {
            CorsResult::Preflight(headers) => {
                assert_eq!(find_header(&headers, "Access-Control-Allow-Origin"), Some("*"));
                assert_eq!(
                    find_header(&headers, "Access-Control-Allow-Methods"),
                    Some("GET, POST, DELETE, OPTIONS")
                );
                assert!(find_header(&headers, "Access-Control-Allow-Headers")
                    .unwrap()
                    .contains("Mcp-Session-Id"));
                assert_eq!(find_header(&headers, "Access-Control-Max-Age"), Some("86400"));
                // No Expose-Headers when expose_session is false
                assert!(find_header(&headers, "Access-Control-Expose-Headers").is_none());
            }
            _ => panic!("expected Preflight"),
        }
    }

    #[test]
    fn wildcard_no_credentials() {
        // Credentials safety: wildcard must NOT be combined with Allow-Credentials.
        let h = wildcard(false);
        match h.process("GET", None) {
            CorsResult::ResponseHeaders(headers) => {
                assert_eq!(find_header(&headers, "Access-Control-Allow-Origin"), Some("*"));
                assert!(find_header(&headers, "Access-Control-Allow-Credentials").is_none());
            }
            _ => panic!("expected ResponseHeaders"),
        }
    }

    // ─── Exact origin matching ──────────────────────────────────────

    #[test]
    fn exact_origin_matched() {
        let h = allow_list(
            vec![CorsOrigin::Literal("https://example.com".into())],
            false,
        );
        match h.process("GET", Some("https://example.com")) {
            CorsResult::ResponseHeaders(headers) => {
                assert_eq!(
                    find_header(&headers, "Access-Control-Allow-Origin"),
                    Some("https://example.com")
                );
                assert_eq!(find_header(&headers, "Vary"), Some("Origin"));
            }
            _ => panic!("expected ResponseHeaders"),
        }
    }

    #[test]
    fn exact_origin_not_matched() {
        let h = allow_list(
            vec![CorsOrigin::Literal("https://example.com".into())],
            false,
        );
        match h.process("GET", Some("https://evil.com")) {
            CorsResult::ResponseHeaders(headers) => {
                // No Allow-Origin for unmatched origin
                assert!(find_header(&headers, "Access-Control-Allow-Origin").is_none());
                // Vary: Origin still present
                assert_eq!(find_header(&headers, "Vary"), Some("Origin"));
            }
            _ => panic!("expected ResponseHeaders"),
        }
    }

    #[test]
    fn exact_origin_no_origin_header() {
        let h = allow_list(
            vec![CorsOrigin::Literal("https://example.com".into())],
            false,
        );
        match h.process("GET", None) {
            CorsResult::ResponseHeaders(headers) => {
                assert!(find_header(&headers, "Access-Control-Allow-Origin").is_none());
                assert_eq!(find_header(&headers, "Vary"), Some("Origin"));
            }
            _ => panic!("expected ResponseHeaders"),
        }
    }

    // ─── Regex origin matching ──────────────────────────────────────

    #[test]
    fn regex_origin_matched() {
        let re = Regex::new(r"^https://.*\.example\.com$").unwrap();
        let h = allow_list(vec![CorsOrigin::Regex(re)], false);
        match h.process("GET", Some("https://sub.example.com")) {
            CorsResult::ResponseHeaders(headers) => {
                assert_eq!(
                    find_header(&headers, "Access-Control-Allow-Origin"),
                    Some("https://sub.example.com")
                );
            }
            _ => panic!("expected ResponseHeaders"),
        }
    }

    #[test]
    fn regex_origin_not_matched() {
        let re = Regex::new(r"^https://.*\.example\.com$").unwrap();
        let h = allow_list(vec![CorsOrigin::Regex(re)], false);
        match h.process("GET", Some("https://evil.com")) {
            CorsResult::ResponseHeaders(headers) => {
                assert!(find_header(&headers, "Access-Control-Allow-Origin").is_none());
            }
            _ => panic!("expected ResponseHeaders"),
        }
    }

    #[test]
    fn mixed_exact_and_regex() {
        let re = Regex::new(r"^https://.*\.dev$").unwrap();
        let h = allow_list(
            vec![
                CorsOrigin::Literal("https://prod.example.com".into()),
                CorsOrigin::Regex(re),
            ],
            false,
        );

        // Exact match
        match h.process("GET", Some("https://prod.example.com")) {
            CorsResult::ResponseHeaders(headers) => {
                assert_eq!(
                    find_header(&headers, "Access-Control-Allow-Origin"),
                    Some("https://prod.example.com")
                );
            }
            _ => panic!("expected ResponseHeaders"),
        }

        // Regex match
        match h.process("GET", Some("https://app.dev")) {
            CorsResult::ResponseHeaders(headers) => {
                assert_eq!(
                    find_header(&headers, "Access-Control-Allow-Origin"),
                    Some("https://app.dev")
                );
            }
            _ => panic!("expected ResponseHeaders"),
        }

        // Neither
        match h.process("GET", Some("https://other.com")) {
            CorsResult::ResponseHeaders(headers) => {
                assert!(find_header(&headers, "Access-Control-Allow-Origin").is_none());
            }
            _ => panic!("expected ResponseHeaders"),
        }
    }

    // ─── OPTIONS preflight (AllowList) ──────────────────────────────

    #[test]
    fn allowlist_preflight_matched() {
        let h = allow_list(
            vec![CorsOrigin::Literal("https://example.com".into())],
            false,
        );
        match h.process("OPTIONS", Some("https://example.com")) {
            CorsResult::Preflight(headers) => {
                assert_eq!(
                    find_header(&headers, "Access-Control-Allow-Origin"),
                    Some("https://example.com")
                );
                assert!(find_header(&headers, "Access-Control-Allow-Methods").is_some());
                assert!(find_header(&headers, "Access-Control-Allow-Headers").is_some());
                assert_eq!(find_header(&headers, "Access-Control-Max-Age"), Some("86400"));
                assert_eq!(find_header(&headers, "Vary"), Some("Origin"));
            }
            _ => panic!("expected Preflight"),
        }
    }

    #[test]
    fn allowlist_preflight_not_matched() {
        let h = allow_list(
            vec![CorsOrigin::Literal("https://example.com".into())],
            false,
        );
        match h.process("OPTIONS", Some("https://evil.com")) {
            CorsResult::Preflight(headers) => {
                // No Allow-Origin
                assert!(find_header(&headers, "Access-Control-Allow-Origin").is_none());
                // No methods/headers/max-age either
                assert!(find_header(&headers, "Access-Control-Allow-Methods").is_none());
                // Still has Vary
                assert_eq!(find_header(&headers, "Vary"), Some("Origin"));
            }
            _ => panic!("expected Preflight"),
        }
    }

    // ─── Expose-Headers (Mcp-Session-Id) ────────────────────────────

    #[test]
    fn expose_session_header_when_enabled() {
        let h = wildcard(true);
        match h.process("GET", Some("https://example.com")) {
            CorsResult::ResponseHeaders(headers) => {
                assert_eq!(
                    find_header(&headers, "Access-Control-Expose-Headers"),
                    Some("Mcp-Session-Id")
                );
            }
            _ => panic!("expected ResponseHeaders"),
        }
    }

    #[test]
    fn no_expose_session_header_when_disabled() {
        let h = wildcard(false);
        match h.process("GET", Some("https://example.com")) {
            CorsResult::ResponseHeaders(headers) => {
                assert!(find_header(&headers, "Access-Control-Expose-Headers").is_none());
            }
            _ => panic!("expected ResponseHeaders"),
        }
    }

    #[test]
    fn expose_session_on_preflight() {
        let h = wildcard(true);
        match h.process("OPTIONS", Some("https://example.com")) {
            CorsResult::Preflight(headers) => {
                assert_eq!(
                    find_header(&headers, "Access-Control-Expose-Headers"),
                    Some("Mcp-Session-Id")
                );
            }
            _ => panic!("expected Preflight"),
        }
    }

    #[test]
    fn expose_session_allowlist() {
        let h = allow_list(
            vec![CorsOrigin::Literal("https://example.com".into())],
            true,
        );
        match h.process("GET", Some("https://example.com")) {
            CorsResult::ResponseHeaders(headers) => {
                assert_eq!(
                    find_header(&headers, "Access-Control-Expose-Headers"),
                    Some("Mcp-Session-Id")
                );
            }
            _ => panic!("expected ResponseHeaders"),
        }
    }

    // ─── OPTIONS case-insensitive ───────────────────────────────────

    #[test]
    fn options_case_insensitive() {
        let h = wildcard(false);
        assert!(matches!(
            h.process("options", Some("https://x.com")),
            CorsResult::Preflight(_)
        ));
        assert!(matches!(
            h.process("Options", Some("https://x.com")),
            CorsResult::Preflight(_)
        ));
    }

    // ─── Vary: Origin ───────────────────────────────────────────────

    #[test]
    fn vary_origin_in_allowlist_mode() {
        let h = allow_list(
            vec![CorsOrigin::Literal("https://example.com".into())],
            false,
        );

        // Matched origin
        match h.process("GET", Some("https://example.com")) {
            CorsResult::ResponseHeaders(headers) => {
                assert_eq!(find_header(&headers, "Vary"), Some("Origin"));
            }
            _ => panic!("expected ResponseHeaders"),
        }

        // Unmatched origin — still has Vary
        match h.process("GET", Some("https://other.com")) {
            CorsResult::ResponseHeaders(headers) => {
                assert_eq!(find_header(&headers, "Vary"), Some("Origin"));
            }
            _ => panic!("expected ResponseHeaders"),
        }
    }

    #[test]
    fn no_vary_in_wildcard_mode() {
        let h = wildcard(false);
        match h.process("GET", Some("https://example.com")) {
            CorsResult::ResponseHeaders(headers) => {
                assert!(find_header(&headers, "Vary").is_none());
            }
            _ => panic!("expected ResponseHeaders"),
        }
    }

    // ─── Max header size ────────────────────────────────────────────

    #[test]
    fn header_within_limit_normal() {
        assert!(header_within_limit("Content-Type", "application/json"));
        assert!(header_within_limit("X-Custom", "short value"));
    }

    #[test]
    fn header_within_limit_at_boundary() {
        // name + value + 4 = MAX_HEADER_SIZE
        let name = "X";
        let value_len = MAX_HEADER_SIZE - 1 - 4; // name="X" (1) + 4 overhead
        let value = "a".repeat(value_len);
        assert!(header_within_limit(name, &value));

        // One byte over
        let value_over = "a".repeat(value_len + 1);
        assert!(!header_within_limit(name, &value_over));
    }

    #[test]
    fn header_reject_oversized() {
        let big_name = "X-".to_string() + &"A".repeat(MAX_HEADER_SIZE);
        assert!(!header_within_limit(&big_name, "value"));
    }

    // ─── Custom headers ─────────────────────────────────────────────

    #[test]
    fn apply_custom_headers_basic() {
        let mut response = vec![
            ("Content-Type".to_string(), "text/plain".to_string()),
        ];
        let custom = vec![Header {
            name: "X-Custom".into(),
            value: "hello".into(),
        }];
        apply_custom_headers(&mut response, &custom);

        assert_eq!(response.len(), 2);
        assert_eq!(response[1].0, "X-Custom");
        assert_eq!(response[1].1, "hello");
    }

    #[test]
    fn apply_custom_headers_last_wins() {
        let mut response = vec![
            ("Content-Type".to_string(), "text/plain".to_string()),
        ];
        let custom = vec![
            Header {
                name: "Content-Type".into(),
                value: "application/json".into(),
            },
        ];
        apply_custom_headers(&mut response, &custom);

        assert_eq!(response.len(), 1);
        assert_eq!(response[0].0, "Content-Type");
        assert_eq!(response[0].1, "application/json");
    }

    #[test]
    fn apply_custom_headers_case_insensitive() {
        let mut response = vec![
            ("content-type".to_string(), "text/plain".to_string()),
        ];
        let custom = vec![Header {
            name: "Content-Type".into(),
            value: "application/json".into(),
        }];
        apply_custom_headers(&mut response, &custom);

        assert_eq!(response.len(), 1);
        assert_eq!(response[0].1, "application/json");
    }

    #[test]
    fn apply_custom_headers_multiple() {
        let mut response = Vec::new();
        let custom = vec![
            Header { name: "X-A".into(), value: "1".into() },
            Header { name: "X-B".into(), value: "2".into() },
            Header { name: "X-A".into(), value: "3".into() }, // overwrites first X-A
        ];
        apply_custom_headers(&mut response, &custom);

        assert_eq!(response.len(), 2);
        // X-A should have last value
        let xa = response.iter().find(|(n, _)| n == "X-A").unwrap();
        assert_eq!(xa.1, "3");
        let xb = response.iter().find(|(n, _)| n == "X-B").unwrap();
        assert_eq!(xb.1, "2");
    }

    #[test]
    fn apply_custom_headers_empty() {
        let mut response = vec![
            ("Existing".to_string(), "value".to_string()),
        ];
        apply_custom_headers(&mut response, &[]);
        assert_eq!(response.len(), 1);
    }
}
