
//! stdio → Streamable HTTP (stateless) gateway.
//!
//! Per-request child process with auto-initialization. Each POST spawns a new
//! [`ChildBridge`], optionally performs MCP initialization (for non-`initialize`
//! requests), forwards the client's message(s), collects responses, then kills
//! the child.
//!
//! # Endpoints
//!
//! - `POST /mcp`: JSON-RPC request/notification → spawn child, auto-init, respond, kill
//! - `GET /mcp`: 405 Method Not Allowed (no sessions in stateless mode)
//! - `DELETE /mcp`: 405 Method Not Allowed (no sessions)
//! - Health endpoints as configured by `--healthEndpoint`
//!
//! # Auto-Init
//!
//! For non-`initialize` requests, the gateway transparently initializes the child:
//!
//! 1. Spawn child process
//! 2. Send synthetic `initialize` request with generated ID (`init_<ts>_<rand>`)
//! 3. Wait for initialize response (buffering notifications, D-005)
//! 4. Send `notifications/initialized` notification
//! 5. Forward original client messages
//! 6. Collect and return responses as SSE event stream
//! 7. Kill child

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};


use asupersync::channel::mpsc as async_mpsc;
use asupersync::http::h1::{Http1Listener, Request as H1Request, Response as H1Response};
use asupersync::runtime::RuntimeHandle;
use asupersync::time::{timeout, wall_now};
use asupersync::Cx;

use crate::child::ChildBridge;
use crate::cli::{CorsConfig, Header};
use crate::codec::CodecError;
use crate::cors::{self, CorsHandler, CorsResult};
use crate::error::{self, GatewayError};
use crate::health;
use crate::jsonrpc::{Parsed, RawMessage};
use crate::observe::{self, Logger, Metrics, Transport};

use super::stateful_http::GatewayRequest;
use super::sse_to_stdio::{build_fallback_init, build_initialized_notification, generate_init_id, raw_id_matches_str};

// ─── Constants ────────────────────────────────────────────────────────

/// Maximum request body size (16MB, D-101).
#[allow(dead_code)]
const MAX_BODY_SIZE: usize = 16 * 1024 * 1024;

/// Maximum concurrent requests (D-102). Each spawns a child process.
#[allow(dead_code)]
const MAX_CONCURRENT_REQUESTS: usize = 1024;

/// Timeout for auto-init sequence (PRD line 225).
#[allow(dead_code)]
const AUTO_INIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for waiting on child response after forwarding client messages.
#[allow(dead_code)]
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);


// ─── Response helpers ────────────────────────────────────────────────

/// HTTP response from handler.
///
/// Mirrors [`super::stateful_http::GatewayResponse`] but defined locally to avoid
/// coupling to stateful_http's private builder methods.
#[allow(dead_code)]
pub struct GatewayResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[allow(dead_code)]
fn response_plain_text(status: u16, body: &str) -> GatewayResponse {
    GatewayResponse {
        status,
        headers: vec![("content-type".into(), "text/plain".into())],
        body: body.as_bytes().to_vec(),
    }
}

#[allow(dead_code)]
fn response_sse(status: u16, events: &str) -> GatewayResponse {
    GatewayResponse {
        status,
        headers: vec![
            ("content-type".into(), "text/event-stream".into()),
            ("cache-control".into(), "no-cache".into()),
        ],
        body: events.as_bytes().to_vec(),
    }
}

#[allow(dead_code)]
fn response_accepted() -> GatewayResponse {
    GatewayResponse {
        status: 202,
        headers: Vec::new(),
        body: Vec::new(),
    }
}

#[allow(dead_code)]
fn response_no_content() -> GatewayResponse {
    GatewayResponse {
        status: 204,
        headers: Vec::new(),
        body: Vec::new(),
    }
}

#[allow(dead_code)]
fn response_from_error(err: &GatewayError) -> GatewayResponse {
    GatewayResponse {
        status: err.status_code(),
        headers: vec![("content-type".into(), err.content_type().into())],
        body: err.body().into_bytes(),
    }
}

/// 405 Method Not Allowed — no Content-Type header (matches TS `writeHead(405).end()`).
///
/// Body: JSON-RPC error `{"jsonrpc":"2.0","error":{"code":-32000,"message":"Method not allowed."},"id":null}`.
#[allow(dead_code)]
fn response_method_not_allowed() -> GatewayResponse {
    let err_msg =
        RawMessage::error_response(None, error::codes::SERVER_ERROR, "Method not allowed.");
    let body = serde_json::to_string(&err_msg).expect("JSON-RPC error serialization");
    GatewayResponse {
        status: 405,
        headers: Vec::new(), // No Content-Type per TS behavior
        body: body.into_bytes(),
    }
}

/// 500 Internal Server Error — outer catch-all for panics.
#[allow(dead_code)]
fn response_internal_error() -> GatewayResponse {
    let err_msg =
        RawMessage::error_response(None, error::codes::INTERNAL_ERROR, "Internal server error");
    let body = serde_json::to_string(&err_msg).expect("JSON-RPC error serialization");
    GatewayResponse {
        status: 500,
        headers: vec![("content-type".into(), "application/json".into())],
        body: body.into_bytes(),
    }
}

// ─── SSE event formatting ────────────────────────────────────────────

/// Format a RawMessage as an SSE event.
#[allow(dead_code)]
fn format_sse_event(msg: &RawMessage) -> String {
    let json = serde_json::to_string(msg).expect("JSON-RPC message serialization");
    format!("event: message\ndata: {json}\n\n")
}

/// Format a JSON string as an SSE event (used for batch array events).
#[allow(dead_code)]
fn format_sse_event_raw(json: &str) -> String {
    format!("event: message\ndata: {json}\n\n")
}

// ─── Accept header helpers ───────────────────────────────────────────

/// Returns `true` when the client wants only `application/json` (no SSE).
///
/// The rule: if `accept` contains `application/json` but neither
/// `text/event-stream` nor `*/*`, the client is a plain-JSON consumer and we
/// must NOT SSE-wrap the response.  Presence of `text/event-stream` or `*/*`
/// — or an explicit `text/event-stream` — means the client supports SSE, so
/// we keep the existing SSE-wrapped path.
#[allow(dead_code)]
fn prefers_json_only(accept: &str) -> bool {
    accept.contains("application/json")
        && !accept.contains("text/event-stream")
        && !accept.contains("*/*")
}

/// HTTP 200 response with `Content-Type: application/json`.
#[allow(dead_code)]
fn response_json(body: &str) -> GatewayResponse {
    GatewayResponse {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: body.as_bytes().to_vec(),
    }
}

/// Wait for the initialize response from child, buffering notifications (D-005).
///
/// Returns `Ok(())` when the init response is received. Buffered notifications
/// are stored in `buffered` for later forwarding. Returns `Err` on timeout or
/// child death.
#[allow(dead_code)]
async fn wait_for_init_response(
    cx: &Cx,
    rx: &mut async_mpsc::Receiver<Result<Parsed, CodecError>>,
    init_id: &str,
    total_timeout: Duration,
    buffered: &mut Vec<RawMessage>,
) -> Result<(), GatewayError> {
    let deadline = Instant::now() + total_timeout;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(GatewayError::AutoInitTimeout);
        }

        let recv_result = timeout(wall_now(), remaining, rx.recv(cx)).await;
        let parsed = match recv_result {
            Ok(Ok(Ok(p))) => p,
            Ok(Ok(Err(codec_err))) => {
                return Err(GatewayError::Child(crate::child::ChildError::Codec(
                    codec_err,
                )));
            }
            Ok(Err(_)) => return Err(GatewayError::ChildDead),
            Err(_elapsed) => return Err(GatewayError::AutoInitTimeout),
        };

        let msgs = match parsed {
            Parsed::Single(m) => vec![m],
            Parsed::Batch(ms) => ms,
        };
        for msg in msgs {
            if msg.is_response() {
                if let Some(ref id) = msg.id {
                    if raw_id_matches_str(id, init_id) {
                        // Init response received — suppress (don't forward)
                        return Ok(());
                    }
                }
            }
            // Buffer everything else (notifications, unexpected responses)
            buffered.push(msg);
        }
    }
}

/// Collect responses from child stdout after forwarding client messages.
///
/// Returns all collected messages (responses + notifications) and whether
/// all expected responses were received.
#[allow(dead_code)]
async fn collect_responses(
    cx: &Cx,
    rx: &mut async_mpsc::Receiver<Result<Parsed, CodecError>>,
    expected: usize,
    total_timeout: Duration,
) -> (Vec<RawMessage>, bool) {
    let deadline = Instant::now() + total_timeout;
    let mut collected: Vec<RawMessage> = Vec::new();
    let mut response_count = 0;

    while response_count < expected {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        let recv_result = timeout(wall_now(), remaining, rx.recv(cx)).await;
        let parsed = match recv_result {
            Ok(Ok(Ok(p))) => p,
            // Codec error or channel closed: stop collecting; caller checks
            // child.is_dead() to distinguish death from a clean stop.
            _ => break,
        };

        let msgs = match parsed {
            Parsed::Single(m) => vec![m],
            Parsed::Batch(ms) => ms,
        };
        for msg in msgs {
            if msg.is_response() {
                response_count += 1;
            }
            collected.push(msg);
        }
    }

    let complete = response_count >= expected;
    (collected, complete)
}

// ─── Request permit guard ────────────────────────────────────────────

/// RAII guard that decrements the active request counter on drop.
#[allow(dead_code)]
struct RequestPermit<'a> {
    counter: &'a AtomicUsize,
}

#[allow(dead_code)]
impl Drop for RequestPermit<'_> {
    #[allow(dead_code)]
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

// ─── Gateway ──────────────────────────────────────────────────────────

/// Shared gateway state for stateless Streamable HTTP mode.
#[allow(dead_code)]
pub struct StatelessHttpGateway {
    cmd: String,
    cors: CorsHandler,
    custom_headers: Vec<Header>,
    mcp_path: String,
    health_endpoints: Vec<String>,
    protocol_version: String,
    active_requests: AtomicUsize,
    metrics: Arc<Metrics>,
    logger: Arc<Logger>,
}

#[allow(dead_code)]
impl StatelessHttpGateway {
    /// Create a new stateless HTTP gateway.
    #[allow(dead_code)]
    pub fn new(
        cmd: String,
        mcp_path: String,
        health_endpoints: Vec<String>,
        cors_config: CorsConfig,
        custom_headers: Vec<Header>,
        protocol_version: String,
        metrics: Arc<Metrics>,
        logger: Arc<Logger>,
    ) -> Self {
        Self {
            cmd,
            cors: CorsHandler::new(cors_config, false), // no Mcp-Session-Id to expose
            custom_headers,
            mcp_path,
            health_endpoints,
            protocol_version,
            active_requests: AtomicUsize::new(0),
            metrics,
            logger,
        }
    }

    // ─── Request dispatch ──────────────────────────────────────────

    /// Handle an incoming HTTP request. Returns a response.
    #[allow(dead_code)]
    pub async fn handle_request(&self, cx: &Cx, req: &GatewayRequest) -> GatewayResponse {
        // CORS handling
        let cors_result = self.cors.process(
            &req.method,
            req.header("origin"),
            req.header("access-control-request-headers"),
        );
        match &cors_result {
            CorsResult::Preflight(headers) => {
                let mut resp = response_no_content();
                resp.headers.extend(headers.iter().cloned());
                return resp;
            }
            CorsResult::Disabled => {}
            CorsResult::ResponseHeaders(_) => {} // applied after handler
        }

        // Health endpoints
        for health_path in &self.health_endpoints {
            if req.path == *health_path {
                let detail = req
                    .query
                    .get("detail")
                    .map(|v| v == "true")
                    .unwrap_or(false);
                let health_resp =
                    health::check_health(&self.metrics, detail, self.logger.level());
                let resp = GatewayResponse {
                    status: health_resp.status_code(),
                    headers: vec![(
                        "content-type".into(),
                        health_resp.content_type().into(),
                    )],
                    body: health_resp.body().as_bytes().to_vec(),
                };
                return self.apply_cors(resp, &cors_result);
            }
        }

        // MCP endpoint dispatch
        if req.path == self.mcp_path {
            Metrics::inc(&self.metrics.total_requests);
            let resp = match req.method.as_str() {
                "POST" => self.handle_post(cx, req).await,
                // All non-POST methods return 405 in stateless mode
                _ => response_method_not_allowed(),
            };
            return self.apply_cors(resp, &cors_result);
        }

        // Not found
        self.apply_cors(response_plain_text(404, "Not Found"), &cors_result)
    }

    #[allow(dead_code)]
    fn apply_cors(&self, mut resp: GatewayResponse, cors_result: &CorsResult) -> GatewayResponse {
        if let CorsResult::ResponseHeaders(headers) = cors_result {
            resp.headers.extend(headers.iter().cloned());
        }
        cors::apply_custom_headers(&mut resp.headers, &self.custom_headers);
        resp
    }

    // ─── POST handler ──────────────────────────────────────────────

    /// Top-level POST dispatch. Panics in the async body propagate to the
    /// asupersync task supervisor, which isolates them from the listener loop;
    /// the client sees a connection abort rather than a JSON 500. This is a
    /// documented divergence from the previous sync handler.
    #[allow(dead_code)]
    async fn handle_post(&self, cx: &Cx, req: &GatewayRequest) -> GatewayResponse {
        let t0 = std::time::Instant::now();
        observe::record_request(&self.metrics, Transport::StatelessHttp);
        let resp = match self.handle_post_inner(cx, req).await {
            Ok(resp) => resp,
            Err(e) => {
                self.logger
                    .error(&format!("stateless POST error: {e}"));
                response_from_error(&e)
            }
        };
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        if resp.status >= 400 {
            observe::record_error(&self.metrics, Transport::StatelessHttp);
        }
        observe::record_latency(&self.metrics, Transport::StatelessHttp, elapsed_ms);
        resp
    }

    #[allow(dead_code)]
    async fn handle_post_inner(
        &self,
        cx: &Cx,
        req: &GatewayRequest,
    ) -> Result<GatewayResponse, GatewayError> {
        // 1. Acquire request permit (1024 max concurrent)
        let _permit = self.acquire_request_permit()?;

        // 2. Validate Content-Type
        let content_type = req.header("content-type").unwrap_or("");
        if !content_type.contains("application/json") {
            return Err(GatewayError::wrong_content_type(
                "application/json",
                content_type,
            ));
        }

        // 3. Validate Accept header
        let accept = req.header("accept").unwrap_or("");
        if !accept.contains("application/json")
            && !accept.contains("text/event-stream")
            && !accept.contains("*/*")
            && accept != "*"
        {
            return Err(GatewayError::missing_accept());
        }

        // 4. Body size check (16MB, D-101)
        if req.body.len() > MAX_BODY_SIZE {
            return Err(GatewayError::payload_too_large());
        }

        // 5. Parse JSON body
        let body_str = std::str::from_utf8(&req.body)
            .map_err(|_| GatewayError::bad_request("invalid UTF-8"))?;
        let parsed = crate::jsonrpc::parse_line(body_str)
            .map_err(|e| GatewayError::bad_request(&format!("malformed JSON: {e}")))?;

        let messages: Vec<RawMessage> = match parsed {
            Parsed::Single(msg) => vec![msg],
            Parsed::Batch(msgs) => msgs,
        };

        // 6. Short-circuit: if all messages are notifications, return 202 immediately.
        //    Pure notifications require no response and no child interaction — spawning
        //    a child and running the full auto-init exchange just to discard it wastes
        //    ~130ms per call (one full process spawn + two child roundtrips).
        let all_notifications = messages.iter().all(|m| m.is_notification());
        if all_notifications {
            return Ok(response_accepted());
        }

        // 7. Spawn child process
        let child =
            ChildBridge::spawn(&self.cmd, self.metrics.clone(), self.logger.clone()).map_err(
                |e| {
                    Metrics::inc(&self.metrics.spawn_failures);
                    GatewayError::Internal(format!("failed to spawn child: {e}"))
                },
            )?;

        // Take exclusive ownership of the child's stdout receiver for async
        // await-driven reads. This per-request child is the sole consumer, so
        // `None` here would indicate a programming error.
        let mut rx = child
            .take_stdout_rx()
            .ok_or_else(|| GatewayError::Internal("stdout receiver missing".into()))?;

        // 8. Determine path: direct initialize (Case 1) or auto-init (Case 2)
        let has_initialize = messages.iter().any(|m| m.is_initialize_request());
        let result = if has_initialize {
            self.forward_direct(cx, &child, &mut rx, messages, accept).await
        } else {
            self.auto_init_and_forward(cx, &child, &mut rx, messages, accept).await
        };

        // 9. Kill child (always, regardless of success/failure)
        child.kill();

        result
    }

    /// Acquire a request permit. Returns `ServiceUnavailable` if at capacity.
    #[allow(dead_code)]
    fn acquire_request_permit(&self) -> Result<RequestPermit<'_>, GatewayError> {
        let prev = self.active_requests.fetch_add(1, Ordering::AcqRel);
        if prev >= MAX_CONCURRENT_REQUESTS {
            self.active_requests.fetch_sub(1, Ordering::AcqRel);
            return Err(GatewayError::ServiceUnavailable(
                "max concurrent requests".into(),
            ));
        }
        Ok(RequestPermit {
            counter: &self.active_requests,
        })
    }

    // ─── Case 1: Direct forward (initialize request) ──────────────

    #[allow(dead_code)]
    async fn forward_direct(
        &self,
        cx: &Cx,
        child: &ChildBridge,
        rx: &mut async_mpsc::Receiver<Result<Parsed, CodecError>>,
        messages: Vec<RawMessage>,
        accept: &str,
    ) -> Result<GatewayResponse, GatewayError> {
        let expected_responses = messages.iter().filter(|m| m.is_request()).count();

        // Write all messages to child stdin
        for msg in &messages {
            child.write_message(msg)?;
        }

        // If all notifications (no requests), return 202
        if expected_responses == 0 {
            return Ok(response_accepted());
        }

        // Collect responses
        let (collected, _complete) =
            collect_responses(cx, rx, expected_responses, RESPONSE_TIMEOUT).await;

        if collected.is_empty() {
            if child.is_dead() {
                return Err(GatewayError::ChildDead);
            }
            return Err(GatewayError::Internal("response timeout".into()));
        }

        // Dispatch: plain JSON when client only wants application/json; SSE otherwise.
        if prefers_json_only(accept) {
            if collected.len() == 1 {
                // Single response: bare JSON object
                let json = serde_json::to_string(&collected[0])
                    .expect("JSON-RPC message serialization");
                Ok(response_json(&json))
            } else {
                // Batch response: JSON array
                let json =
                    serde_json::to_string(&collected).expect("JSON-RPC batch serialization");
                Ok(response_json(&json))
            }
        } else {
            // SSE path: single SSE event for batches, one event for single.
            if collected.len() == 1 {
                let sse_body = format_sse_event(&collected[0]);
                Ok(response_sse(200, &sse_body))
            } else {
                // Batch: wrap entire array in a single SSE event.
                let json =
                    serde_json::to_string(&collected).expect("JSON-RPC batch serialization");
                let sse_body = format_sse_event_raw(&json);
                Ok(response_sse(200, &sse_body))
            }
        }
    }

    // ─── Case 2: Auto-init + forward ──────────────────────────────

    #[allow(dead_code)]
    async fn auto_init_and_forward(
        &self,
        cx: &Cx,
        child: &ChildBridge,
        rx: &mut async_mpsc::Receiver<Result<Parsed, CodecError>>,
        messages: Vec<RawMessage>,
        accept: &str,
    ) -> Result<GatewayResponse, GatewayError> {
        // 1. Generate and send synthetic initialize request
        let init_id = generate_init_id();
        let init_msg = build_fallback_init(&init_id, &self.protocol_version);
        child.write_message(&init_msg)?;

        // 2. Wait for initialize response (buffer notifications, D-005)
        let mut buffered_notifications: Vec<RawMessage> = Vec::new();
        wait_for_init_response(
            cx,
            rx,
            &init_id,
            AUTO_INIT_TIMEOUT,
            &mut buffered_notifications,
        )
        .await?;

        // 3. Send notifications/initialized
        let initialized_notif = build_initialized_notification();
        child.write_message(&initialized_notif)?;

        // 4. Count expected responses from original messages
        let expected_responses = messages.iter().filter(|m| m.is_request()).count();

        // 5. Forward original messages to child
        for msg in &messages {
            child.write_message(msg)?;
        }

        // 6. If all notifications (no requests), return 202
        if expected_responses == 0 {
            return Ok(response_accepted());
        }

        // 7. Collect responses
        let (collected, _complete) =
            collect_responses(cx, rx, expected_responses, RESPONSE_TIMEOUT).await;

        if collected.is_empty() && expected_responses > 0 {
            if child.is_dead() {
                return Err(GatewayError::ChildDead);
            }
            return Err(GatewayError::Internal("response timeout".into()));
        }

        // 8. Build response — buffered notifications then collected messages.
        //    Dispatch: plain JSON when client only wants application/json; SSE otherwise.
        //    Buffered notifications (pre-init) are always prepended on the SSE path;
        //    on the JSON path they are silently dropped (no-id notifications have no
        //    JSON-RPC response counterpart that a plain-JSON client would expect).
        if prefers_json_only(accept) {
            if collected.len() == 1 {
                // Single response: bare JSON object
                let json = serde_json::to_string(&collected[0])
                    .expect("JSON-RPC message serialization");
                Ok(response_json(&json))
            } else {
                // Batch response: JSON array
                let json =
                    serde_json::to_string(&collected).expect("JSON-RPC batch serialization");
                Ok(response_json(&json))
            }
        } else {
            // SSE path: emit buffered notifications individually, then responses.
            // For a batch of responses (len > 1), wrap them in a single SSE event.
            let mut sse_body = String::new();
            for msg in &buffered_notifications {
                sse_body.push_str(&format_sse_event(msg));
            }
            if collected.len() == 1 {
                sse_body.push_str(&format_sse_event(&collected[0]));
            } else {
                // Batch: wrap entire array in a single SSE event.
                let json =
                    serde_json::to_string(&collected).expect("JSON-RPC batch serialization");
                sse_body.push_str(&format_sse_event_raw(&json));
            }
            Ok(response_sse(200, &sse_body))
        }
    }
}

// ─── Entry point ────────────────────────────────────────────────────────

/// Convert an asupersync HTTP/1.1 request to a gateway request.
fn convert_h1_request(req: H1Request) -> GatewayRequest {
    let uri = req.uri.clone();
    let (path, query_str) = if let Some(pos) = uri.find('?') {
        (&uri[..pos], &uri[pos + 1..])
    } else {
        (uri.as_str(), "")
    };
    let headers: HashMap<String, String> = req.headers.into_iter().collect();
    let query: HashMap<String, String> = query_str
        .split('&')
        .filter_map(|pair| {
            if pair.is_empty() {
                return None;
            }
            let mut kv = pair.splitn(2, '=');
            let k = kv.next()?.to_string();
            let v = kv.next().unwrap_or("").to_string();
            if k.is_empty() { None } else { Some((k, v)) }
        })
        .collect();
    GatewayRequest {
        method: req.method.as_str().to_string(),
        path: path.to_string(),
        headers,
        body: req.body,
        query,
    }
}

/// Convert a gateway response to an asupersync HTTP/1.1 response.
fn convert_gateway_response(resp: GatewayResponse) -> H1Response {
    let reason = super::sse::http_reason(resp.status);
    let mut r = H1Response::new(resp.status, reason, resp.body);
    for (name, value) in resp.headers {
        r = r.with_header(name, value);
    }
    r
}

/// Run the stdio → Streamable HTTP (stateless) gateway.
#[allow(dead_code)]
pub async fn run(_cx: &asupersync::Cx, config: crate::cli::Config, rt_handle: RuntimeHandle) -> anyhow::Result<()> {
    let logger = Arc::new(crate::observe::Logger::new(
        config.output_transport,
        config.log_level,
    ));
    let metrics = crate::observe::Metrics::new();

    logger.startup(
        env!("CARGO_PKG_VERSION"),
        &config.input_value,
        &config.output_transport.to_string(),
        config.port,
    );

    let _shutdown = crate::signal::install(&logger)?;

    let metrics_ready = metrics.clone();
    let gw = StatelessHttpGateway::new(
        config.input_value,
        config.streamable_http_path,
        config.health_endpoints,
        config.cors,
        config.headers,
        config.protocol_version,
        metrics,
        logger,
    );
    let gw = Arc::new(gw);

    // Bind async HTTP listener
    let gw_handler = gw.clone();
    let listener = Http1Listener::bind(
        format!("0.0.0.0:{}", config.port),
        move |req: H1Request| {
            let gw = gw_handler.clone();
            async move {
                let gw_req = convert_h1_request(req);
                let cx = Cx::current().expect("request handler runs in task context");
                let resp = gw.handle_request(&cx, &gw_req).await;
                convert_gateway_response(resp)
            }
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("failed to bind port {}: {e}", config.port))?;

    // Mark gateway as ready
    metrics_ready.set_ready();

    // Run accept loop until shutdown
    listener.run(&rt_handle).await?;
    Ok(())
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use serde_json::value::RawValue;
    use crate::cli::{LogLevel, OutputTransport};

    /// Shared runtime for async test bridging. Spawning tasks on this runtime
    /// gives them a real `Cx`, which `handle_request` needs to drive the
    /// async stdout receiver and timer wheel.
    fn test_runtime() -> &'static asupersync::runtime::Runtime {
        static RT: std::sync::OnceLock<asupersync::runtime::Runtime> = std::sync::OnceLock::new();
        RT.get_or_init(|| {
            asupersync::runtime::RuntimeBuilder::new()
                .build()
                .expect("test runtime")
        })
    }

    /// Spawn `gw.handle_request(cx, req)` on the test runtime and block the
    /// test thread until it completes. Bridges the runtime task back to the
    /// sync `#[test]` harness via std::sync::mpsc.
    fn run_handler(
        gw: Arc<StatelessHttpGateway>,
        req: GatewayRequest,
    ) -> GatewayResponse {
        let (tx, rx) = std::sync::mpsc::channel();
        test_runtime().handle().spawn(async move {
            let cx = Cx::current().expect("spawn task has context");
            let resp = gw.handle_request(&cx, &req).await;
            let _ = tx.send(resp);
        });
        rx.recv().expect("handler task completed")
    }

    #[allow(dead_code)]
    fn test_logger() -> Arc<Logger> {
        Arc::new(Logger::buffered(
            OutputTransport::StreamableHttp,
            LogLevel::Debug,
        ))
    }

    #[allow(dead_code)]
    fn test_metrics() -> Arc<Metrics> {
        Metrics::new()
    }

    #[allow(dead_code)]
    fn make_gateway_request(
        method: &str,
        path: &str,
        body: &str,
        headers: Vec<(&str, &str)>,
    ) -> GatewayRequest {
        let mut h = HashMap::new();
        for (k, v) in headers {
            h.insert(k.to_string(), v.to_string());
        }
        GatewayRequest {
            method: method.to_string(),
            path: path.to_string(),
            headers: h,
            body: body.as_bytes().to_vec(),
            query: HashMap::new(),
        }
    }

    #[allow(dead_code)]
    fn make_test_gateway() -> Arc<StatelessHttpGateway> {
        Arc::new(StatelessHttpGateway::new(
            "cat".into(),
            "/mcp".into(),
            vec!["/healthz".into()],
            CorsConfig::Disabled,
            vec![],
            "2024-11-05".into(),
            test_metrics(),
            test_logger(),
        ))
    }

    #[allow(dead_code)]
    fn make_test_gateway_with_cors() -> Arc<StatelessHttpGateway> {
        Arc::new(StatelessHttpGateway::new(
            "cat".into(),
            "/mcp".into(),
            vec!["/healthz".into()],
            CorsConfig::Wildcard,
            vec![],
            "2024-11-05".into(),
            test_metrics(),
            test_logger(),
        ))
    }

    /// Mock MCP server: responds to initialize with init result,
    /// to requests with a tool result. Notifications are consumed silently.
    #[allow(dead_code)]
    const MOCK_MCP_SERVER: &str = concat!(
        "python3 -c \"import sys, json\n",
        "while True:\n",
        "    line = sys.stdin.readline()\n",
        "    if not line: break\n",
        "    try: msg = json.loads(line)\n",
        "    except: continue\n",
        "    if 'id' not in msg or msg['id'] is None: continue\n",
        "    r = {'jsonrpc': '2.0', 'id': msg['id']}\n",
        "    if msg.get('method') == 'initialize':\n",
        "        r['result'] = {'protocolVersion': '2024-11-05', 'capabilities': {}}\n",
        "    else:\n",
        "        r['result'] = {'content': [{'type': 'text', 'text': 'ok'}]}\n",
        "    print(json.dumps(r), flush=True)\n",
        "\"",
    );

    #[allow(dead_code)]
    fn post_headers() -> Vec<(&'static str, &'static str)> {
        vec![
            ("content-type", "application/json"),
            ("accept", "application/json, text/event-stream"),
        ]
    }

    // ─── SSE event formatting ──────────────────────────────────────

    #[test]
    fn sse_event_format() {
        let result =
            serde_json::value::to_raw_value(&serde_json::json!({"ok": true})).unwrap();
        let msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(RawValue::from_string("1".into()).unwrap()),
            method: None,
            params: None,
            result: Some(result),
            error: None,
            ..Default::default()
        };
        let event = format_sse_event(&msg);
        assert!(event.starts_with("event: message\ndata: "));
        assert!(event.ends_with("\n\n"));
        assert!(event.contains("\"jsonrpc\":\"2.0\""));
    }

    // ─── 405 responses ──────────────────────────────────────────────

    #[test]
    fn get_returns_405() {
        let gw = make_test_gateway();
        let req = make_gateway_request("GET", "/mcp", "", vec![]);
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 405);
        // No Content-Type header per TS behavior
        let ct = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"));
        assert!(ct.is_none(), "405 should have no Content-Type");
        // Body is JSON-RPC error
        let body = String::from_utf8_lossy(&resp.body);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["error"]["code"], -32000);
        assert_eq!(parsed["error"]["message"], "Method not allowed.");
        assert!(parsed["id"].is_null());
    }

    #[test]
    fn delete_returns_405() {
        let gw = make_test_gateway();
        let req = make_gateway_request("DELETE", "/mcp", "", vec![]);
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 405);
    }

    #[test]
    fn put_returns_405() {
        let gw = make_test_gateway();
        let req = make_gateway_request("PUT", "/mcp", "", vec![]);
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 405);
    }

    // ─── POST validation ────────────────────────────────────────────

    #[test]
    fn post_wrong_content_type_returns_415() {
        let gw = make_test_gateway();
        let req = make_gateway_request(
            "POST",
            "/mcp",
            "{}",
            vec![
                ("content-type", "text/plain"),
                ("accept", "application/json, text/event-stream"),
            ],
        );
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 415);
    }

    #[test]
    fn post_missing_accept_returns_406() {
        let gw = make_test_gateway();
        let req = make_gateway_request(
            "POST",
            "/mcp",
            "{}",
            vec![("content-type", "application/json")],
        );
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 406);
    }

    #[test]
    fn post_body_too_large_returns_413() {
        let gw = make_test_gateway();
        let big_body = "x".repeat(MAX_BODY_SIZE + 1);
        let req = make_gateway_request("POST", "/mcp", &big_body, post_headers());
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 413);
    }

    #[test]
    fn post_malformed_json_returns_400() {
        let gw = make_test_gateway();
        let req = make_gateway_request("POST", "/mcp", "not json", post_headers());
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 400);
    }

    // ─── Auto-init helpers ──────────────────────────────────────────

    #[test]
    fn generate_init_id_format() {
        let id = generate_init_id();
        assert!(id.starts_with("init_"));
        let parts: Vec<&str> = id.split('_').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "init");
        // Timestamp (digits)
        assert!(parts[1].chars().all(|c| c.is_ascii_digit()));
        // 9 base-36 chars
        assert_eq!(parts[2].len(), 9);
        assert!(parts[2]
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn generate_init_id_unique() {
        let id1 = generate_init_id();
        let id2 = generate_init_id();
        assert_ne!(id1, id2);
    }

    // ─── raw_id_matches_str ─────────────────────────────────────────

    fn raw(s: &str) -> Box<RawValue> {
        RawValue::from_string(s.into()).unwrap()
    }

    /// The common path: string ID from `generate_init_id()` matches a JSON
    /// string response ID.
    #[test]
    fn raw_id_matches_str_string_id_matches() {
        let init_id = generate_init_id();
        // build_fallback_init wraps init_id as a JSON string.
        let json_id = raw(&format!("\"{init_id}\""));
        assert!(
            raw_id_matches_str(&json_id, &init_id),
            "string-typed ID must match its decoded value"
        );
    }

    /// Non-matching string ID must return false.
    #[test]
    fn raw_id_matches_str_string_id_no_match() {
        let json_id = raw(r#""init_123_aaa""#);
        assert!(!raw_id_matches_str(&json_id, "init_999_zzz"));
    }

    /// Numeric IDs fall back to raw-text comparison. This covers the case
    /// where a client sends a numeric initialize request ID (e.g. `"id":42`).
    #[test]
    fn raw_id_matches_str_numeric_id_matches() {
        // Numeric 42 in raw JSON is just the text "42".
        let json_id = raw("42");
        assert!(
            raw_id_matches_str(&json_id, "42"),
            "numeric ID must match its raw text representation"
        );
    }

    /// Numeric ID must not match a different number.
    #[test]
    fn raw_id_matches_str_numeric_id_no_match() {
        let json_id = raw("42");
        assert!(!raw_id_matches_str(&json_id, "43"));
    }

    /// Zero numeric ID (boundary case).
    #[test]
    fn raw_id_matches_str_zero_numeric_id() {
        let json_id = raw("0");
        assert!(raw_id_matches_str(&json_id, "0"));
        assert!(!raw_id_matches_str(&json_id, "1"));
    }

    #[test]
    fn synthetic_init_has_correct_shape() {
        let msg = build_fallback_init("init_123_abc", "2024-11-05");
        assert!(msg.is_initialize_request());
        assert_eq!(msg.id.as_ref().unwrap().get(), r#""init_123_abc""#);
        let params_str = msg.params.as_ref().unwrap().get();
        let params: serde_json::Value = serde_json::from_str(params_str).unwrap();
        assert_eq!(params["protocolVersion"], "2024-11-05");
        assert_eq!(params["capabilities"]["roots"]["listChanged"], true);
        assert!(params["capabilities"]["sampling"].is_object());
        assert_eq!(params["clientInfo"]["name"], "supergateway");
    }

    #[test]
    fn initialized_notification_shape() {
        let msg = build_initialized_notification();
        assert!(msg.is_notification());
        assert_eq!(msg.method_str(), Some("notifications/initialized"));
        assert!(msg.id.is_none());
        assert!(msg.params.is_none());
    }

    // ─── Request permit ─────────────────────────────────────────────

    #[test]
    fn request_permit_increments_and_decrements() {
        let counter = AtomicUsize::new(0);
        {
            counter.fetch_add(1, Ordering::AcqRel);
            let _permit = RequestPermit {
                counter: &counter,
            };
            assert_eq!(counter.load(Ordering::Acquire), 1);
        }
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    #[test]
    fn max_concurrent_requests_returns_503() {
        let gw = make_test_gateway();
        gw.active_requests
            .store(MAX_CONCURRENT_REQUESTS, Ordering::Release);
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"test"}"#;
        let req = make_gateway_request("POST", "/mcp", body, post_headers());
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 503);
    }

    // ─── Health endpoint ────────────────────────────────────────────

    #[test]
    fn health_endpoint_returns_503_before_ready() {
        let gw = make_test_gateway();
        let req = make_gateway_request("GET", "/healthz", "", vec![]);
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 503);
    }

    #[test]
    fn health_endpoint_returns_200_when_ready() {
        let gw = make_test_gateway();
        gw.metrics.set_ready();
        let req = make_gateway_request("GET", "/healthz", "", vec![]);
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 200);
        assert_eq!(String::from_utf8_lossy(&resp.body), "ok");
    }

    // ─── Custom headers ─────────────────────────────────────────────

    /// Helper: create a gateway with one custom header.
    fn make_gateway_with_custom_header(name: &str, value: &str) -> Arc<StatelessHttpGateway> {
        Arc::new(StatelessHttpGateway::new(
            "cat".into(),
            "/mcp".into(),
            vec!["/healthz".into()],
            CorsConfig::Disabled,
            vec![Header {
                name: name.into(),
                value: value.into(),
            }],
            "2024-11-05".into(),
            test_metrics(),
            test_logger(),
        ))
    }

    #[test]
    fn custom_header_appears_in_405_response() {
        // 405 is returned for GET /mcp — it goes through apply_cors so custom headers apply
        let gw = make_gateway_with_custom_header("X-Custom", "test");
        let req = make_gateway_request("GET", "/mcp", "", vec![]);
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 405);
        let found = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("X-Custom"));
        assert!(found.is_some(), "X-Custom header missing from 405 response; headers: {:?}", resp.headers);
        assert_eq!(found.unwrap().1, "test");
    }

    #[test]
    fn custom_header_appears_in_post_415_error() {
        // Wrong Content-Type → 415. POST errors go through apply_cors in handle_request.
        let gw = make_gateway_with_custom_header("X-Custom", "test");
        let req = make_gateway_request(
            "POST",
            "/mcp",
            "{}",
            vec![
                ("content-type", "text/plain"),
                ("accept", "application/json, text/event-stream"),
            ],
        );
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 415);
        let found = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("X-Custom"));
        assert!(found.is_some(), "X-Custom header missing from 415 POST response; headers: {:?}", resp.headers);
        assert_eq!(found.unwrap().1, "test");
    }

    #[test]
    fn custom_header_appears_in_post_406_error() {
        // Missing Accept → 406. POST errors go through apply_cors.
        let gw = make_gateway_with_custom_header("X-Custom", "test");
        let req = make_gateway_request(
            "POST",
            "/mcp",
            "{}",
            vec![("content-type", "application/json")],
        );
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 406);
        let found = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("X-Custom"));
        assert!(found.is_some(), "X-Custom header missing from 406 POST response; headers: {:?}", resp.headers);
        assert_eq!(found.unwrap().1, "test");
    }

    #[test]
    fn custom_header_appears_in_health_response() {
        // Health responses also go through apply_cors (custom headers applied there now).
        let gw = make_gateway_with_custom_header("X-Custom", "test");
        gw.metrics.set_ready();
        let req = make_gateway_request("GET", "/healthz", "", vec![]);
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 200);
        let found = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("X-Custom"));
        assert!(found.is_some(), "X-Custom header missing from health response; headers: {:?}", resp.headers);
        assert_eq!(found.unwrap().1, "test");
    }

    #[test]
    fn custom_header_appears_in_post_202_notification() {
        // Pure notification → 202 short-circuit. Goes through apply_cors.
        let gw = Arc::new(StatelessHttpGateway::new(
            MOCK_MCP_SERVER.into(),
            "/mcp".into(),
            vec![],
            CorsConfig::Disabled,
            vec![Header {
                name: "X-Custom".into(),
                value: "test".into(),
            }],
            "2024-11-05".into(),
            test_metrics(),
            test_logger(),
        ));
        let body = r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"token":"x"}}"#;
        let req = make_gateway_request("POST", "/mcp", body, post_headers());
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 202);
        let found = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("X-Custom"));
        assert!(found.is_some(), "X-Custom header missing from 202 response; headers: {:?}", resp.headers);
        assert_eq!(found.unwrap().1, "test");
    }

    #[test]
    fn custom_header_appears_in_post_200_response() {
        // POST with initialize → 200 SSE response. Goes through apply_cors.
        let gw = Arc::new(StatelessHttpGateway::new(
            MOCK_MCP_SERVER.into(),
            "/mcp".into(),
            vec![],
            CorsConfig::Disabled,
            vec![Header {
                name: "X-Custom".into(),
                value: "test".into(),
            }],
            "2024-11-05".into(),
            test_metrics(),
            test_logger(),
        ));
        let body =
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"capabilities":{}}}"#;
        let req = make_gateway_request("POST", "/mcp", body, post_headers());
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 200);
        let found = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("X-Custom"));
        assert!(found.is_some(), "X-Custom header missing from 200 POST response; headers: {:?}", resp.headers);
        assert_eq!(found.unwrap().1, "test");
    }

    #[test]
    fn custom_header_not_duplicated_in_health_response() {
        // Verify that custom header appears exactly once (not doubled) — since we removed
        // the manual apply_custom_headers call from health path and centralized in apply_cors.
        let gw = make_gateway_with_custom_header("X-Custom", "test");
        gw.metrics.set_ready();
        let req = make_gateway_request("GET", "/healthz", "", vec![]);
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 200);
        let count = resp
            .headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("X-Custom"))
            .count();
        assert_eq!(count, 1, "X-Custom must appear exactly once, found {count}; headers: {:?}", resp.headers);
    }

    // ─── CORS ───────────────────────────────────────────────────────

    #[test]
    fn options_preflight_returns_204() {
        let gw = make_test_gateway_with_cors();
        let req = make_gateway_request(
            "OPTIONS",
            "/mcp",
            "",
            vec![("origin", "https://example.com")],
        );
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 204);
        let acao = resp
            .headers
            .iter()
            .find(|(k, _)| k == "Access-Control-Allow-Origin");
        assert!(acao.is_some());
    }

    #[test]
    fn cors_no_session_header_exposed() {
        let gw = make_test_gateway_with_cors();
        let req = make_gateway_request(
            "GET",
            "/mcp",
            "",
            vec![("origin", "https://example.com")],
        );
        let resp = run_handler(gw.clone(), req);
        let expose = resp
            .headers
            .iter()
            .find(|(k, _)| k == "Access-Control-Expose-Headers");
        assert!(
            expose.is_none(),
            "stateless mode should not expose Mcp-Session-Id"
        );
    }

    // ─── Not found ──────────────────────────────────────────────────

    #[test]
    fn unknown_path_returns_404() {
        let gw = make_test_gateway();
        let req = make_gateway_request("GET", "/unknown", "", vec![]);
        let resp = run_handler(gw.clone(), req);
        assert_eq!(resp.status, 404);
    }

    // ─── No Mcp-Session-Id ──────────────────────────────────────────

    #[test]
    fn no_session_id_in_405() {
        let gw = make_test_gateway();
        let req = make_gateway_request("GET", "/mcp", "", vec![]);
        let resp = run_handler(gw.clone(), req);
        let sid = resp
            .headers
            .iter()
            .find(|(k, _)| k == "Mcp-Session-Id");
        assert!(sid.is_none());
    }

    // ─── Integration: initialize passthrough (Case 1) ───────────────

    #[test]
    fn post_initialize_passthrough() {
        let gw = Arc::new(StatelessHttpGateway::new(
            MOCK_MCP_SERVER.into(),
            "/mcp".into(),
            vec![],
            CorsConfig::Disabled,
            vec![],
            "2024-11-05".into(),
            test_metrics(),
            test_logger(),
        ));

        let body =
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"capabilities":{}}}"#;
        let req = make_gateway_request("POST", "/mcp", body, post_headers());
        let resp = run_handler(gw.clone(), req);

        assert_eq!(resp.status, 200);
        let ct = resp
            .headers
            .iter()
            .find(|(k, _)| k == "content-type");
        assert_eq!(ct.unwrap().1, "text/event-stream");
        // No Mcp-Session-Id
        let sid = resp
            .headers
            .iter()
            .find(|(k, _)| k == "Mcp-Session-Id");
        assert!(sid.is_none());
        // Body contains init response
        let body_str = String::from_utf8_lossy(&resp.body);
        assert!(body_str.contains("protocolVersion"));
    }

    // ─── Integration: auto-init tool call (Case 2) ──────────────────

    #[test]
    fn post_auto_init_tool_call() {
        let gw = Arc::new(StatelessHttpGateway::new(
            MOCK_MCP_SERVER.into(),
            "/mcp".into(),
            vec![],
            CorsConfig::Disabled,
            vec![],
            "2024-11-05".into(),
            test_metrics(),
            test_logger(),
        ));

        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hello"}}}"#;
        let req = make_gateway_request("POST", "/mcp", body, post_headers());
        let resp = run_handler(gw.clone(), req);

        assert_eq!(resp.status, 200);
        let body_str = String::from_utf8_lossy(&resp.body);
        // Should contain the tool response (not the init response)
        assert!(body_str.contains("\"ok\"") || body_str.contains("\"result\""));
    }

    // ─── Integration: notification returns 202 ──────────────────────

    #[test]
    fn post_notification_returns_202() {
        let gw = Arc::new(StatelessHttpGateway::new(
            MOCK_MCP_SERVER.into(),
            "/mcp".into(),
            vec![],
            CorsConfig::Disabled,
            vec![],
            "2024-11-05".into(),
            test_metrics(),
            test_logger(),
        ));

        let body = r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"token":"x"}}"#;
        let req = make_gateway_request("POST", "/mcp", body, post_headers());
        let resp = run_handler(gw.clone(), req);

        assert_eq!(resp.status, 202);
    }

    // ─── Integration: auto-init timeout ─────────────────────────────

    #[test]
    fn auto_init_timeout_returns_504() {
        // Child never responds → auto-init times out after 5s
        let gw = Arc::new(StatelessHttpGateway::new(
            "sleep 999".into(),
            "/mcp".into(),
            vec![],
            CorsConfig::Disabled,
            vec![],
            "2024-11-05".into(),
            test_metrics(),
            test_logger(),
        ));

        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let req = make_gateway_request("POST", "/mcp", body, post_headers());
        let resp = run_handler(gw.clone(), req);

        assert_eq!(resp.status, 504);
        let body_str = String::from_utf8_lossy(&resp.body);
        let parsed: serde_json::Value = serde_json::from_str(&body_str).unwrap();
        assert_eq!(parsed["error"]["code"], -32603);
    }

    // ─── Integration: batch request ─────────────────────────────────

    #[test]
    fn post_batch_request_sse() {
        // With Accept: text/event-stream, a batch returns a SINGLE SSE event
        // whose data is the JSON array of responses.
        let gw = Arc::new(StatelessHttpGateway::new(
            MOCK_MCP_SERVER.into(),
            "/mcp".into(),
            vec![],
            CorsConfig::Disabled,
            vec![],
            "2024-11-05".into(),
            test_metrics(),
            test_logger(),
        ));

        let body = r#"[{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}},{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{}}]"#;
        let req = make_gateway_request(
            "POST",
            "/mcp",
            body,
            vec![
                ("content-type", "application/json"),
                ("accept", "text/event-stream"),
            ],
        );
        let resp = run_handler(gw.clone(), req);

        assert_eq!(resp.status, 200);
        let ct = resp.headers.iter().find(|(k, _)| k == "content-type");
        assert_eq!(ct.unwrap().1, "text/event-stream");
        let body_str = String::from_utf8_lossy(&resp.body);
        // Batch: single SSE event wrapping the JSON array
        let event_count = body_str.matches("event: message").count();
        assert_eq!(event_count, 1, "expected 1 SSE event for batch, body: {body_str}");
        // The data line must be a valid JSON array
        let data_line = body_str
            .lines()
            .find(|l| l.starts_with("data: "))
            .expect("SSE data line");
        let arr: serde_json::Value =
            serde_json::from_str(data_line.trim_start_matches("data: ")).unwrap();
        assert!(arr.is_array(), "batch SSE data must be a JSON array");
        assert_eq!(arr.as_array().unwrap().len(), 2);
    }

    // ─── Accept-header dispatch ─────────────────────────────────────

    #[test]
    fn post_json_accept_returns_bare_json_single() {
        // `Accept: application/json` → bare JSON object, no SSE wrapping.
        let gw = Arc::new(StatelessHttpGateway::new(
            MOCK_MCP_SERVER.into(),
            "/mcp".into(),
            vec![],
            CorsConfig::Disabled,
            vec![],
            "2024-11-05".into(),
            test_metrics(),
            test_logger(),
        ));

        let body =
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"capabilities":{}}}"#;
        let req = make_gateway_request(
            "POST",
            "/mcp",
            body,
            vec![
                ("content-type", "application/json"),
                ("accept", "application/json"),
            ],
        );
        let resp = run_handler(gw.clone(), req);

        assert_eq!(resp.status, 200);
        let ct = resp.headers.iter().find(|(k, _)| k == "content-type");
        assert_eq!(
            ct.unwrap().1,
            "application/json",
            "Accept: application/json must yield Content-Type: application/json"
        );
        // Body must be a bare JSON object (not SSE)
        let body_str = String::from_utf8_lossy(&resp.body);
        assert!(
            !body_str.contains("event: message"),
            "bare JSON response must not contain SSE event markers"
        );
        let parsed: serde_json::Value = serde_json::from_str(&body_str).unwrap();
        assert!(parsed.is_object(), "single request must yield a JSON object");
    }

    #[test]
    fn post_json_accept_returns_json_array_for_batch() {
        // `Accept: application/json` + batch body → JSON array, no SSE wrapping.
        let gw = Arc::new(StatelessHttpGateway::new(
            MOCK_MCP_SERVER.into(),
            "/mcp".into(),
            vec![],
            CorsConfig::Disabled,
            vec![],
            "2024-11-05".into(),
            test_metrics(),
            test_logger(),
        ));

        let body = r#"[{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}},{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{}}]"#;
        let req = make_gateway_request(
            "POST",
            "/mcp",
            body,
            vec![
                ("content-type", "application/json"),
                ("accept", "application/json"),
            ],
        );
        let resp = run_handler(gw.clone(), req);

        assert_eq!(resp.status, 200);
        let ct = resp.headers.iter().find(|(k, _)| k == "content-type");
        assert_eq!(ct.unwrap().1, "application/json");
        let body_str = String::from_utf8_lossy(&resp.body);
        assert!(
            !body_str.contains("event: message"),
            "bare JSON batch must not contain SSE event markers"
        );
        let parsed: serde_json::Value = serde_json::from_str(&body_str).unwrap();
        assert!(parsed.is_array(), "batch must yield a JSON array");
        assert_eq!(parsed.as_array().unwrap().len(), 2);
    }

    // ─── prefers_json_only helper ───────────────────────────────────

    #[test]
    fn prefers_json_only_json_only() {
        assert!(prefers_json_only("application/json"));
    }

    #[test]
    fn prefers_json_only_json_with_quality() {
        assert!(prefers_json_only("application/json;q=0.9"));
    }

    #[test]
    fn prefers_json_only_rejects_sse() {
        assert!(!prefers_json_only("application/json, text/event-stream"));
    }

    #[test]
    fn prefers_json_only_rejects_wildcard() {
        assert!(!prefers_json_only("*/*"));
        assert!(!prefers_json_only("application/json, */*"));
    }

    #[test]
    fn prefers_json_only_rejects_sse_only() {
        assert!(!prefers_json_only("text/event-stream"));
    }

    #[test]
    fn prefers_json_only_rejects_empty() {
        assert!(!prefers_json_only(""));
    }

    // ─── Integration: child death during request ────────────────────

    #[test]
    fn child_death_returns_502() {
        let gw = Arc::new(StatelessHttpGateway::new(
            // Child exits immediately
            "true".into(),
            "/mcp".into(),
            vec![],
            CorsConfig::Disabled,
            vec![],
            "2024-11-05".into(),
            test_metrics(),
            test_logger(),
        ));

        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let req = make_gateway_request("POST", "/mcp", body, post_headers());
        let resp = run_handler(gw.clone(), req);

        // Child dies during auto-init → either 502 (ChildDead) or 502 (AutoInitTimeout)
        assert!(
            resp.status == 502 || resp.status == 500,
            "expected 502 or 500, got {}",
            resp.status
        );
    }
}
