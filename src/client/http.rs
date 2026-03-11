
//! HTTP/1.1 client for connecting to remote MCP servers.
//!
//! Used by client modes (SSE->stdio, Streamable HTTP->stdio). Wraps Asupersync's
//! `HttpClient` with MCP-specific behavior: custom headers, session tracking,
//! per-method redirect policy, and JSON-RPC error extraction.

use std::sync::Mutex;

use asupersync::http::h1::http_client::{
    ClientError, ClientIo, HttpClient as AsupersyncHttpClient, HttpClientConfig, ParsedUrl,
    RedirectPolicy, Scheme,
};
use asupersync::http::h1::types::{Method, Response};
use asupersync::http::h1::client::ClientStreamingResponse;
use asupersync::http::pool::PoolConfig;

use crate::cli::Header;

// ─── Constants ─────────────────────────────────────────────────────

/// Maximum idle connections per host.
#[allow(dead_code)]
const MAX_IDLE_PER_HOST: usize = 4;

/// Idle connection timeout.
#[allow(dead_code)]
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Maximum GET redirects.
#[allow(dead_code)]
const MAX_GET_REDIRECTS: u32 = 5;

// ─── Error types ───────────────────────────────────────────────────

/// Errors from the HTTP client.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum HttpClientError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    #[error("connection failed: {0}")]
    Connect(String),

    #[error("TLS error: {0}")]
    Tls(String),

    #[error("HTTP protocol error: {0}")]
    Protocol(String),

    #[error("too many redirects ({count} of max {max})")]
    TooManyRedirects { count: u32, max: u32 },

    #[error("server error {status}: {body}")]
    ServerError { status: u16, body: String },

    #[error("I/O error: {0}")]
    Io(String),
}

#[allow(dead_code)]
impl From<ClientError> for HttpClientError {
    #[allow(dead_code)]
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::InvalidUrl(s) => Self::InvalidUrl(s),
            ClientError::DnsError(e) => Self::Connect(e.to_string()),
            ClientError::ConnectError(e) => Self::Connect(e.to_string()),
            ClientError::TlsError(e) => Self::Tls(e),
            ClientError::HttpError(e) => Self::Protocol(e.to_string()),
            ClientError::TooManyRedirects { count, max } => {
                Self::TooManyRedirects { count, max }
            }
            ClientError::Io(e) => Self::Io(e.to_string()),
        }
    }
}

// ─── Response types ────────────────────────────────────────────────

/// HTTP response with metadata needed by MCP client modes.
#[derive(Debug)]
#[allow(dead_code)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

#[allow(dead_code)]
impl HttpResponse {
    /// Get the Content-Type header value (case-insensitive lookup).
    #[allow(dead_code)]
    pub fn content_type(&self) -> Option<&str> {
        get_header(&self.headers, "Content-Type")
    }

    /// Get the Mcp-Session-Id header value.
    #[allow(dead_code)]
    pub fn mcp_session_id(&self) -> Option<&str> {
        get_header(&self.headers, "Mcp-Session-Id")
    }
}

/// Streaming HTTP response — the body is read incrementally.
#[allow(dead_code)]
pub struct HttpStreamResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// Streaming body reader.
    pub body: ClientStreamingResponse<ClientIo>,
}

#[allow(dead_code)]
impl HttpStreamResponse {
    /// Get the Content-Type header value.
    #[allow(dead_code)]
    pub fn content_type(&self) -> Option<&str> {
        get_header(&self.headers, "Content-Type")
    }

    /// Get the Mcp-Session-Id header value.
    #[allow(dead_code)]
    pub fn mcp_session_id(&self) -> Option<&str> {
        get_header(&self.headers, "Mcp-Session-Id")
    }
}

// ─── HttpClient ────────────────────────────────────────────────────

/// MCP HTTP client with custom headers, session tracking, and per-method redirect policy.
///
/// Wraps Asupersync's `HttpClient` with:
/// - Custom `--header` values applied to every outgoing request
/// - `--oauth2Bearer` applied as Authorization header (already in headers from CLI)
/// - Connection pool: max 4 idle connections per host, 60s idle timeout
/// - Redirects: follow up to 5 for GET, do NOT follow for POST
/// - Content-Type exposed on response for caller branching
/// - Mcp-Session-Id capture/injection for stateful session management
/// - Error response body parsing (JSON-RPC extraction on non-2xx)
#[allow(dead_code)]
pub struct HttpClient {
    /// Underlying asupersync HTTP client (configured with RedirectPolicy::None
    /// so we handle redirects ourselves per-method).
    inner: AsupersyncHttpClient,
    /// Custom headers from --header and --oauth2Bearer, applied to every request.
    custom_headers: Vec<(String, String)>,
    /// Captured Mcp-Session-Id from most recent response.
    session_id: Mutex<Option<String>>,
}

#[allow(dead_code)]
impl HttpClient {
    /// Create a new HTTP client.
    ///
    /// `headers` are the CLI `--header` values (including `--oauth2Bearer` if set).
    /// These are applied to every outgoing request.
    #[allow(dead_code)]
    pub fn new(headers: &[Header]) -> Self {
        let pool_config = PoolConfig::builder()
            .max_connections_per_host(MAX_IDLE_PER_HOST)
            .idle_timeout(IDLE_TIMEOUT)
            .build();

        let config = HttpClientConfig {
            pool_config,
            // We handle redirects per-method, so disable automatic redirects.
            redirect_policy: RedirectPolicy::None,
            user_agent: Some("supergateway-rs/0.1".into()),
        };

        let custom_headers: Vec<(String, String)> = headers
            .iter()
            .map(|h| (h.name.clone(), h.value.clone()))
            .collect();

        Self {
            inner: AsupersyncHttpClient::with_config(config),
            custom_headers,
            session_id: Mutex::new(None),
        }
    }

    /// Send a POST request with JSON body.
    ///
    /// Does NOT follow redirects (POST is not idempotent).
    /// Does NOT retry on failure.
    #[allow(dead_code)]
    pub async fn post(
        &self,
        url: &str,
        body: &[u8],
        content_type: &str,
    ) -> Result<HttpResponse, HttpClientError> {
        let mut headers = self.build_headers();
        headers.push(("Content-Type".to_owned(), content_type.to_owned()));

        let resp = self
            .inner
            .request(Method::Post, url, headers, body.to_vec())
            .await?;

        self.capture_session_id(&resp.headers);
        self.check_error_response(resp)
    }

    /// Send a GET request and return a streaming response.
    ///
    /// Follows up to 5 redirects. Used for SSE endpoints where the body
    /// is read incrementally as events arrive.
    ///
    /// If `last_event_id` is `Some(id)` and non-empty, the `Last-Event-ID`
    /// header is added to support SSE reconnection (D-013).
    #[allow(dead_code)]
    pub async fn get_stream(
        &self,
        url: &str,
        accept: &str,
        last_event_id: Option<&str>,
    ) -> Result<HttpStreamResponse, HttpClientError> {
        let mut headers = self.build_headers();
        headers.push(("Accept".to_owned(), accept.to_owned()));
        if let Some(id) = last_event_id.filter(|id| !id.is_empty()) {
            headers.push(("Last-Event-ID".to_owned(), id.to_owned()));
        }

        let resp = self
            .get_with_redirects_streaming(url, headers, 0)
            .await?;

        let status = resp.head.status;
        let resp_headers = resp.head.headers.clone();

        self.capture_session_id(&resp_headers);

        Ok(HttpStreamResponse {
            status,
            headers: resp_headers,
            body: resp,
        })
    }

    /// Send a DELETE request.
    ///
    /// Does NOT follow redirects.
    #[allow(dead_code)]
    pub async fn delete(&self, url: &str) -> Result<HttpResponse, HttpClientError> {
        let headers = self.build_headers();
        let resp = self
            .inner
            .request(Method::Delete, url, headers, Vec::new())
            .await?;

        self.capture_session_id(&resp.headers);
        self.check_error_response(resp)
    }

    /// Send a POST request and return a streaming response.
    ///
    /// Does NOT follow redirects. Used by Streamable HTTP mode where
    /// the server may return either `application/json` or `text/event-stream`.
    #[allow(dead_code)]
    pub async fn post_stream(
        &self,
        url: &str,
        body: &[u8],
        content_type: &str,
    ) -> Result<HttpStreamResponse, HttpClientError> {
        let mut headers = self.build_headers();
        headers.push(("Content-Type".to_owned(), content_type.to_owned()));

        let resp = self
            .inner
            .request_streaming(Method::Post, url, headers, body.to_vec())
            .await?;

        let status = resp.head.status;
        let resp_headers = resp.head.headers.clone();

        self.capture_session_id(&resp_headers);

        if status >= 400 {
            // For error responses on streaming, we can't easily read the body
            // without consuming the stream. Return the stream and let the caller
            // handle it based on status code.
        }

        Ok(HttpStreamResponse {
            status,
            headers: resp_headers,
            body: resp,
        })
    }

    /// Get the currently captured Mcp-Session-Id, if any.
    #[allow(dead_code)]
    pub fn session_id(&self) -> Option<String> {
        self.session_id.lock().unwrap().clone()
    }

    /// Manually set the Mcp-Session-Id (e.g., from a previous session).
    #[allow(dead_code)]
    pub fn set_session_id(&self, id: Option<String>) {
        *self.session_id.lock().unwrap() = id;
    }

    // ─── Internal ───────────────────────────────────────────────────

    /// Build the outgoing headers list: custom headers + session ID.
    #[allow(dead_code)]
    fn build_headers(&self) -> Vec<(String, String)> {
        let mut headers = self.custom_headers.clone();

        // Inject Mcp-Session-Id if captured from a previous response.
        if let Some(ref id) = *self.session_id.lock().unwrap() {
            // Remove any existing Mcp-Session-Id from custom headers (shouldn't
            // happen, but be defensive).
            headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Mcp-Session-Id"));
            headers.push(("Mcp-Session-Id".to_owned(), id.clone()));
        }

        headers
    }

    /// Capture Mcp-Session-Id from response headers.
    #[allow(dead_code)]
    fn capture_session_id(&self, headers: &[(String, String)]) {
        if let Some(id) = get_header(headers, "Mcp-Session-Id") {
            *self.session_id.lock().unwrap() = Some(id.to_owned());
        }
    }

    /// Check for error responses and extract error details.
    ///
    /// On non-2xx responses:
    /// - Content-Type application/json: return body as-is (caller parses JSON-RPC error)
    /// - Otherwise: wrap status + body text in HttpClientError
    #[allow(dead_code)]
    fn check_error_response(&self, resp: Response) -> Result<HttpResponse, HttpClientError> {
        let status = resp.status;
        let headers = resp.headers;
        let body = resp.body;

        if (200..300).contains(&status) {
            return Ok(HttpResponse {
                status,
                headers,
                body,
            });
        }

        // For JSON responses, return the response so callers can extract
        // JSON-RPC error details.
        let ct = get_header(&headers, "Content-Type");
        if ct.is_some_and(|ct| ct.contains("application/json")) {
            return Ok(HttpResponse {
                status,
                headers,
                body,
            });
        }

        // Non-JSON error: wrap as HttpClientError.
        let body_text = String::from_utf8_lossy(&body).into_owned();
        Err(HttpClientError::ServerError {
            status,
            body: body_text,
        })
    }

    /// GET with manual redirect following (up to MAX_GET_REDIRECTS).
    async fn get_with_redirects_streaming(
        &self,
        url: &str,
        headers: Vec<(String, String)>,
        redirect_count: u32,
    ) -> Result<ClientStreamingResponse<ClientIo>, HttpClientError> {
        let resp = self
            .inner
            .request_streaming(Method::Get, url, headers.clone(), Vec::new())
            .await?;

        let status = resp.head.status;
        if is_redirect(status) {
            if redirect_count >= MAX_GET_REDIRECTS {
                return Err(HttpClientError::TooManyRedirects {
                    count: redirect_count + 1,
                    max: MAX_GET_REDIRECTS,
                });
            }

            if let Some(location) = get_header(&resp.head.headers, "Location") {
                let next_url = resolve_redirect_url(url, location);
                // Drop the streaming response (closes connection) before following.
                drop(resp);
                return Box::pin(self.get_with_redirects_streaming(
                    &next_url,
                    headers,
                    redirect_count + 1,
                ))
                .await;
            }
        }

        Ok(resp)
    }
}

// ─── Utilities ─────────────────────────────────────────────────────

/// Case-insensitive header lookup.
#[allow(dead_code)]
fn get_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Check if status code is a redirect.
#[allow(dead_code)]
fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Resolve a redirect Location relative to the current URL.
#[allow(dead_code)]
fn resolve_redirect_url(current_url: &str, location: &str) -> String {
    // Absolute URL
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_owned();
    }

    // Parse current URL to get scheme + authority
    if let Ok(parsed) = ParsedUrl::parse(current_url) {
        let scheme = match parsed.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
        };

        // Protocol-relative
        if let Some(rest) = location.strip_prefix("//") {
            return format!("{scheme}://{rest}");
        }

        // Absolute path
        if location.starts_with('/') {
            return format!("{scheme}://{}:{}{location}", parsed.host, parsed.port);
        }

        // Relative path
        let base_path = parsed
            .path
            .rfind('/')
            .map_or("/", |i| &parsed.path[..=i]);
        return format!(
            "{scheme}://{}:{}{base_path}{location}",
            parsed.host, parsed.port
        );
    }

    // Fallback: treat as absolute
    location.to_owned()
}

// ─── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Header utilities ───────────────────────────────────────────

    #[test]
    fn get_header_case_insensitive() {
        let headers = vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("Mcp-Session-Id".to_owned(), "sess-123".to_owned()),
        ];
        assert_eq!(get_header(&headers, "content-type"), Some("application/json"));
        assert_eq!(get_header(&headers, "MCP-SESSION-ID"), Some("sess-123"));
        assert_eq!(get_header(&headers, "X-Missing"), None);
    }

    // ─── Redirect detection ─────────────────────────────────────────

    #[test]
    fn redirect_status_codes() {
        assert!(is_redirect(301));
        assert!(is_redirect(302));
        assert!(is_redirect(303));
        assert!(is_redirect(307));
        assert!(is_redirect(308));
        assert!(!is_redirect(200));
        assert!(!is_redirect(404));
        assert!(!is_redirect(304));
    }

    // ─── Redirect URL resolution ────────────────────────────────────

    #[test]
    fn resolve_absolute_url() {
        let result = resolve_redirect_url(
            "http://example.com/old",
            "https://other.com/new",
        );
        assert_eq!(result, "https://other.com/new");
    }

    #[test]
    fn resolve_protocol_relative() {
        let result = resolve_redirect_url(
            "https://example.com/old",
            "//cdn.example.com/asset",
        );
        assert_eq!(result, "https://cdn.example.com/asset");
    }

    #[test]
    fn resolve_absolute_path() {
        let result = resolve_redirect_url(
            "http://example.com:8080/old/page",
            "/new/page",
        );
        assert_eq!(result, "http://example.com:8080/new/page");
    }

    #[test]
    fn resolve_relative_path() {
        let result = resolve_redirect_url(
            "http://example.com/dir/old",
            "new",
        );
        assert_eq!(result, "http://example.com:80/dir/new");
    }

    // ─── HttpResponse helpers ───────────────────────────────────────

    #[test]
    fn http_response_content_type() {
        let resp = HttpResponse {
            status: 200,
            headers: vec![
                ("Content-Type".to_owned(), "text/event-stream".to_owned()),
            ],
            body: Vec::new(),
        };
        assert_eq!(resp.content_type(), Some("text/event-stream"));
    }

    #[test]
    fn http_response_session_id() {
        let resp = HttpResponse {
            status: 200,
            headers: vec![
                ("Mcp-Session-Id".to_owned(), "abc-123".to_owned()),
            ],
            body: Vec::new(),
        };
        assert_eq!(resp.mcp_session_id(), Some("abc-123"));
    }

    #[test]
    fn http_response_missing_headers() {
        let resp = HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: Vec::new(),
        };
        assert_eq!(resp.content_type(), None);
        assert_eq!(resp.mcp_session_id(), None);
    }

    // ─── Client construction ────────────────────────────────────────

    #[test]
    fn client_new_with_headers() {
        let headers = vec![
            Header {
                name: "X-Custom".into(),
                value: "val".into(),
            },
            Header {
                name: "Authorization".into(),
                value: "Bearer tok123".into(),
            },
        ];
        let client = HttpClient::new(&headers);
        assert_eq!(client.custom_headers.len(), 2);
        assert_eq!(client.custom_headers[0].0, "X-Custom");
        assert_eq!(client.custom_headers[1].1, "Bearer tok123");
    }

    #[test]
    fn client_new_empty_headers() {
        let client = HttpClient::new(&[]);
        assert!(client.custom_headers.is_empty());
    }

    // ─── Session ID management ──────────────────────────────────────

    #[test]
    fn session_id_initially_none() {
        let client = HttpClient::new(&[]);
        assert!(client.session_id().is_none());
    }

    #[test]
    fn session_id_set_and_get() {
        let client = HttpClient::new(&[]);
        client.set_session_id(Some("sess-abc".into()));
        assert_eq!(client.session_id(), Some("sess-abc".into()));
    }

    #[test]
    fn session_id_clear() {
        let client = HttpClient::new(&[]);
        client.set_session_id(Some("sess-abc".into()));
        client.set_session_id(None);
        assert!(client.session_id().is_none());
    }

    #[test]
    fn capture_session_id_from_headers() {
        let client = HttpClient::new(&[]);
        let headers = vec![
            ("Mcp-Session-Id".to_owned(), "new-sess".to_owned()),
        ];
        client.capture_session_id(&headers);
        assert_eq!(client.session_id(), Some("new-sess".into()));
    }

    #[test]
    fn capture_session_id_case_insensitive() {
        let client = HttpClient::new(&[]);
        let headers = vec![
            ("mcp-session-id".to_owned(), "lower-case".to_owned()),
        ];
        client.capture_session_id(&headers);
        assert_eq!(client.session_id(), Some("lower-case".into()));
    }

    #[test]
    fn capture_session_id_not_present_keeps_old() {
        let client = HttpClient::new(&[]);
        client.set_session_id(Some("old-sess".into()));
        let headers = vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
        ];
        client.capture_session_id(&headers);
        assert_eq!(client.session_id(), Some("old-sess".into()));
    }

    // ─── build_headers ──────────────────────────────────────────────

    #[test]
    fn build_headers_includes_custom() {
        let headers = vec![
            Header { name: "X-A".into(), value: "1".into() },
            Header { name: "X-B".into(), value: "2".into() },
        ];
        let client = HttpClient::new(&headers);
        let built = client.build_headers();
        assert_eq!(built.len(), 2);
        assert_eq!(built[0].0, "X-A");
        assert_eq!(built[1].0, "X-B");
    }

    #[test]
    fn build_headers_injects_session_id() {
        let client = HttpClient::new(&[]);
        client.set_session_id(Some("sess-xyz".into()));
        let built = client.build_headers();
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].0, "Mcp-Session-Id");
        assert_eq!(built[0].1, "sess-xyz");
    }

    #[test]
    fn build_headers_no_session_id_when_none() {
        let client = HttpClient::new(&[]);
        let built = client.build_headers();
        assert!(built.is_empty());
    }

    #[test]
    fn build_headers_deduplicates_session_id() {
        let headers = vec![
            Header {
                name: "Mcp-Session-Id".into(),
                value: "old-from-cli".into(),
            },
        ];
        let client = HttpClient::new(&headers);
        client.set_session_id(Some("captured-id".into()));
        let built = client.build_headers();
        // Should have only one Mcp-Session-Id, the captured one.
        let session_headers: Vec<_> = built
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("Mcp-Session-Id"))
            .collect();
        assert_eq!(session_headers.len(), 1);
        assert_eq!(session_headers[0].1, "captured-id");
    }

    // ─── Error response checking ────────────────────────────────────

    #[test]
    fn check_error_response_2xx_passes_through() {
        let client = HttpClient::new(&[]);
        let resp = Response::new(200, "OK", b"body".to_vec())
            .with_header("Content-Type", "application/json");
        let result = client.check_error_response(resp).unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(result.body, b"body");
    }

    #[test]
    fn check_error_response_json_error_passes_through() {
        let client = HttpClient::new(&[]);
        let resp = Response::new(400, "Bad Request", b"{\"error\":true}".to_vec())
            .with_header("Content-Type", "application/json");
        let result = client.check_error_response(resp).unwrap();
        assert_eq!(result.status, 400);
        assert_eq!(result.body, b"{\"error\":true}");
    }

    #[test]
    fn check_error_response_json_charset_passes_through() {
        let client = HttpClient::new(&[]);
        let resp =
            Response::new(500, "Internal Server Error", b"{\"jsonrpc\":\"2.0\"}".to_vec())
                .with_header("Content-Type", "application/json; charset=utf-8");
        let result = client.check_error_response(resp).unwrap();
        assert_eq!(result.status, 500);
    }

    #[test]
    fn check_error_response_non_json_error() {
        let client = HttpClient::new(&[]);
        let resp = Response::new(502, "Bad Gateway", b"upstream error".to_vec())
            .with_header("Content-Type", "text/plain");
        let err = client.check_error_response(resp).unwrap_err();
        match err {
            HttpClientError::ServerError { status, body } => {
                assert_eq!(status, 502);
                assert_eq!(body, "upstream error");
            }
            _ => panic!("expected ServerError, got {err:?}"),
        }
    }

    #[test]
    fn check_error_response_no_content_type_error() {
        let client = HttpClient::new(&[]);
        let resp = Response::new(503, "Service Unavailable", b"down".to_vec());
        let err = client.check_error_response(resp).unwrap_err();
        assert!(matches!(err, HttpClientError::ServerError { status: 503, .. }));
    }

    // ─── Error conversion ───────────────────────────────────────────

    #[test]
    fn client_error_to_http_client_error() {
        let err: HttpClientError = ClientError::InvalidUrl("bad".into()).into();
        assert!(matches!(err, HttpClientError::InvalidUrl(ref s) if s == "bad"));

        let err: HttpClientError =
            ClientError::TlsError("cert fail".into()).into();
        assert!(matches!(err, HttpClientError::Tls(ref s) if s == "cert fail"));

        let err: HttpClientError = ClientError::TooManyRedirects {
            count: 5,
            max: 10,
        }
        .into();
        assert!(matches!(
            err,
            HttpClientError::TooManyRedirects {
                count: 5,
                max: 10
            }
        ));
    }

    #[test]
    fn error_display() {
        let err = HttpClientError::InvalidUrl("bad url".into());
        assert!(format!("{err}").contains("bad url"));

        let err = HttpClientError::ServerError {
            status: 500,
            body: "oops".into(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("500"));
        assert!(msg.contains("oops"));

        let err = HttpClientError::TooManyRedirects { count: 5, max: 5 };
        let msg = format!("{err}");
        assert!(msg.contains("5"));
    }
}
