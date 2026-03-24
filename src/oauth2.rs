
//! OAuth2 authorization server for MCP transport endpoints.
//!
//! Implements server-side OAuth2 so MCP clients (e.g. Claude.ai) can authenticate
//! via OAuth2 before accessing MCP endpoints. Follows the MCP authorization spec.
//!
//! # Endpoints
//!
//! - `GET /.well-known/oauth-authorization-server` — RFC 8414 metadata
//! - `POST /token` — client_credentials + authorization_code grants
//! - `GET /authorize` — auto-approve, redirect with code (PKCE support)
//! - `POST /register` — RFC 7591 dynamic client registration
//!
//! # Design
//!
//! - Opaque tokens (UUID v4), not JWT — simpler, no signing keys needed
//! - Auto-approve on /authorize (no consent screen — infrastructure auth)
//! - Dynamic client registration stores in-memory (resets on restart)
//! - Lazy token expiry cleanup on access
//! - In-memory stores using `HashMap` + `RwLock` (read-heavy, write-rare)

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::cli::OAuth2ServerConfig;

// ─── Token store ────────────────────────────────────────────────────

#[allow(dead_code)]
struct StoredToken {
    #[allow(dead_code)]
    client_id: String,
    expires_at: Instant,
}

// ─── Authorization code store ───────────────────────────────────────

#[allow(dead_code)]
struct StoredAuthCode {
    client_id: String,
    redirect_uri: String,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    expires_at: Instant,
}

// ─── Dynamic client registration store ──────────────────────────────

#[allow(dead_code)]
struct RegisteredClient {
    client_id: String,
    client_secret: String,
    redirect_uris: Vec<String>,
    grant_types: Vec<String>,
    response_types: Vec<String>,
    token_endpoint_auth_method: String,
}

// ─── OAuth2Result ───────────────────────────────────────────────────

/// Result of processing a request through the OAuth2 handler.
///
/// Follows the same composable dispatch pattern as [`crate::cors::CorsResult`].
#[derive(Debug)]
#[allow(dead_code)]
pub enum OAuth2Result {
    /// This is an OAuth2 endpoint — return this HTTP response immediately.
    EndpointResponse(OAuth2Response),
    /// Not an OAuth2 endpoint and token is valid — continue to MCP handler.
    Passthrough(Result<(), OAuth2Error>),
}

/// HTTP response from the OAuth2 handler.
#[derive(Debug)]
#[allow(dead_code)]
pub struct OAuth2Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// OAuth2 authentication error.
#[derive(Debug)]
#[allow(dead_code)]
pub enum OAuth2Error {
    /// Missing or invalid Bearer token.
    Unauthorized(String),
}

// ─── OAuth2Response helpers ─────────────────────────────────────────

#[allow(dead_code)]
impl OAuth2Response {
    #[allow(dead_code)]
    fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![
                ("content-type".into(), "application/json".into()),
                ("cache-control".into(), "no-store".into()),
            ],
            body: body.as_bytes().to_vec(),
        }
    }

    #[allow(dead_code)]
    fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            headers: vec![("location".into(), location.to_string())],
            body: Vec::new(),
        }
    }

    #[allow(dead_code)]
    fn error(status: u16, error: &str, description: &str) -> Self {
        let body = serde_json::json!({
            "error": error,
            "error_description": description,
        });
        Self::json(status, &body.to_string())
    }

    #[allow(dead_code)]
    fn unauthorized(description: &str) -> Self {
        Self {
            status: 401,
            headers: vec![
                ("content-type".into(), "application/json".into()),
                ("cache-control".into(), "no-store".into()),
                (
                    "www-authenticate".into(),
                    format!("Bearer error=\"invalid_token\", error_description=\"{}\"", description),
                ),
            ],
            body: serde_json::json!({
                "error": "invalid_token",
                "error_description": description,
            })
            .to_string()
            .into_bytes(),
        }
    }
}

// ─── OAuth2Handler ──────────────────────────────────────────────────

/// Processes OAuth2 requests based on CLI configuration.
///
/// Composable with the gateway request handling: gateway handlers call
/// `process()` and either short-circuit (OAuth2 endpoint response) or
/// continue to MCP handling (token validated).
#[allow(dead_code)]
pub struct OAuth2Handler {
    config: OAuth2ServerConfig,
    tokens: RwLock<HashMap<String, StoredToken>>,
    auth_codes: RwLock<HashMap<String, StoredAuthCode>>,
    registered_clients: RwLock<HashMap<String, RegisteredClient>>,
}

#[allow(dead_code)]
impl OAuth2Handler {
    /// Create a new OAuth2 handler with the given configuration.
    #[allow(dead_code)]
    pub fn new(config: OAuth2ServerConfig) -> Self {
        Self {
            config,
            tokens: RwLock::new(HashMap::new()),
            auth_codes: RwLock::new(HashMap::new()),
            registered_clients: RwLock::new(HashMap::new()),
        }
    }

    /// Process an incoming request.
    ///
    /// The gateway should:
    /// - `EndpointResponse`: return this response immediately (OAuth2 endpoint)
    /// - `Passthrough(Ok(()))`: token valid, continue to MCP handler
    /// - `Passthrough(Err(_))`: token invalid, return 401
    #[allow(dead_code)]
    pub fn process(
        &self,
        method: &str,
        path: &str,
        headers: &HashMap<String, String>,
        body: &[u8],
        query: &HashMap<String, String>,
    ) -> OAuth2Result {
        // OAuth2 metadata endpoint
        if path == "/.well-known/oauth-authorization-server" && method.eq_ignore_ascii_case("GET") {
            return OAuth2Result::EndpointResponse(self.handle_metadata(headers));
        }

        // Token endpoint
        if path == "/token" && method.eq_ignore_ascii_case("POST") {
            return OAuth2Result::EndpointResponse(self.handle_token(body));
        }

        // Authorization endpoint
        if path == "/authorize" && method.eq_ignore_ascii_case("GET") {
            return OAuth2Result::EndpointResponse(self.handle_authorize(query));
        }

        // Dynamic client registration endpoint
        if path == "/register" && method.eq_ignore_ascii_case("POST") {
            return OAuth2Result::EndpointResponse(self.handle_register(body));
        }

        // All other paths: validate Bearer token
        OAuth2Result::Passthrough(self.validate_bearer(headers))
    }

    // ─── Metadata endpoint ──────────────────────────────────────────

    #[allow(dead_code)]
    fn handle_metadata(&self, headers: &HashMap<String, String>) -> OAuth2Response {
        let issuer = self.resolve_issuer(headers);

        let metadata = serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{}/authorize", issuer),
            "token_endpoint": format!("{}/token", issuer),
            "registration_endpoint": format!("{}/register", issuer),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "client_credentials"],
            "token_endpoint_auth_methods_supported": ["client_secret_post"],
            "code_challenge_methods_supported": ["S256"],
            "scopes_supported": ["mcp"],
        });

        OAuth2Response::json(200, &metadata.to_string())
    }

    // ─── Token endpoint ─────────────────────────────────────────────

    #[allow(dead_code)]
    fn handle_token(&self, body: &[u8]) -> OAuth2Response {
        let body_str = match std::str::from_utf8(body) {
            Ok(s) => s,
            Err(_) => return OAuth2Response::error(400, "invalid_request", "invalid UTF-8 body"),
        };

        let params = parse_form_urlencoded(body_str);

        let grant_type = match params.get("grant_type") {
            Some(gt) => gt.as_str(),
            None => {
                return OAuth2Response::error(400, "invalid_request", "missing grant_type");
            }
        };

        match grant_type {
            "client_credentials" => self.handle_client_credentials(&params),
            "authorization_code" => self.handle_authorization_code(&params),
            _ => OAuth2Response::error(400, "unsupported_grant_type", "unsupported grant_type"),
        }
    }

    #[allow(dead_code)]
    fn handle_client_credentials(
        &self,
        params: &HashMap<String, String>,
    ) -> OAuth2Response {
        let client_id = params.get("client_id").map(|s| s.as_str()).unwrap_or("");
        let client_secret = params.get("client_secret").map(|s| s.as_str()).unwrap_or("");

        if !self.verify_client(client_id, client_secret) {
            return OAuth2Response::error(401, "invalid_client", "invalid client credentials");
        }

        self.issue_token(client_id)
    }

    #[allow(dead_code)]
    fn handle_authorization_code(
        &self,
        params: &HashMap<String, String>,
    ) -> OAuth2Response {
        let code = match params.get("code") {
            Some(c) => c.clone(),
            None => return OAuth2Response::error(400, "invalid_request", "missing code"),
        };

        let client_id = params.get("client_id").map(|s| s.as_str()).unwrap_or("");
        let client_secret = params.get("client_secret").map(|s| s.as_str()).unwrap_or("");

        if !self.verify_client(client_id, client_secret) {
            return OAuth2Response::error(401, "invalid_client", "invalid client credentials");
        }

        // Look up and remove the authorization code
        let stored = {
            let mut codes = self.auth_codes.write().unwrap();
            codes.remove(&code)
        };

        let stored = match stored {
            Some(s) => s,
            None => return OAuth2Response::error(400, "invalid_grant", "invalid or expired authorization code"),
        };

        // Check expiry
        if Instant::now() > stored.expires_at {
            return OAuth2Response::error(400, "invalid_grant", "authorization code expired");
        }

        // Check client_id matches
        if stored.client_id != client_id {
            return OAuth2Response::error(400, "invalid_grant", "client_id mismatch");
        }

        // Check redirect_uri matches if provided
        if let Some(redirect_uri) = params.get("redirect_uri") {
            if *redirect_uri != stored.redirect_uri {
                return OAuth2Response::error(400, "invalid_grant", "redirect_uri mismatch");
            }
        }

        // Verify PKCE code_verifier if code_challenge was stored
        if let Some(ref challenge) = stored.code_challenge {
            let verifier = match params.get("code_verifier") {
                Some(v) => v,
                None => {
                    return OAuth2Response::error(
                        400,
                        "invalid_grant",
                        "missing code_verifier (PKCE required)",
                    );
                }
            };

            let method = stored.code_challenge_method.as_deref().unwrap_or("S256");
            if method != "S256" {
                return OAuth2Response::error(
                    400,
                    "invalid_request",
                    "only S256 code_challenge_method is supported",
                );
            }

            if !verify_pkce_s256(verifier, challenge) {
                return OAuth2Response::error(400, "invalid_grant", "PKCE verification failed");
            }
        }

        self.issue_token(client_id)
    }

    // ─── Authorization endpoint ─────────────────────────────────────

    #[allow(dead_code)]
    fn handle_authorize(&self, query: &HashMap<String, String>) -> OAuth2Response {
        let client_id = match query.get("client_id") {
            Some(c) => c.clone(),
            None => {
                return OAuth2Response::error(400, "invalid_request", "missing client_id");
            }
        };

        let redirect_uri = match query.get("redirect_uri") {
            Some(r) => r.clone(),
            None => {
                return OAuth2Response::error(400, "invalid_request", "missing redirect_uri");
            }
        };

        let response_type = query.get("response_type").map(|s| s.as_str()).unwrap_or("");
        if response_type != "code" {
            return OAuth2Response::error(400, "unsupported_response_type", "only 'code' is supported");
        }

        // Verify client_id is known (either the configured one or a dynamically registered one)
        if client_id != self.config.client_id {
            let clients = self.registered_clients.read().unwrap();
            if !clients.contains_key(&client_id) {
                return OAuth2Response::error(400, "invalid_request", "unknown client_id");
            }
        }

        // Extract PKCE parameters
        let code_challenge = query.get("code_challenge").cloned();
        let code_challenge_method = query.get("code_challenge_method").cloned();

        // If code_challenge is present, method must be S256
        if code_challenge.is_some() {
            let method = code_challenge_method.as_deref().unwrap_or("plain");
            if method != "S256" {
                return OAuth2Response::error(
                    400,
                    "invalid_request",
                    "only S256 code_challenge_method is supported",
                );
            }
        }

        // Generate authorization code (auto-approve: no consent screen)
        let auth_code = Uuid::new_v4().to_string();

        // Store it
        {
            let mut codes = self.auth_codes.write().unwrap();
            // Lazy cleanup of expired codes
            codes.retain(|_, v| Instant::now() < v.expires_at);
            codes.insert(
                auth_code.clone(),
                StoredAuthCode {
                    client_id,
                    redirect_uri: redirect_uri.clone(),
                    code_challenge,
                    code_challenge_method,
                    expires_at: Instant::now() + Duration::from_secs(600), // 10 minute expiry
                },
            );
        }

        // Build redirect URL
        let state = query.get("state");
        let separator = if redirect_uri.contains('?') { '&' } else { '?' };
        let mut location = format!("{redirect_uri}{separator}code={auth_code}");
        if let Some(state) = state {
            location.push_str(&format!("&state={state}"));
        }

        OAuth2Response::redirect(&location)
    }

    // ─── Dynamic client registration ────────────────────────────────

    #[allow(dead_code)]
    fn handle_register(&self, body: &[u8]) -> OAuth2Response {
        let body_str = match std::str::from_utf8(body) {
            Ok(s) => s,
            Err(_) => return OAuth2Response::error(400, "invalid_request", "invalid UTF-8 body"),
        };

        let parsed: serde_json::Value = match serde_json::from_str(body_str) {
            Ok(v) => v,
            Err(_) => return OAuth2Response::error(400, "invalid_request", "invalid JSON body"),
        };

        let redirect_uris = parsed
            .get("redirect_uris")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if redirect_uris.is_empty() {
            return OAuth2Response::error(
                400,
                "invalid_client_metadata",
                "redirect_uris is required and must not be empty",
            );
        }

        let grant_types = parsed
            .get("grant_types")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec!["authorization_code".to_string()]);

        let response_types = parsed
            .get("response_types")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec!["code".to_string()]);

        let token_endpoint_auth_method = parsed
            .get("token_endpoint_auth_method")
            .and_then(|v| v.as_str())
            .unwrap_or("client_secret_post")
            .to_string();

        // Generate client credentials
        let new_client_id = Uuid::new_v4().to_string();
        let new_client_secret = Uuid::new_v4().to_string();

        let client = RegisteredClient {
            client_id: new_client_id.clone(),
            client_secret: new_client_secret.clone(),
            redirect_uris: redirect_uris.clone(),
            grant_types: grant_types.clone(),
            response_types: response_types.clone(),
            token_endpoint_auth_method: token_endpoint_auth_method.clone(),
        };

        {
            let mut clients = self.registered_clients.write().unwrap();
            clients.insert(new_client_id.clone(), client);
        }

        let resp_body = serde_json::json!({
            "client_id": new_client_id,
            "client_secret": new_client_secret,
            "redirect_uris": redirect_uris,
            "grant_types": grant_types,
            "response_types": response_types,
            "token_endpoint_auth_method": token_endpoint_auth_method,
        });

        OAuth2Response::json(201, &resp_body.to_string())
    }

    // ─── Token validation ───────────────────────────────────────────

    /// Validate the Bearer token from the Authorization header.
    #[allow(dead_code)]
    fn validate_bearer(&self, headers: &HashMap<String, String>) -> Result<(), OAuth2Error> {
        let auth_header = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.as_str());

        let token = match auth_header {
            Some(h) if h.starts_with("Bearer ") => &h[7..],
            Some(h) if h.starts_with("bearer ") => &h[7..],
            Some(_) => {
                return Err(OAuth2Error::Unauthorized(
                    "Authorization header must use Bearer scheme".into(),
                ));
            }
            None => {
                return Err(OAuth2Error::Unauthorized(
                    "missing Authorization header".into(),
                ));
            }
        };

        let tokens = self.tokens.read().unwrap();
        match tokens.get(token) {
            Some(stored) if Instant::now() < stored.expires_at => Ok(()),
            Some(_) => Err(OAuth2Error::Unauthorized("token expired".into())),
            None => Err(OAuth2Error::Unauthorized("invalid token".into())),
        }
    }

    // ─── Helpers ────────────────────────────────────────────────────

    /// Verify client credentials against configured or registered clients.
    #[allow(dead_code)]
    fn verify_client(&self, client_id: &str, client_secret: &str) -> bool {
        // Check configured client first
        if client_id == self.config.client_id && client_secret == self.config.client_secret {
            return true;
        }

        // Check dynamically registered clients
        let clients = self.registered_clients.read().unwrap();
        if let Some(client) = clients.get(client_id) {
            return client.client_secret == client_secret;
        }

        false
    }

    /// Issue a new access token for the given client.
    #[allow(dead_code)]
    fn issue_token(&self, client_id: &str) -> OAuth2Response {
        let token = Uuid::new_v4().to_string();
        let expiry = Duration::from_secs(self.config.token_expiry_secs);

        {
            let mut tokens = self.tokens.write().unwrap();
            // Lazy cleanup of expired tokens
            tokens.retain(|_, v| Instant::now() < v.expires_at);
            tokens.insert(
                token.clone(),
                StoredToken {
                    client_id: client_id.to_string(),
                    expires_at: Instant::now() + expiry,
                },
            );
        }

        let body = serde_json::json!({
            "access_token": token,
            "token_type": "Bearer",
            "expires_in": self.config.token_expiry_secs,
            "scope": "mcp",
        });

        OAuth2Response::json(200, &body.to_string())
    }

    /// Resolve the issuer URL from configuration or request headers.
    #[allow(dead_code)]
    fn resolve_issuer(&self, headers: &HashMap<String, String>) -> String {
        if let Some(ref issuer) = self.config.issuer {
            return issuer.clone();
        }

        // Auto-detect from X-Forwarded-Proto and Host headers
        let proto = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-forwarded-proto"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("http");

        let host = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("host"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("localhost");

        format!("{proto}://{host}")
    }
}

// ─── PKCE S256 verification ─────────────────────────────────────────

/// Verify PKCE S256: code_challenge == BASE64URL(SHA256(code_verifier))
#[allow(dead_code)]
fn verify_pkce_s256(code_verifier: &str, code_challenge: &str) -> bool {
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let hash = hasher.finalize();
    let computed = URL_SAFE_NO_PAD.encode(hash);
    computed == code_challenge
}

// ─── Form URL-encoded parser ────────────────────────────────────────

/// Parse application/x-www-form-urlencoded body into a HashMap.
#[allow(dead_code)]
fn parse_form_urlencoded(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter_map(|pair| {
            if pair.is_empty() {
                return None;
            }
            let mut kv = pair.splitn(2, '=');
            let k = kv.next()?.to_string();
            let v = kv.next().unwrap_or("").to_string();
            // Basic percent-decoding for common OAuth2 values
            let k = percent_decode(&k);
            let v = percent_decode(&v);
            if k.is_empty() {
                None
            } else {
                Some((k, v))
            }
        })
        .collect()
}

/// Minimal percent-decoding for form-urlencoded values.
#[allow(dead_code)]
fn percent_decode(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == '+' {
            result.push(' ');
        } else if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if hex.len() == 2 {
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    result.push(byte as char);
                } else {
                    result.push('%');
                    result.push_str(&hex);
                }
            } else {
                result.push('%');
                result.push_str(&hex);
            }
        } else {
            result.push(c);
        }
    }
    result
}

// ─── Convenience for gateway integration ────────────────────────────

/// Build a 401 Unauthorized response from an OAuth2Error.
///
/// Used by gateway code that receives `Passthrough(Err(e))`.
#[allow(dead_code)]
pub fn unauthorized_response(err: &OAuth2Error) -> OAuth2Response {
    match err {
        OAuth2Error::Unauthorized(desc) => OAuth2Response::unauthorized(desc),
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> OAuth2ServerConfig {
        OAuth2ServerConfig {
            client_id: "test-client".into(),
            client_secret: "test-secret".into(),
            token_expiry_secs: 3600,
            issuer: Some("https://example.com".into()),
        }
    }

    fn make_headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn empty_query() -> HashMap<String, String> {
        HashMap::new()
    }

    // ─── Metadata ───────────────────────────────────────────────────

    #[test]
    fn metadata_endpoint_returns_json() {
        let handler = OAuth2Handler::new(test_config());
        let headers = make_headers(&[("host", "example.com")]);
        let result = handler.process("GET", "/.well-known/oauth-authorization-server", &headers, &[], &empty_query());
        match result {
            OAuth2Result::EndpointResponse(resp) => {
                assert_eq!(resp.status, 200);
                let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
                assert_eq!(body["issuer"], "https://example.com");
                assert!(body["token_endpoint"].as_str().unwrap().contains("/token"));
                assert!(body["authorization_endpoint"].as_str().unwrap().contains("/authorize"));
                assert!(body["registration_endpoint"].as_str().unwrap().contains("/register"));
            }
            _ => panic!("expected EndpointResponse"),
        }
    }

    #[test]
    fn metadata_auto_issuer_from_headers() {
        let config = OAuth2ServerConfig {
            client_id: "test".into(),
            client_secret: "secret".into(),
            token_expiry_secs: 3600,
            issuer: None, // auto-detect
        };
        let handler = OAuth2Handler::new(config);
        let headers = make_headers(&[
            ("host", "myserver.example.com:8000"),
            ("x-forwarded-proto", "https"),
        ]);
        let result = handler.process("GET", "/.well-known/oauth-authorization-server", &headers, &[], &empty_query());
        match result {
            OAuth2Result::EndpointResponse(resp) => {
                let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
                assert_eq!(body["issuer"], "https://myserver.example.com:8000");
            }
            _ => panic!("expected EndpointResponse"),
        }
    }

    // ─── Client credentials grant ───────────────────────────────────

    #[test]
    fn client_credentials_grant_success() {
        let handler = OAuth2Handler::new(test_config());
        let body = b"grant_type=client_credentials&client_id=test-client&client_secret=test-secret";
        let result = handler.process("POST", "/token", &make_headers(&[]), body, &empty_query());
        match result {
            OAuth2Result::EndpointResponse(resp) => {
                assert_eq!(resp.status, 200);
                let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
                assert!(body["access_token"].as_str().is_some());
                assert_eq!(body["token_type"], "Bearer");
                assert_eq!(body["expires_in"], 3600);
            }
            _ => panic!("expected EndpointResponse"),
        }
    }

    #[test]
    fn client_credentials_bad_secret() {
        let handler = OAuth2Handler::new(test_config());
        let body = b"grant_type=client_credentials&client_id=test-client&client_secret=wrong";
        let result = handler.process("POST", "/token", &make_headers(&[]), body, &empty_query());
        match result {
            OAuth2Result::EndpointResponse(resp) => {
                assert_eq!(resp.status, 401);
            }
            _ => panic!("expected EndpointResponse"),
        }
    }

    // ─── Token validation ───────────────────────────────────────────

    #[test]
    fn valid_token_passthrough() {
        let handler = OAuth2Handler::new(test_config());

        // Get a token first
        let body = b"grant_type=client_credentials&client_id=test-client&client_secret=test-secret";
        let result = handler.process("POST", "/token", &make_headers(&[]), body, &empty_query());
        let token = match result {
            OAuth2Result::EndpointResponse(resp) => {
                let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
                body["access_token"].as_str().unwrap().to_string()
            }
            _ => panic!("expected EndpointResponse"),
        };

        // Use token on MCP path
        let headers = make_headers(&[("authorization", &format!("Bearer {token}"))]);
        let result = handler.process("POST", "/mcp", &headers, &[], &empty_query());
        match result {
            OAuth2Result::Passthrough(Ok(())) => {} // success
            _ => panic!("expected Passthrough(Ok)"),
        }
    }

    #[test]
    fn missing_token_returns_error() {
        let handler = OAuth2Handler::new(test_config());
        let result = handler.process("POST", "/mcp", &make_headers(&[]), &[], &empty_query());
        match result {
            OAuth2Result::Passthrough(Err(OAuth2Error::Unauthorized(_))) => {}
            _ => panic!("expected Passthrough(Err)"),
        }
    }

    #[test]
    fn invalid_token_returns_error() {
        let handler = OAuth2Handler::new(test_config());
        let headers = make_headers(&[("authorization", "Bearer invalid-token")]);
        let result = handler.process("POST", "/mcp", &headers, &[], &empty_query());
        match result {
            OAuth2Result::Passthrough(Err(OAuth2Error::Unauthorized(_))) => {}
            _ => panic!("expected Passthrough(Err)"),
        }
    }

    // ─── Authorization code flow ────────────────────────────────────

    #[test]
    fn authorization_code_flow() {
        let handler = OAuth2Handler::new(test_config());

        // Step 1: GET /authorize
        let mut query = HashMap::new();
        query.insert("client_id".into(), "test-client".into());
        query.insert("redirect_uri".into(), "https://example.com/callback".into());
        query.insert("response_type".into(), "code".into());
        query.insert("state".into(), "mystate".into());

        let result = handler.process("GET", "/authorize", &make_headers(&[]), &[], &query);
        let location = match result {
            OAuth2Result::EndpointResponse(resp) => {
                assert_eq!(resp.status, 302);
                resp.headers
                    .iter()
                    .find(|(k, _)| k == "location")
                    .map(|(_, v)| v.clone())
                    .unwrap()
            }
            _ => panic!("expected EndpointResponse"),
        };

        // Extract the code from redirect
        assert!(location.starts_with("https://example.com/callback?code="));
        assert!(location.contains("&state=mystate"));
        let code = location
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();

        // Step 2: POST /token with the code
        let body = format!(
            "grant_type=authorization_code&code={}&client_id=test-client&client_secret=test-secret&redirect_uri=https%3A%2F%2Fexample.com%2Fcallback",
            code
        );
        let result = handler.process("POST", "/token", &make_headers(&[]), body.as_bytes(), &empty_query());
        match result {
            OAuth2Result::EndpointResponse(resp) => {
                assert_eq!(resp.status, 200);
                let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
                assert!(body["access_token"].as_str().is_some());
            }
            _ => panic!("expected EndpointResponse"),
        }
    }

    // ─── PKCE S256 ──────────────────────────────────────────────────

    #[test]
    fn pkce_s256_flow() {
        let handler = OAuth2Handler::new(test_config());

        // Generate PKCE pair
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

        // Step 1: GET /authorize with PKCE
        let mut query = HashMap::new();
        query.insert("client_id".into(), "test-client".into());
        query.insert("redirect_uri".into(), "https://example.com/callback".into());
        query.insert("response_type".into(), "code".into());
        query.insert("code_challenge".into(), challenge);
        query.insert("code_challenge_method".into(), "S256".into());

        let result = handler.process("GET", "/authorize", &make_headers(&[]), &[], &query);
        let location = match result {
            OAuth2Result::EndpointResponse(resp) => {
                assert_eq!(resp.status, 302);
                resp.headers.iter().find(|(k, _)| k == "location").unwrap().1.clone()
            }
            _ => panic!("expected redirect"),
        };

        let code = location.split("code=").nth(1).unwrap().split('&').next().unwrap();

        // Step 2: POST /token with code_verifier
        let body = format!(
            "grant_type=authorization_code&code={}&client_id=test-client&client_secret=test-secret&redirect_uri=https%3A%2F%2Fexample.com%2Fcallback&code_verifier={}",
            code, verifier
        );
        let result = handler.process("POST", "/token", &make_headers(&[]), body.as_bytes(), &empty_query());
        match result {
            OAuth2Result::EndpointResponse(resp) => {
                assert_eq!(resp.status, 200);
            }
            _ => panic!("expected EndpointResponse"),
        }
    }

    #[test]
    fn pkce_wrong_verifier_fails() {
        let handler = OAuth2Handler::new(test_config());

        let verifier = "correct-verifier-value";
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

        let mut query = HashMap::new();
        query.insert("client_id".into(), "test-client".into());
        query.insert("redirect_uri".into(), "https://example.com/callback".into());
        query.insert("response_type".into(), "code".into());
        query.insert("code_challenge".into(), challenge);
        query.insert("code_challenge_method".into(), "S256".into());

        let result = handler.process("GET", "/authorize", &make_headers(&[]), &[], &query);
        let location = match result {
            OAuth2Result::EndpointResponse(resp) => {
                resp.headers.iter().find(|(k, _)| k == "location").unwrap().1.clone()
            }
            _ => panic!("expected redirect"),
        };
        let code = location.split("code=").nth(1).unwrap().split('&').next().unwrap();

        // Use wrong verifier
        let body = format!(
            "grant_type=authorization_code&code={}&client_id=test-client&client_secret=test-secret&redirect_uri=https%3A%2F%2Fexample.com%2Fcallback&code_verifier=wrong-verifier",
            code
        );
        let result = handler.process("POST", "/token", &make_headers(&[]), body.as_bytes(), &empty_query());
        match result {
            OAuth2Result::EndpointResponse(resp) => {
                assert_eq!(resp.status, 400);
                let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
                assert_eq!(body["error"], "invalid_grant");
            }
            _ => panic!("expected EndpointResponse"),
        }
    }

    // ─── Dynamic client registration ────────────────────────────────

    #[test]
    fn dynamic_registration_and_use() {
        let handler = OAuth2Handler::new(test_config());

        // Register a new client
        let reg_body = serde_json::json!({
            "redirect_uris": ["https://newclient.example.com/callback"],
            "grant_types": ["authorization_code", "client_credentials"],
        });
        let result = handler.process(
            "POST",
            "/register",
            &make_headers(&[]),
            reg_body.to_string().as_bytes(),
            &empty_query(),
        );
        let (new_client_id, new_client_secret) = match result {
            OAuth2Result::EndpointResponse(resp) => {
                assert_eq!(resp.status, 201);
                let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
                (
                    body["client_id"].as_str().unwrap().to_string(),
                    body["client_secret"].as_str().unwrap().to_string(),
                )
            }
            _ => panic!("expected EndpointResponse"),
        };

        // Use the new client for client_credentials
        let body = format!(
            "grant_type=client_credentials&client_id={}&client_secret={}",
            new_client_id, new_client_secret
        );
        let result = handler.process("POST", "/token", &make_headers(&[]), body.as_bytes(), &empty_query());
        match result {
            OAuth2Result::EndpointResponse(resp) => {
                assert_eq!(resp.status, 200);
            }
            _ => panic!("expected EndpointResponse"),
        }
    }

    #[test]
    fn registration_missing_redirect_uris() {
        let handler = OAuth2Handler::new(test_config());
        let body = serde_json::json!({}).to_string();
        let result = handler.process("POST", "/register", &make_headers(&[]), body.as_bytes(), &empty_query());
        match result {
            OAuth2Result::EndpointResponse(resp) => {
                assert_eq!(resp.status, 400);
            }
            _ => panic!("expected EndpointResponse"),
        }
    }

    // ─── Unsupported grant type ─────────────────────────────────────

    #[test]
    fn unsupported_grant_type() {
        let handler = OAuth2Handler::new(test_config());
        let body = b"grant_type=refresh_token&refresh_token=abc";
        let result = handler.process("POST", "/token", &make_headers(&[]), body, &empty_query());
        match result {
            OAuth2Result::EndpointResponse(resp) => {
                assert_eq!(resp.status, 400);
                let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
                assert_eq!(body["error"], "unsupported_grant_type");
            }
            _ => panic!("expected EndpointResponse"),
        }
    }

    // ─── PKCE utility ───────────────────────────────────────────────

    #[test]
    fn pkce_s256_verification() {
        // RFC 7636 Appendix B example (adjusted)
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

        assert!(verify_pkce_s256(verifier, &challenge));
        assert!(!verify_pkce_s256("wrong-verifier", &challenge));
    }

    // ─── Form URL-encoded parsing ───────────────────────────────────

    #[test]
    fn parse_form_urlencoded_basic() {
        let params = parse_form_urlencoded("grant_type=client_credentials&client_id=test");
        assert_eq!(params.get("grant_type").unwrap(), "client_credentials");
        assert_eq!(params.get("client_id").unwrap(), "test");
    }

    #[test]
    fn parse_form_urlencoded_percent_decode() {
        let params = parse_form_urlencoded("redirect_uri=https%3A%2F%2Fexample.com%2Fcallback");
        assert_eq!(
            params.get("redirect_uri").unwrap(),
            "https://example.com/callback"
        );
    }

    #[test]
    fn parse_form_urlencoded_empty() {
        let params = parse_form_urlencoded("");
        assert!(params.is_empty());
    }
}
