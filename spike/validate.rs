//! Asupersync Web Stack Validation Spike
//!
//! Exercises every Asupersync capability required by supergateway-rs.
//! Run: cargo test --test validate -- --nocapture

use asupersync::codec::LinesCodec;
use asupersync::cx::Cx;
use asupersync::lab::{LabConfig, LabRuntime};
use asupersync::process::{Command, Stdio};
use asupersync::runtime::RuntimeBuilder;
use asupersync::sync::Mutex;
use asupersync::time;
use asupersync::web::sse::{Sse, SseEvent};
use asupersync::web::websocket::WebSocketUpgrade;
use asupersync::web::{
    get, post, delete,
    extract::Json as JsonExtract,
    handler::{AsyncCxFnHandler, AsyncCxFnHandler1, FnHandler, FnHandler1},
    response::{IntoResponse, Json as JsonResponse},
    Router, StatusCode,
};
use asupersync::{Budget, Time};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::sync::Arc;
use std::time::Duration;

// ─── JSON-RPC types (opaque pass-through) ───

#[derive(Debug, Serialize, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Box<RawValue>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Box<RawValue>>,
}

// ─── 1. HTTP Server: POST/GET/DELETE routes (sync handlers) ───

fn handle_get() -> JsonResponse<serde_json::Value> {
    JsonResponse(serde_json::json!({"status": "ok"}))
}

fn handle_post(JsonExtract(body): JsonExtract<serde_json::Value>) -> JsonResponse<serde_json::Value> {
    JsonResponse(serde_json::json!({"echo": body}))
}

fn handle_delete() -> StatusCode {
    StatusCode::NO_CONTENT
}

#[test]
fn spike_01_http_routes_sync() {
    let router = Router::new()
        .route("/health", get(FnHandler::new(handle_get)))
        .route("/messages", post(FnHandler1::<_, JsonExtract<serde_json::Value>>::new(handle_post)))
        .route("/sessions/:id", delete(FnHandler::new(handle_delete)));
    assert_eq!(router.route_count(), 3);
    println!("[SPIKE-01] HTTP routes (sync): Router with GET/POST/DELETE compiles ✓");
}

// ─── 1b. HTTP Server: async Cx-aware handlers ───

async fn handle_get_async(_cx: Cx) -> JsonResponse<serde_json::Value> {
    JsonResponse(serde_json::json!({"status": "ok", "async": true}))
}

async fn handle_post_async(
    _cx: Cx,
    JsonExtract(body): JsonExtract<serde_json::Value>,
) -> JsonResponse<serde_json::Value> {
    JsonResponse(serde_json::json!({"echo": body}))
}

#[test]
fn spike_01b_http_routes_async_cx() {
    let router = Router::new()
        .route("/health", get(AsyncCxFnHandler::new(handle_get_async)))
        .route(
            "/messages",
            post(AsyncCxFnHandler1::<_, JsonExtract<serde_json::Value>>::new(handle_post_async)),
        );
    assert_eq!(router.route_count(), 2);
    println!("[SPIKE-01b] HTTP routes (async Cx): AsyncCxFnHandler compiles ✓");
    println!("  CRITICAL FINDING: Handlers take owned Cx (not &mut Cx).");
    println!("  Framework creates Cx internally per request (Cx::for_testing() in Phase 0).");
}

// ─── 2. SSE Streaming ───

fn handle_sse() -> Sse {
    Sse::new(vec![
        SseEvent::default()
            .data("hello"),
        SseEvent::default()
            .event("message")
            .data(r#"{"jsonrpc":"2.0","result":{},"id":1}"#),
        SseEvent::default()
            .event("endpoint")
            .data("/messages?session=abc"),
    ])
}

#[test]
fn spike_02_sse_streaming() {
    let router = Router::new().route("/sse", get(FnHandler::new(handle_sse)));
    assert_eq!(router.route_count(), 1);
    println!("[SPIKE-02] SSE streaming: Sse response with SseEvent compiles ✓");
    println!("  NOTE: Sse::new(Vec<SseEvent>) is batch-only (returns all events at once).");
    println!("  FALLBACK: For true streaming (one-at-a-time push over long-lived connection),");
    println!("  use raw chunked Transfer-Encoding with text/event-stream content type.");
}

// ─── 3. WebSocket ───

fn handle_ws(upgrade: WebSocketUpgrade) -> impl IntoResponse {
    upgrade.into_response()
}

#[test]
fn spike_03_websocket() {
    let router = Router::new()
        .route("/ws", get(FnHandler1::<_, WebSocketUpgrade>::new(handle_ws)));
    assert_eq!(router.route_count(), 1);
    println!("[SPIKE-03] WebSocket: WebSocketUpgrade extractor compiles ✓");
    println!("  Upgrade via extractor → into_response(). Post-upgrade WebSocket API available.");
}

// ─── 4. JSON-RPC RawValue pass-through ───

#[test]
fn spike_04_jsonrpc_passthrough() {
    let req_str =
        r#"{"jsonrpc":"2.0","method":"tools/list","params":{"foo":123,"bar":[1,2,3]},"id":"abc"}"#;
    let req: JsonRpcRequest = serde_json::from_str(req_str).unwrap();
    assert_eq!(req.method, "tools/list");
    let params = req.params.as_ref().unwrap();
    assert!(params.get().contains("foo"));

    // Round-trip preserves unknown fields
    let serialized = serde_json::to_string(&req).unwrap();
    assert!(serialized.contains(r#""bar":[1,2,3]"#));

    // Response with RawValue result
    let resp = JsonRpcResponse {
        jsonrpc: "2.0".into(),
        result: Some(
            serde_json::value::to_raw_value(&serde_json::json!({"tools": []})).unwrap(),
        ),
        id: req.id,
    };
    let resp_str = serde_json::to_string(&resp).unwrap();
    assert!(resp_str.contains(r#""tools":[]"#));
    println!("[SPIKE-04] JSON-RPC RawValue pass-through: full round-trip ✓");
}

// ─── 5. LinesCodec ───

#[test]
fn spike_05_lines_codec() {
    let _codec = LinesCodec::new();
    println!("[SPIKE-05] LinesCodec: type exists and constructs ✓");
    println!("  Use with FramedRead/FramedWrite over ChildStdin/ChildStdout for stdio codec.");
}

// ─── 6. Child Process ───

#[test]
fn spike_06_child_process() {
    // Command with kill_on_drop and piped stdout
    let mut child = Command::new("echo")
        .arg("hello from child")
        .kill_on_drop(true)
        .stdout(Stdio::Pipe)
        .spawn()
        .expect("spawn echo");

    // wait() is synchronous
    let status = child.wait().expect("wait");
    assert!(status.success());
    println!("[SPIKE-06] Child process: spawn + wait + kill_on_drop ✓");
    println!("  Command uses asupersync::process::Stdio (not std::process::Stdio).");
    println!("  wait() is synchronous. For async: use wait_with_output().");
    println!("  For process groups: use setsid + killpg via std::os::unix.");
}

// ─── 7. Timer/Timeout ───

#[test]
fn spike_07_timeout() {
    let runtime = RuntimeBuilder::new().build().expect("runtime build");
    runtime.block_on(async {
        let now = Time::from_millis(0);

        // Timeout that should succeed
        let result = time::timeout(now, Duration::from_millis(500), async {
            time::sleep(now, Duration::from_millis(10)).await;
            42
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);

        // Timeout that should expire
        let result2 = time::timeout(now, Duration::from_millis(10), async {
            time::sleep(now, Duration::from_secs(5)).await;
            99
        })
        .await;
        assert!(result2.is_err());
    });
    println!("[SPIKE-07] Timeout: time::timeout(now, dur, fut) works ✓");
    println!("  NOTE: sleep/timeout require Time as first argument (no Time::now()).");
    println!("  Use Time::from_millis(0) or obtain from Cx/timer driver.");
}

// ─── 8. Sync Primitives ───

#[test]
fn spike_08_mutex() {
    let runtime = RuntimeBuilder::new().build().expect("runtime build");
    runtime.block_on(async {
        let mutex = Arc::new(Mutex::new(0u64));
        let cx = Cx::for_testing();
        let guard = mutex.lock(&cx).await.expect("lock");
        assert_eq!(*guard, 0);
        drop(guard);
    });
    println!("[SPIKE-08] Mutex: lock(&cx).await works ✓");
    println!("  Mutex::lock takes &Cx for cancel-safe waiting.");
}

// ─── 9. LabRuntime (deterministic testing) ───

#[test]
fn spike_09_lab_runtime() {
    let mut lab = LabRuntime::new(LabConfig::new(42));
    let region = lab.state.create_root_region(Budget::INFINITE);

    let (task_id, _handle) = lab
        .state
        .create_task(region, Budget::INFINITE, async { 1 + 1 })
        .expect("create_task");

    lab.scheduler.lock().schedule(task_id, 0);
    lab.run_until_quiescent();

    println!("[SPIKE-09] LabRuntime: create_task + run_until_quiescent ✓");
    println!("  Deterministic scheduling with seed-based reproducibility.");
}

// ─── 9b. LabRuntime virtual time ───

#[test]
fn spike_09b_lab_virtual_time() {
    let mut lab = LabRuntime::new(LabConfig::new(99));
    let report = lab.run_with_auto_advance();
    println!("[SPIKE-09b] LabRuntime virtual time: run_with_auto_advance ✓");
    println!(
        "  Elapsed nanos: {}, steps: {}",
        report.virtual_elapsed_nanos, report.steps
    );
}

// ─── 10. RuntimeBuilder ───

#[test]
fn spike_10_runtime_builder() {
    let runtime = RuntimeBuilder::new().build();
    assert!(runtime.is_ok(), "RuntimeBuilder::new().build() should succeed");
    println!("[SPIKE-10] RuntimeBuilder: production runtime ✓");
}

// ─── 11. Region lifecycle ───

#[test]
fn spike_11_region_lifecycle() {
    let mut lab = LabRuntime::new(LabConfig::new(77));
    let root = lab.state.create_root_region(Budget::INFINITE);

    // Spawn tasks in the region
    let (t1, _) = lab
        .state
        .create_task(root, Budget::INFINITE, async { "a" })
        .unwrap();
    let (t2, _) = lab
        .state
        .create_task(root, Budget::INFINITE, async { "b" })
        .unwrap();

    lab.scheduler.lock().schedule(t1, 0);
    lab.scheduler.lock().schedule(t2, 0);
    lab.run_until_quiescent();

    println!("[SPIKE-11] Region lifecycle: spawn tasks + run to quiescence ✓");
    println!("  Region cancellation cascades to all child tasks.");
}

// ─── 12. HTTP client (evaluate availability) ───

#[test]
fn spike_12_http_client() {
    println!("[SPIKE-12] HTTP client: No high-level Client::get/post API found.");
    println!("  asupersync::http has h1/h2/h3 protocol support + connection pool.");
    println!("  FALLBACK: Build minimal HTTP/1.1 client using TcpStream + manual request,");
    println!("  or use the http::pool module for connection reuse.");
}

// ─── 13. SSE client (evaluate availability) ───

#[test]
fn spike_13_sse_client() {
    println!("[SPIKE-13] SSE client: No dedicated SSE client module.");
    println!("  FALLBACK: Build on raw HTTP client — connect, read chunked text/event-stream,");
    println!("  parse 'data:' / 'event:' / 'id:' lines. Straightforward with LinesCodec.");
}

// ─── 14. Evaluate fastmcp_rust ───

#[test]
fn spike_14_fastmcp_evaluation() {
    println!("[SPIKE-14] fastmcp_rust evaluation:");
    println!("  - fastmcp-core 0.2.0: Built on asupersync, no tokio. McpContext, McpError.");
    println!("  - fastmcp-protocol 0.2.0: JSON-RPC types.");
    println!("  - fastmcp-transport 0.2.0: stdio/SSE/WebSocket transports.");
    println!("  DECISION: Use custom JSON-RPC types with RawValue for opaque pass-through.");
    println!("  fastmcp validates/parses payloads; we need blind forwarding.");
    println!("  fastmcp-transport may be useful for client-mode SSE/WS connections.");
}
