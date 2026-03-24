
use std::process;

use clap::Parser;
use regex::Regex;

/// Output transport choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum OutputTransport {
    Stdio,
    Sse,
    Ws,
    StreamableHttp,
}

#[allow(dead_code)]
impl std::fmt::Display for OutputTransport {
    #[allow(dead_code)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdio => write!(f, "stdio"),
            Self::Sse => write!(f, "sse"),
            Self::Ws => write!(f, "ws"),
            Self::StreamableHttp => write!(f, "streamableHttp"),
        }
    }
}

#[allow(dead_code)]
impl std::str::FromStr for OutputTransport {
    type Err = String;
    #[allow(dead_code)]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "stdio" => Ok(Self::Stdio),
            "sse" => Ok(Self::Sse),
            "ws" => Ok(Self::Ws),
            "streamableHttp" => Ok(Self::StreamableHttp),
            _ => Err(format!(
                "invalid output transport '{s}': expected stdio|sse|ws|streamableHttp"
            )),
        }
    }
}

/// Which input source was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum InputMode {
    Stdio,
    Sse,
    StreamableHttp,
}

/// Log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum LogLevel {
    Debug,
    #[default]
    Info,
    None,
}

#[allow(dead_code)]
impl std::fmt::Display for LogLevel {
    #[allow(dead_code)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Debug => write!(f, "debug"),
            Self::Info => write!(f, "info"),
            Self::None => write!(f, "none"),
        }
    }
}

#[allow(dead_code)]
impl std::str::FromStr for LogLevel {
    type Err = String;
    #[allow(dead_code)]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "debug" => Ok(Self::Debug),
            "info" => Ok(Self::Info),
            "none" => Ok(Self::None),
            _ => Err(format!(
                "invalid log level '{s}': expected debug|info|none"
            )),
        }
    }
}

/// A parsed header (name, value).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// A CORS origin — either a literal string or a regex pattern.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum CorsOrigin {
    Literal(String),
    Regex(Regex),
}

/// OAuth2 server configuration from CLI flags.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct OAuth2ServerConfig {
    pub client_id: String,
    pub client_secret: String,
    pub token_expiry_secs: u64,
    pub issuer: Option<String>,
}

/// Three-state CORS configuration.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum CorsConfig {
    /// --cors absent: CORS disabled entirely.
    Disabled,
    /// --cors present with no values: wildcard `*`.
    Wildcard,
    /// --cors origin1 origin2: specific origins (may include regex).
    Origins(Vec<CorsOrigin>),
}

/// Raw CLI arguments parsed by clap.
#[derive(Parser, Debug)]
#[command(
    name = "supergateway",
    about = "MCP transport bridge — stdio/SSE/WebSocket/Streamable HTTP",
    version
)]
#[command(rename_all = "camelCase")]
#[allow(dead_code)]
struct RawArgs {
    /// Spawn a child process as MCP stdio server (shell command)
    #[arg(long)]
    stdio: Option<String>,

    /// Connect to a remote MCP SSE server URL
    #[arg(long)]
    sse: Option<String>,

    /// Connect to a remote MCP Streamable HTTP server URL
    #[arg(long = "streamableHttp")]
    streamable_http: Option<String>,

    /// Output transport (stdio|sse|ws|streamableHttp)
    #[arg(long = "outputTransport")]
    output_transport: Option<OutputTransport>,

    /// Server port
    #[arg(long, default_value = "8000")]
    port: u16,

    /// Base URL prefix for SSE endpoint
    #[arg(long = "baseUrl", default_value = "")]
    base_url: String,

    /// SSE event stream path
    #[arg(long = "ssePath", default_value = "/sse")]
    sse_path: String,

    /// Message POST path
    #[arg(long = "messagePath", default_value = "/message")]
    message_path: String,

    /// Streamable HTTP path
    #[arg(long = "streamableHttpPath", default_value = "/mcp")]
    streamable_http_path: String,

    /// Log level (debug|info|none)
    #[arg(long = "logLevel", default_value = "info")]
    log_level: LogLevel,

    /// CORS origins (absent=disabled, no values=wildcard, with values=specific)
    #[arg(long, num_args(0..))]
    cors: Option<Vec<String>>,

    /// Health endpoint paths
    #[arg(long = "healthEndpoint", num_args(0..))]
    health_endpoint: Option<Vec<String>>,

    /// Custom headers (format: "Name: Value")
    #[arg(long = "header")]
    header: Vec<String>,

    /// OAuth2 Bearer token
    #[arg(long = "oauth2Bearer")]
    oauth2_bearer: Option<String>,

    /// OAuth2 server: client ID (enables OAuth2 server mode)
    #[arg(long = "oauth2-server-client-id")]
    oauth2_server_client_id: Option<String>,

    /// OAuth2 server: client secret (required with --oauth2-server-client-id)
    #[arg(long = "oauth2-server-client-secret")]
    oauth2_server_client_secret: Option<String>,

    /// OAuth2 server: token expiry in seconds (default 3600)
    #[arg(long = "oauth2-token-expiry", default_value = "3600")]
    oauth2_token_expiry: u64,

    /// OAuth2 server: issuer URL for metadata (default: auto from Host header)
    #[arg(long = "oauth2-issuer")]
    oauth2_issuer: Option<String>,

    /// Use stateful sessions (stdio->Streamable HTTP only)
    #[arg(long)]
    stateful: bool,

    /// Session timeout in milliseconds (stateful HTTP only)
    #[arg(long = "sessionTimeout")]
    session_timeout: Option<i64>,

    /// MCP protocol version for stateless HTTP initialization
    #[arg(long = "protocolVersion", default_value = "2024-11-05")]
    protocol_version: String,
}

/// Validated, ready-to-use configuration.
#[derive(Debug)]
#[allow(dead_code)]
pub struct Config {
    pub input_mode: InputMode,
    pub input_value: String,
    pub output_transport: OutputTransport,
    pub port: u16,
    pub base_url: String,
    pub sse_path: String,
    pub message_path: String,
    pub streamable_http_path: String,
    pub log_level: LogLevel,
    pub cors: CorsConfig,
    pub health_endpoints: Vec<String>,
    pub headers: Vec<Header>,
    pub stateful: bool,
    pub session_timeout: Option<u64>,
    pub protocol_version: String,
    pub oauth2_server: Option<OAuth2ServerConfig>,
}

#[allow(dead_code)]
impl Config {
    #[allow(dead_code)]
    pub fn parse() -> Self {
        let raw = RawArgs::parse();
        match Self::validate(raw) {
            Ok(config) => config,
            Err(msg) => {
                eprintln!("error: {msg}");
                process::exit(1);
            }
        }
    }

    #[allow(dead_code)]
    fn validate(raw: RawArgs) -> Result<Self, String> {
        // Determine input mode (exactly one required).
        let input_count = [
            raw.stdio.is_some(),
            raw.sse.is_some(),
            raw.streamable_http.is_some(),
        ]
        .iter()
        .filter(|&&x| x)
        .count();

        if input_count == 0 {
            return Err(
                "exactly one input required: --stdio, --sse, or --streamableHttp".into(),
            );
        }
        if input_count > 1 {
            return Err(
                "--stdio, --sse, and --streamableHttp are mutually exclusive".into(),
            );
        }

        let (input_mode, input_value) = if let Some(cmd) = raw.stdio {
            (InputMode::Stdio, cmd)
        } else if let Some(url) = raw.sse {
            (InputMode::Sse, url)
        } else {
            (InputMode::StreamableHttp, raw.streamable_http.unwrap())
        };

        // Default output transport.
        let output_transport = raw.output_transport.unwrap_or(match input_mode {
            InputMode::Stdio => OutputTransport::Sse,
            InputMode::Sse | InputMode::StreamableHttp => OutputTransport::Stdio,
        });

        // Validate transport combination.
        let valid = match input_mode {
            InputMode::Stdio => matches!(
                output_transport,
                OutputTransport::Sse | OutputTransport::Ws | OutputTransport::StreamableHttp
            ),
            InputMode::Sse | InputMode::StreamableHttp => {
                output_transport == OutputTransport::Stdio
            }
        };
        if !valid {
            return Err(format!(
                "invalid transport combination: {input_mode:?} input with {output_transport} output"
            ));
        }

        // Validate path flags start with '/'.
        validate_path("--ssePath", &raw.sse_path)?;
        validate_path("--messagePath", &raw.message_path)?;
        validate_path("--streamableHttpPath", &raw.streamable_http_path)?;

        // Validate --sessionTimeout.
        let session_timeout = match raw.session_timeout {
            Some(ms) if ms <= 0 => {
                return Err(format!("--sessionTimeout must be positive, got {ms}"));
            }
            Some(ms) => Some(ms as u64),
            None => None,
        };

        // Parse headers.
        let headers = raw
            .header
            .iter()
            .map(|h| parse_header(h))
            .collect::<Result<Vec<_>, _>>()?;

        // Apply --oauth2Bearer as an additional header.
        let mut all_headers = headers;
        if let Some(ref token) = raw.oauth2_bearer {
            if !token.is_empty() {
                all_headers.push(Header {
                    name: "Authorization".into(),
                    value: format!("Bearer {token}"),
                });
            }
        }

        // Parse CORS.
        let cors = parse_cors(raw.cors);

        // Parse health endpoints and validate paths.
        let health_endpoints = match raw.health_endpoint {
            Some(eps) => {
                for ep in &eps {
                    validate_path("--healthEndpoint value", ep)?;
                }
                eps
            }
            None => Vec::new(),
        };

        // Parse OAuth2 server config.
        let oauth2_server = match raw.oauth2_server_client_id {
            Some(client_id) => {
                let client_secret = raw.oauth2_server_client_secret.ok_or(
                    "--oauth2-server-client-secret is required when --oauth2-server-client-id is set"
                        .to_string(),
                )?;
                Some(OAuth2ServerConfig {
                    client_id,
                    client_secret,
                    token_expiry_secs: raw.oauth2_token_expiry,
                    issuer: raw.oauth2_issuer,
                })
            }
            None => None,
        };

        Ok(Self {
            input_mode,
            input_value,
            output_transport,
            port: raw.port,
            base_url: raw.base_url,
            sse_path: raw.sse_path,
            message_path: raw.message_path,
            streamable_http_path: raw.streamable_http_path,
            log_level: raw.log_level,
            cors,
            health_endpoints,
            headers: all_headers,
            stateful: raw.stateful,
            session_timeout,
            protocol_version: raw.protocol_version,
            oauth2_server,
        })
    }
}

#[allow(dead_code)]
fn validate_path(flag: &str, path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("{flag} must start with '/', got '{path}'"));
    }
    Ok(())
}

#[allow(dead_code)]
fn parse_header(raw: &str) -> Result<Header, String> {
    let Some(colon_pos) = raw.find(':') else {
        return Err(format!(
            "invalid --header '{raw}': must contain ':' separator (format: \"Name: Value\")"
        ));
    };
    let name = raw[..colon_pos].trim().to_string();
    let value = raw[colon_pos + 1..].trim().to_string();
    if name.is_empty() {
        return Err(format!("invalid --header '{raw}': empty header name"));
    }
    Ok(Header { name, value })
}

#[allow(dead_code)]
fn parse_cors(raw: Option<Vec<String>>) -> CorsConfig {
    match raw {
        None => CorsConfig::Disabled,
        Some(origins) if origins.is_empty() => CorsConfig::Wildcard,
        Some(origins) => {
            // TS parity: --cors "*" means wildcard, same as --cors with no values.
            if origins.iter().any(|o| o == "*") {
                return CorsConfig::Wildcard;
            }
            let parsed = origins
                .into_iter()
                .map(|o| {
                    // Origins enclosed in /.../ are regex patterns.
                    if o.starts_with('/') && o.ends_with('/') && o.len() > 2 {
                        let pattern = &o[1..o.len() - 1];
                        match Regex::new(pattern) {
                            Ok(re) => CorsOrigin::Regex(re),
                            Err(_) => CorsOrigin::Literal(o),
                        }
                    } else {
                        CorsOrigin::Literal(o)
                    }
                })
                .collect();
            CorsConfig::Origins(parsed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_output_transport_parse() {
        assert_eq!("stdio".parse::<OutputTransport>().unwrap(), OutputTransport::Stdio);
        assert_eq!("sse".parse::<OutputTransport>().unwrap(), OutputTransport::Sse);
        assert_eq!("ws".parse::<OutputTransport>().unwrap(), OutputTransport::Ws);
        assert_eq!(
            "streamableHttp".parse::<OutputTransport>().unwrap(),
            OutputTransport::StreamableHttp
        );
        assert!("invalid".parse::<OutputTransport>().is_err());
    }

    #[test]
    fn test_log_level_parse() {
        assert_eq!("debug".parse::<LogLevel>().unwrap(), LogLevel::Debug);
        assert_eq!("info".parse::<LogLevel>().unwrap(), LogLevel::Info);
        assert_eq!("none".parse::<LogLevel>().unwrap(), LogLevel::None);
        assert!("warn".parse::<LogLevel>().is_err());
    }

    #[test]
    fn test_header_parsing() {
        let h = parse_header("X-Custom: foo:bar").unwrap();
        assert_eq!(h.name, "X-Custom");
        assert_eq!(h.value, "foo:bar");

        let h2 = parse_header("Content-Type: application/json").unwrap();
        assert_eq!(h2.name, "Content-Type");
        assert_eq!(h2.value, "application/json");

        assert!(parse_header("no-colon-here").is_err());
        assert!(parse_header(": empty-name").is_err());
    }

    #[test]
    fn test_cors_parsing() {
        // Absent
        assert!(matches!(parse_cors(None), CorsConfig::Disabled));

        // Present no values
        assert!(matches!(
            parse_cors(Some(vec![])),
            CorsConfig::Wildcard
        ));

        // Literal origins
        let config = parse_cors(Some(vec!["http://localhost:3000".into()]));
        assert!(matches!(config, CorsConfig::Origins(ref o) if o.len() == 1));

        // Regex origin
        let config = parse_cors(Some(vec!["/example\\.com$/".into()]));
        if let CorsConfig::Origins(ref origins) = config {
            assert!(matches!(origins[0], CorsOrigin::Regex(_)));
        } else {
            panic!("expected Origins");
        }

        // Invalid regex falls back to literal
        let config = parse_cors(Some(vec!["/[invalid/".into()]));
        if let CorsConfig::Origins(ref origins) = config {
            assert!(matches!(origins[0], CorsOrigin::Literal(_)));
        } else {
            panic!("expected Origins");
        }
    }

    #[test]
    fn test_path_validation() {
        assert!(validate_path("--ssePath", "/sse").is_ok());
        assert!(validate_path("--ssePath", "sse").is_err());
    }

    // Test validate() directly for transport combinations.

    #[allow(dead_code)]
    fn make_raw(stdio: Option<&str>, sse: Option<&str>, sh: Option<&str>) -> RawArgs {
        RawArgs {
            stdio: stdio.map(String::from),
            sse: sse.map(String::from),
            streamable_http: sh.map(String::from),
            output_transport: None,
            port: 8000,
            base_url: String::new(),
            sse_path: "/sse".into(),
            message_path: "/message".into(),
            streamable_http_path: "/mcp".into(),
            log_level: LogLevel::Info,
            cors: None,
            health_endpoint: None,
            header: vec![],
            oauth2_bearer: None,
            oauth2_server_client_id: None,
            oauth2_server_client_secret: None,
            oauth2_token_expiry: 3600,
            oauth2_issuer: None,
            stateful: false,
            session_timeout: None,
            protocol_version: "2024-11-05".into(),
        }
    }

    #[test]
    fn test_no_input_error() {
        let raw = make_raw(None, None, None);
        assert!(Config::validate(raw).is_err());
    }

    #[test]
    fn test_multiple_inputs_error() {
        let raw = make_raw(Some("echo"), Some("http://x"), None);
        assert!(Config::validate(raw).is_err());
    }

    #[test]
    fn test_stdio_default_output_sse() {
        let raw = make_raw(Some("echo hi"), None, None);
        let config = Config::validate(raw).unwrap();
        assert_eq!(config.input_mode, InputMode::Stdio);
        assert_eq!(config.output_transport, OutputTransport::Sse);
    }

    #[test]
    fn test_sse_default_output_stdio() {
        let raw = make_raw(None, Some("http://localhost:8080/sse"), None);
        let config = Config::validate(raw).unwrap();
        assert_eq!(config.input_mode, InputMode::Sse);
        assert_eq!(config.output_transport, OutputTransport::Stdio);
    }

    #[test]
    fn test_streamable_http_default_output_stdio() {
        let raw = make_raw(None, None, Some("http://localhost:8080/mcp"));
        let config = Config::validate(raw).unwrap();
        assert_eq!(config.input_mode, InputMode::StreamableHttp);
        assert_eq!(config.output_transport, OutputTransport::Stdio);
    }

    #[test]
    fn test_invalid_combination_stdio_to_stdio() {
        let mut raw = make_raw(Some("echo hi"), None, None);
        raw.output_transport = Some(OutputTransport::Stdio);
        assert!(Config::validate(raw).is_err());
    }

    #[test]
    fn test_invalid_combination_sse_to_sse() {
        let mut raw = make_raw(None, Some("http://x"), None);
        raw.output_transport = Some(OutputTransport::Sse);
        assert!(Config::validate(raw).is_err());
    }

    #[test]
    fn test_valid_stdio_to_ws() {
        let mut raw = make_raw(Some("echo hi"), None, None);
        raw.output_transport = Some(OutputTransport::Ws);
        let config = Config::validate(raw).unwrap();
        assert_eq!(config.output_transport, OutputTransport::Ws);
    }

    #[test]
    fn test_valid_stdio_to_streamable_http() {
        let mut raw = make_raw(Some("echo hi"), None, None);
        raw.output_transport = Some(OutputTransport::StreamableHttp);
        let config = Config::validate(raw).unwrap();
        assert_eq!(config.output_transport, OutputTransport::StreamableHttp);
    }

    #[test]
    fn test_session_timeout_positive() {
        let mut raw = make_raw(Some("echo hi"), None, None);
        raw.session_timeout = Some(5000);
        let config = Config::validate(raw).unwrap();
        assert_eq!(config.session_timeout, Some(5000));
    }

    #[test]
    fn test_session_timeout_zero_error() {
        let mut raw = make_raw(Some("echo hi"), None, None);
        raw.session_timeout = Some(0);
        assert!(Config::validate(raw).is_err());
    }

    #[test]
    fn test_session_timeout_negative_error() {
        let mut raw = make_raw(Some("echo hi"), None, None);
        raw.session_timeout = Some(-100);
        assert!(Config::validate(raw).is_err());
    }

    #[test]
    fn test_oauth2_bearer_adds_header() {
        let mut raw = make_raw(Some("echo hi"), None, None);
        raw.oauth2_bearer = Some("my-token".into());
        let config = Config::validate(raw).unwrap();
        assert_eq!(config.headers.len(), 1);
        assert_eq!(config.headers[0].name, "Authorization");
        assert_eq!(config.headers[0].value, "Bearer my-token");
    }

    #[test]
    fn test_oauth2_bearer_empty_no_header() {
        let mut raw = make_raw(Some("echo hi"), None, None);
        raw.oauth2_bearer = Some(String::new());
        let config = Config::validate(raw).unwrap();
        assert!(config.headers.is_empty());
    }

    #[test]
    fn test_health_endpoints_validated() {
        let mut raw = make_raw(Some("echo hi"), None, None);
        raw.health_endpoint = Some(vec!["/healthz".into(), "/ready".into()]);
        let config = Config::validate(raw).unwrap();
        assert_eq!(config.health_endpoints, vec!["/healthz", "/ready"]);
    }

    #[test]
    fn test_health_endpoint_bad_path() {
        let mut raw = make_raw(Some("echo hi"), None, None);
        raw.health_endpoint = Some(vec!["healthz".into()]);
        assert!(Config::validate(raw).is_err());
    }

    #[test]
    fn test_bad_sse_path() {
        let mut raw = make_raw(Some("echo hi"), None, None);
        raw.sse_path = "no-slash".into();
        assert!(Config::validate(raw).is_err());
    }
}
