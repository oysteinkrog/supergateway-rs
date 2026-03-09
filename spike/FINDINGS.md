# Asupersync Web Stack Validation Spike — Findings

**Date:** 2026-03-09
**Crate:** asupersync 0.2.7
**Toolchain:** nightly-2026-01-22 (rustc 1.95.0-nightly)
**Verdict:** GO — all critical capabilities work or have viable fallbacks.

## Summary Table

| # | Item | Status | Notes | Fallback Approach |
|---|------|--------|-------|-------------------|
| 1 | HTTP server: POST/GET/DELETE routes | works | `Router::new().route(path, get(FnHandler::new(f)))` | — |
| 1b | Async Cx-aware handlers | works | `AsyncCxFnHandler::new(f)` wraps `Fn(Cx) -> Fut` | Sync handlers for simple routes |
| 2 | SSE streaming (server) | works-with-fallback | `Sse::new(Vec<SseEvent>)` is batch (all events at once) | Raw chunked `text/event-stream` via `Response::new()` for true streaming |
| 3 | WebSocket upgrade | works | `WebSocketUpgrade` extractor + `into_response()` | — |
| 4 | JSON-RPC RawValue pass-through | works | `serde_json` `raw_value` feature, full round-trip | — |
| 5 | Line-delimited stdio codec | works | `LinesCodec::new()` + `FramedRead`/`FramedWrite` | — |
| 6 | Child process (spawn/kill) | works | `Command::new().kill_on_drop(true)`, `Stdio::Pipe` | — |
| 7 | Timer/timeout | works | `time::timeout(now, dur, fut)` — requires `Time` arg | — |
| 8 | Sync primitives (Mutex) | works | `mutex.lock(&cx).await?` — requires `&Cx` arg | — |
| 9 | LabRuntime (deterministic test) | works | `create_root_region` → `create_task` → `run_until_quiescent` | — |
| 9b | Virtual time | works | `run_with_auto_advance()` | — |
| 10 | RuntimeBuilder (production) | works | `RuntimeBuilder::new().build()?.block_on(fut)` | — |
| 11 | Region lifecycle | works | Tasks in region, scheduled, run to quiescence | — |
| 12 | HTTP client | works-with-fallback | No high-level `Client::get/post` API | Build on `TcpStream` + `http::pool` + manual HTTP/1.1 |
| 13 | SSE client | works-with-fallback | No dedicated SSE client module | Parse `text/event-stream` lines over raw HTTP connection |
| 14 | fastmcp_rust evaluation | evaluated | Built on asupersync, no tokio | Use custom RawValue types; fastmcp-transport useful for client modes |
| 15 | Nightly Rust + Docker | works | Pinned in `rust-toolchain.toml` | — |

## Critical Architectural Findings

### 1. Cx ownership model (CRITICAL — answered)

**Question:** How do concurrent HTTP handlers each get their own `&mut Cx`?

**Answer:** They don't use `&mut Cx`. Handlers receive an **owned `Cx`** (by value):
- Sync handlers: `FnHandler<F>` where `F: Fn(extractors...) -> Res`
- Async handlers: `AsyncCxFnHandler<F>` where `F: Fn(Cx, extractors...) -> Fut`

The framework creates a fresh `Cx` per request internally (currently `Cx::for_testing()` in Phase 0).
This means no aliasing issues — each request gets its own Cx.

**Impact on supergateway-rs:** Handlers can safely hold mutable state per-request via Cx.
Shared state across requests should use `Arc<Mutex<T>>` with `mutex.lock(&cx).await`.

### 2. Handler wiring requires explicit wrapper types

Bare `fn` and `async fn` do NOT auto-implement `Handler`. Must wrap:
- `FnHandler::new(f)` for sync: `fn() -> impl IntoResponse`
- `FnHandler1::new(f)` for sync with 1 extractor
- `AsyncCxFnHandler::new(f)` for async: `async fn(Cx) -> impl IntoResponse`
- `AsyncCxFnHandler1::new(f)` for async with 1 extractor
- Up to 4 extractors supported

### 3. Router is synchronous

`Router::handle(Request) -> Response` is a synchronous call. Async handlers are
internally block_on'd via a static `OnceLock<Runtime>`. This means:

- The web module's Router is a request-response dispatcher, not an async server
- Actual HTTP serving requires: `TcpListener` → accept → parse HTTP → `router.handle(req)` → write response
- The `http::h1::server::Http1Server::serve(stream)` handles the parse/write loop for a single connection

### 4. SSE is batch-only via Sse type

`Sse::new(Vec<SseEvent>)` serializes all events into the response body at once.
For true SSE streaming (push events over time on a long-lived connection),
we need to build a custom response with:
- `Content-Type: text/event-stream`
- `Transfer-Encoding: chunked`
- Write events incrementally using the raw connection

### 5. Time API requires explicit `Time` values

`time::sleep(now, duration)` and `time::timeout(now, duration, future)` both require
a `Time` value as the first argument. There is no `Time::now()` function.
Options:
- `Time::from_millis(0)` for relative timers
- Obtain current time from `Cx` or timer driver
- In LabRuntime, use virtual time via `lab.now()`

### 6. Process module

- `Command::new()` with `.kill_on_drop(true)` works
- `wait()` is **synchronous** (blocking)
- `wait_with_output()` consumes self and returns stdout/stderr
- `Stdio::Pipe` (not `Stdio::Piped`)
- Process group management (`setsid`/`killpg`) not built-in — use `std::os::unix` directly

### 7. fastmcp_rust verdict

fastmcp-core/transport/protocol are all built on asupersync (no tokio dependency). However:
- fastmcp likely validates and parses MCP payloads we want to forward opaquely
- For a transport bridge, we need `RawValue` pass-through without interpretation
- **Recommendation:** Use custom JSON-RPC types with `serde_json::RawValue`
- **Optional:** fastmcp-transport could accelerate client-mode SSE/WS, worth evaluating later

## Go/No-Go Decision

**GO.** All 15 validation items are either "works" or "works-with-fallback".
No critical blockers. The fallbacks (SSE streaming, HTTP client, SSE client)
are straightforward to implement with the available low-level primitives.

## Quality Gates Passed

- `cargo build --release` — no warnings
- `cargo test --test validate` — 16/16 pass
- `cargo clippy --tests -- -D warnings` — no warnings
