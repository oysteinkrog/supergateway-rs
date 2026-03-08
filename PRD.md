# PRD: supergateway-rs — Rust Rewrite using Asupersync (v5)

## Overview
Rewrite supergateway (~2,100 LOC TypeScript MCP transport bridge) in Rust using Asupersync. Drop-in CLI replacement bridging MCP stdio servers with SSE, Streamable HTTP, and WebSocket transports. Implements MCP JSON-RPC handling from scratch using Asupersync's native stacks and structured concurrency.

## Goals
- Drop-in replacement for TypeScript `supergateway` in pm2 ecosystem config
- Full feature parity: all 6 transport modes, all CLI flags, all observable behaviors
- Correctness-first: zero leaked children/FDs, stable RSS, correct shutdown on all paths
- Performance: lower memory, lower latency, higher throughput than TypeScript in every mode
- Structured concurrency: child/session lifecycles tied to Asupersync Regions with automatic cleanup
- Protocol transparency: opaque JSON pass-through, no over-validation of payloads

## Non-Goals
- Publishing to crates.io
- Cross-platform: **Linux/WSL only** for v1. Windows and macOS explicitly out of scope
- TLS termination
- HTTP/2 or HTTP/3
- Backwards compatibility with TypeScript supergateway's npm package interface
- Authentication/authorization (handled upstream)

## Quality Gates
- `cargo +nightly build --release` — no warnings
- `cargo +nightly test` — all tests pass
- `cargo +nightly clippy -- -D warnings` — no warnings
- Compatibility harness passes identically for TS and Rust (with approved divergences documented)
- Benchmarks: criterion microbenchmarks + custom per-mode harness vs TypeScript

## Build Requirements
- **Nightly Rust** — pin exact nightly version in `rust-toolchain.toml` (e.g., `nightly-2026-03-01`)
- **Nightly update policy:** update monthly, run full test suite before merging. If nightly breaks Asupersync, stay on previous pin until upstream fixes.
- Pin `asupersync = "=0.2.7"` (exact, no range)
- Dependencies: `asupersync`, `clap` (derive), `serde`, `serde_json`, `uuid`, `nix`, `regex`

## Architecture

### Structured Concurrency Hierarchy
```
Server Region (top-level, owns HTTP listener)
  +-- Session Region (session-abc, stateful mode)
  |     +-- child process (kill_on_drop or explicit SIGTERM)
  |     +-- stdout reader task
  |     +-- stderr reader task
  |     +-- session timeout watcher
  +-- Session Region (session-def)
  |     +-- ...
  +-- Per-request Region (stateless mode: spawn->init->forward->kill)
```

Region close cascades: server shutdown -> all session Regions cancel -> children killed -> FDs closed.

### Asupersync API Usage (verified items marked, unverified flagged)

**Verified (confirmed in docs):**
- `Cx` is a **single unparameterized type** — `&mut Cx` (mutable ref). No `HasIo`/`HasSpawn`/`HasTime` type parameters. Capabilities are runtime-structural.
- Regions: use `cx.region(|scope| async { scope.spawn(task); })` form consistently. Name regions via `Region::open(cx, "name", ...)` if naming is needed (two forms exist — pick one, be consistent).
- `Outcome<T, E>` with 4 variants: `Ok`, `Err`, `Cancelled`, `Panicked`. Use throughout instead of `Result`.
- Two-phase channel sends: `tx.reserve(&cx).await?.send(msg)` — cancel-safe, no message loss.
- `cx.checkpoint()?` at cancellation points.
- Sync primitives take `&cx`: `mutex.lock(&cx).await?`, `rwlock.read(&cx).await?`
- Time: `asupersync::time::{sleep, timeout, interval}`. Clock via `cx.now()`.
- `LabRuntime` for deterministic testing with virtual time, seed-controlled scheduling, leak/quiescence oracles.

**Unverified (must validate in US-000 spike):**
- `asupersync::process::Command` — may not exist. Fallback: `std::process::Command` with a region-aware `ChildBridge` wrapper that sends SIGTERM to process group on region cancel.
- `kill_on_drop(true)` — may be Tokio-only. Fallback: explicit `Drop` impl on `ChildBridge` that calls `killpg`.
- `&mut Cx` vs `&Cx` — docs show `&mut Cx`. Verify that web framework handlers receive mutable Cx.
- SSE streaming: `Sse::new(vec![...])` may be batch-only. Fallback: raw `text/event-stream` chunked response with manual newline-delimited event framing.
- `WebSocketUpgrade` extractor — WebSocket support documented as incomplete. Fallback: manual HTTP upgrade + `net::websocket` frame handling.
- Web framework middleware: use `App::builder().middleware(X)` pattern (NOT `MiddlewareStack` — that name doesn't exist). Verify middleware ordering (onion model).

### CLI Flags (Complete Reference)

All flags from TypeScript supergateway must be supported. Mode-dependent defaults for `--outputTransport`.

| Flag | Type | Default | Scope | Description |
|------|------|---------|-------|-------------|
| `--stdio <cmd>` | string | — | Input | Command to run MCP server over stdio |
| `--sse <url>` | string | — | Input | SSE URL to connect to |
| `--streamableHttp <url>` | string | — | Input | Streamable HTTP URL to connect to |
| `--outputTransport` | choice | mode-dependent | Output | `stdio\|sse\|ws\|streamableHttp`. Default: `sse` for `--stdio`, `stdio` for `--sse`/`--streamableHttp` |
| `--port` | number | 8000 | Server modes | Port for output MCP server |
| `--baseUrl` | string | '' | stdio->SSE | Public base URL for SSE endpoint event. Prepended to messagePath for the POST URL advertised to SSE clients. Critical for reverse-proxy/path-prefixed deployments |
| `--ssePath` | string | /sse | stdio->SSE | Configurable path for SSE subscriptions |
| `--messagePath` | string | /message | stdio->SSE, stdio->WS | Configurable path for message POST endpoint (SSE) or WebSocket upgrade (WS) |
| `--streamableHttpPath` | string | /mcp | stdio->StreamableHTTP | Configurable path for Streamable HTTP endpoint |
| `--logLevel` | choice | info | All | `debug\|info\|none` |
| `--cors [origins]` | array | — | Server modes | No values = allow all. Supports exact, regex (`/pattern/`), wildcard |
| `--healthEndpoint [eps]` | array | [] | Server modes | One or more health check paths |
| `--header [headers]` | array | [] | All | Custom headers in `Key: Value` format |
| `--oauth2Bearer <token>` | string | — | All | Adds `Authorization: Bearer <token>` header |
| `--stateful` | boolean | false | stdio->StreamableHTTP | Enable stateful (per-session) mode |
| `--sessionTimeout <ms>` | number | — | Stateful HTTP only | Session idle timeout in milliseconds. Must be positive |
| `--protocolVersion` | string | 2024-11-05 | Stateless HTTP only | MCP protocol version for auto-initialization |

**Input validation:**
- Exactly one of `--stdio`/`--sse`/`--streamableHttp` required
- `--sessionTimeout` must be positive if provided
- Output transport must be valid for the chosen input mode
- Path flags must begin with `/`

### Key Modules
- **`cli.rs`** — clap argument parsing, all flags per table above
- **`jsonrpc.rs`** — JSON-RPC 2.0 types with serde. `RawValue` for pass-through. Batch support (see Protocol Transparency)
- **`codec.rs`** — Line-delimited async codec. Handles partial UTF-8 at chunk boundaries. Max frame size enforced
- **`child.rs`** — `ChildBridge`: spawn via `sh -c`, process group via `setsid`, stdin/stdout `.take()`-ed into separate tasks, SIGTERM->SIGKILL on drop/cancel. EPIPE detection and propagation
- **`session.rs`** — Session manager with access counting, timeout, state machine (Active->Closing->Closed). `Arc<RwLock<HashMap>>` with timer holding `Weak` ref
- **`cors.rs`** — CORS: exact, regex (`/pattern/`), wildcard. Preflight caching
- **`mcp_init.rs`** — Auto-init state machine for stateless mode
- **`observe.rs`** — Minimal observability counters (see Observability section)
- **`health.rs`** — Health/readiness endpoint with startup-readiness gating
- **`client/http.rs`** — HTTP/1.1 client on `asupersync::http` (or raw TCP)
- **`client/sse.rs`** — SSE client (EventSource) with reconnection
- **`gateway/*.rs`** — 6 gateway implementations

### Transport Semantics Matrix

| Property | stdio->SSE | stdio->WS | stdio->StreamableHTTP (stateful) | stdio->StreamableHTTP (stateless) | SSE->stdio | StreamableHTTP->stdio |
|---|---|---|---|---|---|---|
| **Child processes** | 1 shared | 1 shared | 1 per session | 1 per request | N/A (remote) | N/A (remote) |
| **Session start** | SSE connect | WS connect | `initialize` POST | each POST | stdin `initialize` | stdin `initialize` |
| **Session end** | client disconnect | WS close | DELETE or timeout | response complete | remote close | remote close |
| **Request ordering** | FIFO per child stdin (serialized writes) | FIFO per child stdin (serialized writes) | FIFO per session (serialized writes) | single request | FIFO per stdin | FIFO per stdin |
| **Concurrent in-flight** | Multiple (broadcast) | Multiple (routed by ID) | Multiple (single child, serialized stdin writes, response correlation by JSON-RPC id) | 1 (per-request child) | 1 (serial stdin) | 1 (serial stdin) |
| **Response routing** | Broadcast ALL | Per-client via ID multiplexer | Per-session | Direct response | Direct stdout | Direct stdout |
| **Notifications (server->client)** | Broadcast ALL | Broadcast ALL | SSE GET stream | N/A (stateless) | Write stdout | Write stdout |
| **Notifications (client->server)** | Forward to child, no response awaited | Forward to child, no response awaited | Forward to child, HTTP 202 | Forward to child, HTTP 202 | POST to remote, no response awaited | POST to remote, no response awaited |
| **Client disconnect** | Remove from map | Remove, stop routing | Close Region, kill child | N/A (request-scoped) | Exit process | Exit process |
| **Child exit** | Exit gateway process (code ?? 1) | Exit gateway process (code ?? 1) | Close session, reject subsequent requests with 503 | N/A | N/A | N/A |
| **Remote close** | N/A | N/A | N/A | N/A | Exit process (code 1) | Exit process (code 1) |
| **Remote error** | N/A | N/A | N/A | N/A | Log only, do NOT exit | Log only, do NOT exit |
| **Backpressure** | Per-client bounded channel | Per-client bounded channel | Per-session bounded channel | Per-request (1 deep) | OS pipe buffer | OS pipe buffer |

**Design decisions documented:**
- **SSE broadcast-to-all is intentional**: single shared child, all clients see all responses. This matches TypeScript behavior.
- **WebSocket ID multiplexer**: Use `HashMap<MangledId, (ClientId, OriginalId)>` instead of string concatenation. This **fixes** the TypeScript bug where string IDs or IDs containing `:` are silently mishandled. Document as intentional divergence.
- **Child exit in server modes (SSE/WS)**: gateway process exits. In stateful HTTP: only the affected session closes.
- **Remote close vs error in client modes**: close = exit(1), error = log only. This asymmetry is intentional for process managers (pm2/systemd) restart semantics.
- **Stdin writes to child are serialized**: all writes to a single child's stdin must be mutex-protected to prevent interleaved JSON messages.

### Protocol Transparency

**Critical for Rust rewrite correctness:**

The gateway SHALL treat JSON-RPC `params`, `result`, and `error.data` as **opaque JSON values**. Use `serde_json::RawValue` for pass-through. Unknown or extension method names and arbitrary payload shapes MUST be forwarded without schema-based rejection, coercion, or narrowing.

Specific rules:
- Unknown methods: forward without interpretation
- Arbitrary `params` shapes: forward as-is
- Arbitrary `result` shapes: forward as-is
- Arbitrary `error.data`: forward as-is
- **Batch JSON-RPC requests** (JSON arrays): forward to child as single line. If child responds with batch, forward batch response. If TS supergateway does not support batch, reject with documented error and add to `DIVERGENCES.md`
- **JSON-RPC notifications** (no `id`): forward to child without awaiting response. Transport-specific HTTP response: 202 Accepted (stateful/stateless HTTP modes)
- **Server-initiated requests**: forward from child/remote to client without interpretation. The MCP protocol is bidirectional

### Backpressure Design

| Scope | Buffer | Overflow policy |
|---|---|---|
| Per SSE client | Bounded mpsc channel, 256 msgs | Block producer. If blocked >30s, disconnect client |
| Per WS client | Bounded mpsc channel, 256 msgs | Block producer. If blocked >30s, disconnect client |
| Per stateful session | Bounded mpsc channel, 256 msgs | Block producer (backpressure to child via pipe buffer) |
| Per stateless request | 1 message deep | Direct response, no buffering needed |
| Child stdout->gateway | OS pipe buffer (64KB) + bounded channel | If channel full, pipe buffer fills, kernel blocks child write |
| Child stdin<-gateway | Serialized writes via mutex, unbounded (requests are small) | N/A — limited by incoming HTTP request rate |
| **Max message size** | 16 MB per JSON-RPC message (line) | Reject with JSON-RPC `-32600` (Invalid Request). HTTP: 413 Payload Too Large |
| **Max concurrent sessions** | 1024 (configurable) | Reject with HTTP 503 |
| **Max concurrent child processes** | 1024 (matches sessions) | Reject with HTTP 503 |
| **Max partial line buffer** | 64 MB | Kill child/session on overflow (protocol violation) |

**Deadlock prevention:** Child stdout MUST be continuously drained into the bounded channel. The stdout reader task runs in its own spawn within the session Region, never blocking on anything except the channel reserve. If the channel is full (slow client), the OS pipe buffer fills, and the kernel blocks the child's write — this is safe backpressure, not deadlock. Stderr is drained independently (logged, never queued).

### Auto-Initialization State Machine (`mcp_init.rs`)

Used only in stateless Streamable HTTP mode.

```
                         +---------------------------+
    incoming POST ------>| is_initialize_request()?  |
                         +----------+----------------+
                            yes |         | no
                                v         v
                         +----------+  +---------------------+
                         | Forward   |  | Store pending msg   |
                         | directly  |  | Send auto-init      |
                         | to child  |  | (--protocolVersion)  |
                         +----------+  +----------+----------+
                                                   |
                                        +----------v-----------+
                                        | Wait for init resp    |
                                        | (suppress from client)|
                                        +----------+-----------+
                                                   |
                                        +----------v-----------+
                                        | Send notifications/   |
                                        | initialized to child  |
                                        +----------+-----------+
                                                   |
                                        +----------v-----------+
                                        | Forward pending msg   |
                                        | to child              |
                                        +-----------------------+
```

**Auto-init request ID format:** `init_<timestamp_ms>_<random_alphanumeric_9>` (e.g., `init_1709856000000_a3b5c7d9e`). Use a prefix that won't collide with client-generated numeric IDs.

**Auto-init sequence (detailed):**
1. Check if incoming request is `initialize` — if yes, forward directly, set `isInitialized = true`
2. If not initialized and not already auto-initializing:
   a. Store incoming message as `pendingOriginalMessage`
   b. Set `isAutoInitializing = true`
   c. Generate auto-init request with `--protocolVersion` and synthetic client info
   d. Send `initialize` request to child stdin
   e. Wait for child response with matching init request ID
   f. **Suppress** the init response from reaching the client
   g. Send `notifications/initialized` notification to child
   h. Set `isInitialized = true`, `isAutoInitializing = false`
   i. Forward the original pending message to child
3. If already initialized: forward directly

**Race conditions addressed:**
- Auto-init is **exactly-once per request** (stateless mode = one child per request, no races)
- In stateful mode, auto-init does NOT apply — client must send explicit `initialize`
- If child exits during auto-init: Region cancels, return JSON-RPC `-32603` (Internal Error)
- If auto-init times out (5s): kill child, return JSON-RPC `-32603`
- No concurrent requests per child in stateless mode (one request per child lifecycle)
- If child emits notifications before init response: buffer and forward after init completes
- **TS batch edge case**: if a batch request arrives during auto-init, `pendingOriginalMessage` is overwritten (message loss). Rust: use a Vec to buffer multiple pending messages (improvement over TS)

**Stateful vs Stateless mode differences (complete):**

| Aspect | Stateful | Stateless |
|---|---|---|
| Child lifecycle | Per-session (long-lived) | Per-request (ephemeral) |
| Session tracking | UUID in `Mcp-Session-Id` header | None (`sessionIdGenerator: undefined`) |
| Auto-initialization | No — client sends explicit `initialize` | Yes — auto-init for non-init requests |
| GET endpoint | SSE stream for server-initiated messages | 405 Method Not Allowed |
| DELETE endpoint | Session termination | 405 Method Not Allowed |
| CORS exposedHeaders | Includes `Mcp-Session-Id` | Does not include `Mcp-Session-Id` |
| Error handling | No outer try/catch (SDK handles) | Outer try/catch returns 500 with `-32603` |
| Transport per request | Shared transport per session | New transport + server per request |
| Session counter | Access-counting with timeout | None |

### Session State Machine (Stateful HTTP)

```
+--------+    initialize POST     +---------+    DELETE or timeout    +---------+
| (none) | ---------------------->| Active  | ---------------------->| Closing |
+--------+                        +---------+                        +---------+
                                    |     ^                              |
                                    |     |  new POST/GET                |
                                    |     +----(accepted)                |
                                    |                                    |
                                    | POST/GET during close              | drain in-flight
                                    +----> reject 503                    | kill child
                                                                         | cleanup
                                                                         v
                                                                     +---------+
                                                                     | Closed  |
                                                                     +---------+
```

- **Active->Closing** transition: stop accepting new requests, cancel pending timeouts
- **Closing**: drain in-flight responses (up to 5s), then force-close
- **DELETE is idempotent**: DELETE on Closing/Closed returns success
- **Concurrent POST after DELETE**: rejected with 503
- **Session access counter**: increment on request accept, decrement on HTTP response terminal state (finish, close, error). Timeout MUST NOT fire while active-request counter > 0

### Observability (Minimal)

Logged at `info` level on change, queryable via health endpoint JSON response:

| Counter/Gauge | Description |
|---|---|
| `active_sessions` | Current session count |
| `active_children` | Current child process count |
| `active_clients` | Current SSE/WS client count |
| `total_requests` | Cumulative requests handled |
| `total_spawns` | Cumulative child processes spawned |
| `spawn_failures` | Child spawn errors |
| `session_timeouts` | Sessions cleaned up by timeout |
| `forced_kills` | Children killed by SIGKILL (SIGTERM didn't work) |
| `client_disconnects` | Transport disconnections |
| `backpressure_events` | Times a channel blocked producer |
| `queue_depth_max` | High-water mark of any channel |
| `decode_errors` | Invalid UTF-8 or JSON parse failures from child stdout |

### Health and Readiness

Health endpoints SHALL represent **readiness to relay MCP traffic**, not mere process liveness.

**Readiness conditions (all must be true):**
1. Child process started successfully (server modes with shared child: SSE, WS)
2. Transport/listener is bound and accepting connections
3. Gateway relay loop is active

**Before readiness and after fatal failure:** return HTTP 503 (not "ok").

**Endpoints:**
- `GET <configured-path>` returns `"ok"` (plain text) when ready, 503 when not
- `GET <configured-path>?detail=true` returns JSON with all counters (if `--logLevel debug`)

### Process Lifecycle (Linux/WSL only)

**Spawn sequence:**
1. `Command::new("sh").arg("-c").arg(stdio_cmd)` with `pre_exec(|| { setsid(); Ok(()) })`
2. `stdin(Stdio::piped())`, `stdout(Stdio::piped())`, `stderr(Stdio::piped())`
3. `.take()` stdin, stdout, stderr into separate async tasks

**EPIPE / broken pipe handling:**
- On EPIPE writing to child stdin: mark child as dead immediately, stop accepting new messages for that child/session, propagate error to caller with transport-appropriate status (HTTP 502, WS close, SSE error event)
- Stateful sessions with dead child are **terminally failed** (no auto-restart — MCP session state would be lost)
- Detection: check write result on every stdin write

**Shutdown sequence:**
1. Stop accepting new sessions/requests
2. Cancel all session Regions (triggers three-phase cancel: request->drain->finalize)
3. Each Region's drop: send SIGTERM to process group (`killpg(pgid, SIGTERM)`)
4. Wait up to 5s for child exit
5. If still alive: SIGKILL to process group
6. Drain remaining stdout/stderr (up to 1MB, then truncate with log warning)
7. Close all FDs
8. Exit process

**Container/PID 1 considerations:**
- When running as PID 1 (Docker without tini/dumb-init): must reap zombie children
- Document recommendation to use `tini` or `dumb-init` as entrypoint
- Shutdown timeout applies regardless of PID number
- Test in containerized environment (US-001)

**Grandchildren:** `setsid` creates a new process group. `killpg` sends signal to entire group, including grandchildren (e.g., `sh` -> `npm exec` -> `node mcp-server`). This handles the original TypeScript bug where `child.kill()` only killed `sh`, leaving `npm` and `node` orphaned.

**Stderr:** Captured and logged. Buffer limit: 64KB per read. Lines >4KB truncated with `[truncated]` suffix. Never queued — logged inline by stderr reader task. Invalid UTF-8 on stderr: lossy decode (replace with U+FFFD), do not crash.

**Stdout invalid UTF-8:** Fatal protocol violation for that child/session. Close session, return JSON-RPC `-32603` to any in-flight request. Log the decode error.

### Logging

- **Prefix**: All log messages prefixed with `[supergateway]`
- **Routing**: When `--outputTransport` is `stdio`, ALL log output (both info and error) goes to stderr. This prevents corrupting the JSON-RPC stream on stdout. Otherwise: info->stdout, error->stderr
- **Levels**: `none` = no-op, `info` = standard, `debug` = verbose with full-depth object inspection
- **Debug mode**: Full serialization of message contents. No header values logged at info level (only at debug)
- **No timestamps**: Plain text output (timestamps added by pm2/systemd)

### Custom Headers

User-configured response headers (`--header`, `--oauth2Bearer`) SHALL be applied to **all gateway-generated HTTP responses**:
- SSE stream responses (GET /sse)
- Message POST responses
- Health/readiness responses
- Streamable HTTP responses (POST/GET/DELETE /mcp)
- Error responses (4xx, 5xx)

Exception: hop-by-hop headers controlled by the transport layer.

**Header parsing:**
- Format: `Key: Value` — split at first `:` only (values may contain colons)
- Validate non-empty key and non-empty value (after trimming)
- Later headers with same key overwrite earlier ones (last-wins)
- `--oauth2Bearer <token>` adds `Authorization: Bearer <token>` header

**TS bug in `--oauth2Bearer`:** `'oauth2Bearer' in argv` is always true with yargs (undefined keys exist), producing `"Bearer undefined"` header when no token is provided. Rust: only add Authorization header if token is actually provided (non-empty string).

**TS bug in header count logging:** `Object(headers).length` always evaluates to undefined, so the log always says "(none)" even when headers are configured. Rust: log actual header count.

### `--outputTransport` Default Behavior

The TS code determines the default `outputTransport` using a **raw `process.argv` scan** rather than the parsed yargs values. It searches for the literal strings `--sse` and `--streamableHttp` in argv:
- If found: default to `stdio`
- If not found (i.e., `--stdio` mode): default to `sse`

This means the default depends on which input flag was used, not on the parsed value. Rust: use the clap-parsed input mode to determine the default (cleaner, same result).

### MCP SDK HTTP Semantics (Reimplemented from Scratch)

The TypeScript version delegates HTTP handling to the MCP SDK's transport classes. Since the Rust version implements from scratch, these SDK behaviors MUST be replicated exactly:

#### SSE Transport (stdio->SSE mode)

**SSE Connection (GET `<ssePath>`):**
- Response: `Content-Type: text/event-stream`, keep-alive, no-cache
- First event: `event: endpoint\ndata: <baseUrl><messagePath>?sessionId=<uuid>\n\n`
- Subsequent events: `event: message\ndata: <json>\n\n`
- The endpoint URL tells the client where to POST messages

**Message POST (POST `<messagePath>`):**
- SDK always returns **202 Accepted** for valid POSTs (regardless of whether child responds)
- Body: raw JSON (NOT parsed by express body parser — SDK uses `raw-body` with 4MB limit internally)
- **Express body parser MUST be skipped** on the message path. In Rust: read raw body directly, parse JSON ourselves
- sessionId comes from query parameter `?sessionId=<uuid>`

**Body size limit:** SDK's raw-body defaults to 4MB. Rust implementation should match (4MB default, configurable)

#### Streamable HTTP Transport (stdio->StreamableHTTP modes)

**Request Validation (SDK-enforced, must reimplement):**
1. **Accept header**: MUST include BOTH `application/json` AND `text/event-stream` → 406 Not Acceptable if missing either
2. **Content-Type**: MUST be `application/json` → 415 Unsupported Media Type if wrong
3. **Session validation** (stateful only): wrong/missing `Mcp-Session-Id` → **404 Not Found** (not 400)
4. **Protocol version**: SDK checks `Latest-Protocol-Version` header against `SUPPORTED_PROTOCOL_VERSIONS`

**Supported Protocol Versions:** `["2025-06-18", "2025-03-26", "2024-11-05", "2024-10-07"]`

**POST Response Types (determined by request content):**
- **Notification-only POST** (no `id` in body): HTTP **202 Accepted**, empty body
- **Request POST** (has `id`): **SSE stream response** — `Content-Type: text/event-stream`, one `event: message\ndata: <json>\n\n` per response/notification, stream closes when all requests in the batch are answered
- **Batch POST**: array of messages, some with `id`, some without. Notifications get 202 if ALL are notifications; otherwise SSE stream for the ones with `id`

**GET (Server-Initiated Messages, stateful only):**
- Client opens GET to receive server-initiated notifications/requests via SSE stream
- **409 Conflict** if a GET SSE stream is already open for this session (only one allowed)
- Returns `Content-Type: text/event-stream`

**DELETE (Session Termination, stateful only):**
- Closes session, kills child
- Returns **200 OK** (or 204)

**Stateless mode differences:**
- `sessionIdGenerator: undefined` — no session tracking
- GET → **405 Method Not Allowed**
- DELETE → **405 Method Not Allowed**
- No `Mcp-Session-Id` header in responses
- No CORS `exposedHeaders` for Mcp-Session-Id

#### Server Instance Usage Pattern (Important for Rust Design)

In the TS code, an `McpServer` instance is created but its callbacks (`onmessage`, `onerror`, etc.) are **overwritten** after `server.connect(transport)`. The server effectively acts as a pass-through facade. In Rust, we do NOT need an McpServer abstraction — the gateway directly manages the transport and child process.

### Error Mapping Table

| Condition | HTTP Status | JSON-RPC Error | Notes |
|---|---|---|---|
| Malformed JSON body | 400 | — | Not valid JSON-RPC, pure HTTP error |
| Valid JSON, invalid JSON-RPC | 200 | `-32600` Invalid Request | Per JSON-RPC 2.0 spec |
| Missing Accept headers (StreamableHTTP) | 406 | — | Must include both `application/json` AND `text/event-stream` |
| Wrong Content-Type (StreamableHTTP) | 415 | — | Must be `application/json` |
| No session / invalid session (stateful) | 404 | — | SDK returns 404, not 400 |
| Session closing/closed | 503 | `-32000` | After DELETE or timeout |
| Child process dead | 502 | `-32603` Internal Error | EPIPE or child exit |
| Auto-init timeout | 502 | `-32603` Internal Error | Stateless mode, 5s |
| Internal error (stateless catch-all) | 500 | `-32603` Internal Error | Outer try/catch in stateless mode |
| Message too large | 413 | `-32600` Invalid Request | >16 MB (configurable, SDK default 4MB) |
| Max sessions reached | 503 | — | Pure HTTP error |
| GET/DELETE on stateless | 405 | `-32000` Method Not Allowed | |
| GET SSE stream already open | 409 | — | Stateful mode, one GET SSE per session |
| Not ready (health) | 503 | — | Before readiness |
| CORS preflight | 204 | — | OPTIONS response |
| Notification-only POST | 202 | — | No `id` in body, accepted without response |

### WebSocket Transport Details (stdio->WS)

**ID Multiplexing:**
The TS version uses string concatenation (`clientId + ':' + msg.id`) for routing responses to specific WS clients. This has multiple bugs (see DIVERGENCES.md). The Rust version uses a proper `HashMap<MangledId, (ClientId, OriginalId)>` correlation map.

**Message flow:**
1. **Client→Child**: Client sends JSON-RPC message over WS. Gateway assigns a mangled ID (auto-increment u64), stores `(original_client_id, original_msg_id)` in correlation map, rewrites `id` in message, forwards to child stdin
2. **Child→Client (responses)**: Child responds with mangled ID. Gateway looks up correlation map, restores original ID, sends to correct client WS. Removes entry from map
3. **Child→All (notifications)**: Messages without `id` are broadcast to ALL connected WS clients
4. **Stale cleanup**: On client disconnect, remove all correlation entries for that client

**Health endpoint:**
- The TS code has a bug where all 3 health response branches execute (missing `return` statements, causes "headers already sent" errors)
- Rust: proper if/else with returns. `isReady` flag gates 200 vs 503

**Missing feature in TS:** `--header` CLI flag is NOT passed to WS mode (unlike SSE and HTTP modes). Rust should accept `--header` in WS mode too (document as intentional improvement in DIVERGENCES.md)

### Client Mode Specifics (SSE->stdio, HTTP->stdio)

**Init dance:**
1. First stdin message is `initialize`: create MCP client with caller's `clientInfo` and `capabilities`
2. First stdin message is NOT `initialize`: create fallback client with default info, connect immediately
3. Protocol version passthrough: intercept `initialize` request to remote, replace `protocolVersion` with value from original stdin `initialize` message. Use clean interceptor pattern (not monkey-patching)

**Init detection differences between TS modes:**
- SSE→stdio: uses string comparison `requestMessage.method === 'initialize'` (fragile)
- HTTP→stdio: uses Zod schema `InitializeRequestSchema.safeParse()` (robust)
- Rust: use `is_initialize_request()` helper from `jsonrpc.rs` — checks `method == "initialize"` on parsed JSON-RPC

**Error handling:**
- Extract error `code` and `message` from caught errors
- Strip SDK-generated prefix of form `MCP error <code>:` before forwarding error message to stdout
- Wrap response with `jsonrpc` and `id` fields
- **TS bug**: fallback path (non-init first message) leaves `result` undefined, then `result.hasOwnProperty('error')` throws TypeError. Rust: handle this correctly — always check for undefined/null before property access

**Message forwarding:**
- Request messages (with `method` and `id`): route through MCP client, await response, write to stdout
- **All other inbound messages** (notifications, server-initiated requests, unknown messages): forward directly to stdout in arrival order without interpretation. The gateway MUST NOT assume every inbound message corresponds to a locally outstanding request
- Pass-through validation: use permissive/opaque schema (equivalent of `z.any()`)

**Lifecycle:**
- Module-level singleton client (in Rust: `Arc<Mutex<Option<Client>>>` or equivalent safe pattern)
- Remote transport close -> `process::exit(1)` (triggers pm2/systemd restart)
- Remote transport error -> log only, do NOT exit
- Signal handling + stdin EOF/close -> graceful shutdown

**SSE client custom fetch:**
- SSE→stdio uses a custom `fetch` wrapper to inject headers into the EventSource connection
- This is needed because EventSource (SSE) doesn't natively support custom headers in browser APIs
- The `eventSourceInit.fetch` wrapper applies `--header` and `--oauth2Bearer` headers to SSE connection requests
- HTTP→stdio does NOT use custom fetch for EventSource — only uses `requestInit.headers` for POST requests

**Log direction labels (TS bug):**
- TS code has reversed log labels: `'SSE → Stdio:'` is used for messages going FROM stdio (wrong direction)
- Rust: use correct direction labels in all log messages

## Rust Implementation Design

### What We Reimplement vs What We Skip

**Reimplement from scratch (no MCP SDK equivalent in Rust):**
- All HTTP validation: Accept headers, Content-Type, session management, status codes
- SSE event framing: `event: <type>\ndata: <json>\n\n` format
- SSE client (EventSource): event parsing, reconnection with Last-Event-ID
- WebSocket transport: frame handling, upgrade, per-client routing
- JSON-RPC 2.0 message parsing and routing (with `RawValue` pass-through)
- Session state machine with access counting and timeout
- Auto-initialization state machine for stateless mode
- CORS middleware
- Process group management (`setsid`/`killpg`)

**Skip / Do not reimplement:**
- `McpServer` class — the TS code uses it as a facade (callbacks overwritten). Rust connects transport to child directly
- `Protocol` class auto-increment IDs — only relevant for client modes where we maintain our own ID counter for correlating requests to the remote server
- `InitializeRequestSchema` Zod validation — use simple `method == "initialize"` check
- SDK transport classes — we ARE the transport

### Core Types

```rust
// jsonrpc.rs — opaque pass-through
struct RawMessage {
    jsonrpc: String,         // always "2.0"
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<RawValue>,    // number, string, or null — preserved exactly
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<String>,  // present for requests/notifications
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<RawValue>,// opaque
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<RawValue>,// opaque
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RawValue>, // opaque (contains code, message, data)
}

// Helpers
fn is_request(&self) -> bool   // has id + method
fn is_response(&self) -> bool  // has id + (result or error), no method
fn is_notification(&self) -> bool // has method, no id
fn is_initialize_request(&self) -> bool // method == "initialize"

// Batch: Vec<RawMessage> when input is JSON array
```

### Gateway Architecture Pattern

Each gateway module follows this pattern:

```rust
// Pseudo-code for gateway lifecycle
async fn run_gateway(cx: &mut Cx, config: GatewayConfig) -> Outcome<(), Error> {
    // 1. Bind HTTP/WS listener
    let listener = bind(cx, config.port).await?;

    // 2. Spawn shared child (SSE/WS modes) or defer (HTTP modes)
    let child = match config.mode {
        SharedChild => Some(ChildBridge::spawn(cx, &config.stdio_cmd)?),
        PerSession | PerRequest => None,
    };

    // 3. Start relay loop in server Region
    cx.region(|scope| async {
        // Spawn child stdout reader (if shared child)
        // Spawn signal handler
        // Accept connections in loop
        // Each connection gets its own spawned task
    }).await
}
```

### Concurrency Model

| Mode | Shared State | Synchronization |
|---|---|---|
| stdio→SSE | child stdin, client map | Mutex for stdin writes, RwLock for client map |
| stdio→WS | child stdin, client map, correlation map | Mutex for stdin, RwLock for maps |
| stdio→Stateful HTTP | session map | RwLock for session map, Mutex per session's child stdin |
| stdio→Stateless HTTP | None (per-request) | No shared mutable state |
| SSE→stdio | singleton client | Arc<Mutex<Option<Client>>> |
| HTTP→stdio | singleton client | Arc<Mutex<Option<Client>>> |

## Performance Targets

| Target | Mode | Methodology | Baseline (TS) |
|---|---|---|---|
| RSS < 3 MB idle (gateway only) | All server | `ps -o rss=` after 60s idle | ~65 MB |
| RSS stable +/-10% under 1h soak | Stateful HTTP | 10k reqs/1h, RSS at t=5min vs t=60min | untested |
| Latency < 2ms p99 bridge overhead | Stateful HTTP | In-process mock child, HTTP-in to HTTP-out | ~5-10ms est |
| Latency < 5ms p99 end-to-end | Stateful HTTP | Real child, criterion per-request | ~10-20ms est |
| Throughput > 10k msg/sec | SSE (1 client, warm) | Custom harness, 200-byte payloads | ~2-5k est |
| Startup < 5ms to listening | All server | `hyperfine --warmup 3` | ~200ms |
| Binary < 10 MB stripped | N/A | `ls -la` after `strip` | N/A |
| Zero leaked children | All modes | Soak: 10k reqs + random disconnects, `pgrep` verify | leaks in stateless |
| Zero leaked FDs | All modes | Soak: check `/proc/<pid>/fd` count returns to baseline | untested |
| Graceful shutdown < 5s | All modes | SIGTERM -> verify exit within 5s, no zombies | untested |

**Note on RSS:** Rust allocators (jemalloc, system) may retain memory after deallocation. "Stable +/-10%" means the allocator plateau is acceptable; we do not require RSS to return to exact baseline.

## Compatibility Harness Design (US-001)

**Approach:** Shell script + small Rust test binary that exercises HTTP/SSE/WS endpoints. Runs against either `--binary /path/to/supergateway` (TS) or `--binary /path/to/supergateway-rs` (Rust).

**Reference version:** Pin to the exact TypeScript supergateway commit at project start. Record git hash in test suite.

**Failure classification:**
- **Parity regression:** Rust behaves differently from TS on a behavior that should be identical -> blocks merge
- **Intentional divergence:** Rust intentionally differs (documented in `DIVERGENCES.md`) -> passes
- **Unspecified behavior:** TS behavior is accidental/undefined -> Rust may choose either way

**Approved divergences (initial list — see DIVERGENCES.md for full details):**
- WebSocket ID multiplexer uses proper HashMap instead of string concatenation (fixes TS bug with string/colon IDs and parseInt destroying non-numeric IDs)
- Graceful child process group kill via `setsid`/`killpg` (TS only kills direct child, not grandchildren)
- Max message size enforced (16 MB; TS has no limit beyond SDK's 4MB raw-body default)
- Max concurrent sessions enforced (1024; TS has no limit)
- Health detail endpoint (`?detail=true`) is new
- Health readiness gating (503 before ready; TS returns "ok" unconditionally in some modes)
- Health endpoint return statements fixed (TS WS mode has missing returns, causes "headers already sent")
- Batch JSON-RPC: if TS doesn't support, Rust may support or reject with documented error
- `--oauth2Bearer` only adds header when token is actually provided (TS adds "Bearer undefined")
- `--header` works in WS mode (TS only passes headers to SSE and HTTP modes)
- Client mode fallback path handles undefined result correctly (TS crashes with TypeError)
- Auto-init buffers multiple pending messages (TS overwrites with last message on batch)
- Log direction labels are correct (TS has reversed labels in client modes)
- SSE mode explicitly kills child on signal (TS omits cleanup callback in SSE mode)
- WebSocket `onclose` callback properly fired on transport close (TS never calls it)
- No `console.log` for broadcast messages (TS logs notifications via console.log instead of logger)

**Required test coverage:**
- All 6 transport modes: round-trip message exchange
- Stateful HTTP: init->tools/list->session reuse->DELETE->child killed
- Stateless HTTP: non-init triggers auto-init
- SSE: connect->POST->receive via SSE, broadcast to multiple clients
- WebSocket: connect->send->receive, multi-client ID routing, string IDs
- SSE->stdio: mock SSE server, round-trip
- StreamableHTTP->stdio: mock HTTP server, round-trip
- Health endpoint readiness gating
- CORS preflight (OPTIONS + allowed origins + regex)
- Custom headers on responses (SSE, POST, health)
- Client disconnect -> child cleanup (no zombies)
- SIGTERM -> graceful shutdown within 5s
- stdin EOF -> graceful shutdown
- `--baseUrl` with reverse proxy simulation
- Configurable `--ssePath` and `--messagePath`
- Notification pass-through (both directions)
- Server-initiated request forwarding
- Large message rejection (>16 MB)
- Rapid connect/disconnect cycles (100+)
- Concurrent POSTs to same stateful session
- Child exit behavior per mode
- Container signal forwarding (PID 1 test)
- Error prefix stripping in client modes

## User Stories

### US-000: Asupersync Web Stack Validation Spike [GO/NO-GO GATE]
**Description:** Validate Asupersync can serve the required workloads before committing.

**Acceptance Criteria:**
- [ ] HTTP server: POST/GET/DELETE routes via `asupersync::web::Router` or `App::builder()`
- [ ] SSE streaming: push events one-at-a-time over long-lived connection. If batch-only, demonstrate raw chunked `text/event-stream`
- [ ] WebSocket: upgrade via `WebSocketUpgrade` extractor or manual HTTP upgrade + `net::websocket`. Bidirectional exchange
- [ ] HTTP client: GET and POST via `asupersync::http` or raw TCP
- [ ] SSE client: connect, receive streamed events
- [ ] Child process: determine if `asupersync::process::Command` exists. If not, demonstrate `std::process::Command` with async I/O adapter inside a Region
- [ ] Verify `kill_on_drop` or equivalent — child killed when Region closes
- [ ] Verify `&mut Cx` vs `&Cx` in handler signatures
- [ ] Timer: `asupersync::time::timeout` wrapping a future
- [ ] Region lifecycle: spawn tasks, cancel region, verify cleanup
- [ ] Sync primitives: `mutex.lock(&cx).await?` works as documented
- [ ] Evaluate `fastmcp_rust` — reusable MCP handling? Document findings
- [ ] LabRuntime: deterministic test with virtual time works
- [ ] Nightly Rust: pin toolchain version, verify Docker build
- [ ] **Go/no-go documented.** For each item: works / works-with-fallback / blocked. If any critical item is blocked with no fallback, project does not proceed.

### US-001: Compatibility Harness
**Acceptance Criteria:**
- [ ] Test runner accepts `--binary` flag
- [ ] Mock MCP stdio server (Rust binary): responds to `initialize`, `tools/list`, `echo`, sends notifications on `--notify` flag, supports server-initiated requests
- [ ] Mock SSE server and mock Streamable HTTP server (for client-mode tests)
- [ ] Pin TypeScript supergateway reference commit
- [ ] `DIVERGENCES.md` with initial approved divergence list
- [ ] Tests: all items listed in "Required test coverage" section above
- [ ] Failure classification: parity / intentional-divergence / unspecified
- [ ] All tests pass against TypeScript binary (baseline)
- [ ] Container test: run as PID 1, verify signal forwarding and zombie reaping

### US-002: Project Scaffolding and CLI
**Acceptance Criteria:**
- [ ] `/c/WORK/supergateway-rs` with Cargo.toml, `rust-toolchain.toml` (pinned nightly)
- [ ] clap struct with ALL flags per CLI Flags table. Mode-dependent defaults for `--outputTransport`
- [ ] Mutual exclusivity: exactly one of `--stdio`/`--sse`/`--streamableHttp`
- [ ] Path flags validated to start with `/`
- [ ] `--sessionTimeout` validated positive
- [ ] `--help` output matches TypeScript behavior
- [ ] Exit codes: 0 success, 1 error (match TS)
- [ ] `cargo +nightly build --release` succeeds

### US-003: JSON-RPC 2.0 Types
**Acceptance Criteria:**
- [ ] `JsonRpcMessage` enum: Request, Response, Notification, Error
- [ ] `is_initialize_request()` helper
- [ ] `RawMessage` using `serde_json::RawValue` for **opaque pass-through** of params/result/error.data
- [ ] Batch support: deserialize JSON array as `Vec<JsonRpcMessage>`
- [ ] Unknown methods: deserialize without rejection
- [ ] Round-trip tests: request, response, notification, error, batch, null id, string id, numeric id, unknown method, large params

### US-004: Line-Delimited Stdio Codec
**Acceptance Criteria:**
- [ ] `LineCodec` reads async reader, buffers, splits `\n`, yields `JsonRpcMessage`
- [ ] Handles partial UTF-8 at chunk boundaries correctly
- [ ] **Invalid UTF-8 from stdout**: fatal protocol violation. Close session, return `-32603` to in-flight requests. Log decode error with counter increment
- [ ] **Invalid UTF-8 from stderr**: lossy decode (U+FFFD replacement), log, do not crash
- [ ] Skips empty lines, logs+skips invalid JSON (with `decode_errors` counter)
- [ ] `write_message()` serializes + `\n`, mutex-protected for shared-child modes
- [ ] **Max partial buffer size**: 64 MB. Kill child/session on overflow
- [ ] **EOF handling**: if trailing buffer after EOF is valid JSON, emit as final frame. If invalid, log warning and discard
- [ ] Unit tests: partial buffer, empty lines, invalid JSON, >64KB message, rapid sequential, batch (JSON array on single line), invalid UTF-8, EOF with/without trailing newline, buffer overflow
- [ ] `criterion` benchmark: parse 10k messages

### US-005: Child Process Manager (`ChildBridge`)
**Acceptance Criteria:**
- [ ] Spawn via `sh -c` with `pre_exec(setsid)` for process group
- [ ] Use `asupersync::process::Command` if available (per US-000 findings), else `std::process::Command` with async wrapper
- [ ] Stdin/stdout `.take()`-ed into separate async tasks
- [ ] `kill()`: SIGTERM to pgid -> 5s wait -> SIGKILL
- [ ] `Drop` impl sends SIGTERM to pgid (safety net)
- [ ] **EPIPE detection**: on broken stdin pipe, mark child dead, propagate error to caller
- [ ] **Child exit detection**: detect via wait status, propagate to owning session/gateway
- [ ] Stderr: read + log, 64KB buffer, 4KB line truncation, lossy UTF-8 decode
- [ ] Spawned inside Region — Region cancel kills child
- [ ] **Zombie reaping**: when running as PID 1, properly reap child processes
- [ ] Integration test: spawn, read message, cancel Region, verify pgid killed within 2s
- [ ] Integration test: write to dead child stdin, verify EPIPE handling
- [ ] Integration test: rapid spawn/kill cycles (100+), verify no leaked pids or fds

### US-006: Session Manager
**Acceptance Criteria:**
- [ ] `SessionManager<S>` with `Arc<RwLock<HashMap<String, Session<S>>>>`
- [ ] **Session state machine**: Active -> Closing -> Closed (see architecture diagram)
- [ ] `inc(id)` / `dec(id)` — reference-counted request tracking
- [ ] **dec trigger**: called when HTTP response reaches terminal state (finish, close, error/abort)
- [ ] Count->0: start timeout timer (timer holds `Weak` ref to map)
- [ ] New request during timeout: cancel timer, reactivate
- [ ] Timeout fires: remove session, call cleanup callback. **Timeout MUST NOT fire while active-request counter > 0**
- [ ] `clear(id, run_cleanup)` for explicit teardown (DELETE)
- [ ] **DELETE idempotency**: DELETE on Closing/Closed returns success
- [ ] **Concurrent POST after DELETE**: rejected with 503
- [ ] Max sessions limit (1024, configurable). Reject with 503 when full
- [ ] **Session ID entropy**: use `uuid::Uuid::new_v4()` — cryptographically random, unguessable
- [ ] Unit tests with `LabRuntime`: timeout fire, timeout cancel, concurrent inc/dec, clear during timeout, max sessions, state machine transitions, DELETE idempotency, concurrent POST+DELETE race

### US-007: CORS and Headers
**Acceptance Criteria:**
- [ ] Exact match, regex (`/pattern/` via `regex` crate), wildcard
- [ ] OPTIONS preflight: Allow-Origin, Allow-Methods (GET/POST/DELETE/OPTIONS), Allow-Headers (Content-Type, Mcp-Session-Id), **Access-Control-Max-Age** (configurable, default 86400)
- [ ] Stateful mode: Expose-Headers includes Mcp-Session-Id
- [ ] **Credentials**: wildcard origin MUST NOT be combined with credentials. Reject config or reflect specific origin
- [ ] `--header "key: value"` and `--oauth2Bearer` parsing
- [ ] Max header size: 8KB. Reject oversized with 431
- [ ] **Headers applied to all response types** (SSE, POST, health, error responses)
- [ ] Unit tests: each match type, rejection, header parsing, preflight, credentials edge case

### US-008: Logging and Observability
**Acceptance Criteria:**
- [ ] `LogLevel` enum: debug/info/none
- [ ] stdio output transport -> all logs to stderr; otherwise info->stdout, error->stderr
- [ ] **`[supergateway]` prefix** on all messages
- [ ] Plain text, no timestamps
- [ ] Debug mode: full-depth object serialization
- [ ] Counters (see Observability section): logged on change at info level
- [ ] Health endpoint with readiness gating and detail mode
- [ ] No header values logged at info level (only at debug)

### US-009: Signal Handling and Graceful Shutdown
**Acceptance Criteria:**
- [ ] SIGINT, SIGTERM, SIGHUP handled
- [ ] **stdin EOF/close**: triggers graceful shutdown equivalent to SIGTERM. Critical for client modes where parent process may close stdin
- [ ] Shutdown sequence: stop accepting -> cancel server Region -> cascade -> exit
- [ ] `cx.checkpoint()?` at all cancellation points
- [ ] **PID 1 behavior**: properly reap zombie children when running as container init
- [ ] Integration test: start + SIGTERM -> child pgid killed within 2s, gateway exits cleanly, zero zombies
- [ ] Integration test: stdin close -> same behavior as SIGTERM
- [ ] Integration test: PID 1 + SIGTERM -> children reaped, clean exit
- [ ] Verify: `/proc/<pid>/fd` count at 3 (stdin/stdout/stderr) after shutdown

### US-010: Gateway — stdio->Streamable HTTP (Stateful)
**Acceptance Criteria:**
- [ ] POST `/mcp` + no session + initialize -> spawn child in session Region, return Mcp-Session-Id
- [ ] POST + valid session -> forward to child (stdin writes serialized via mutex)
- [ ] GET + valid session -> SSE notification stream (one per session, 409 if already open)
- [ ] DELETE + valid session -> transition to Closing, drain in-flight, kill child (idempotent)
- [ ] **Accept header validation**: POST must include both `application/json` AND `text/event-stream` → 406
- [ ] **Content-Type validation**: POST must be `application/json` → 415
- [ ] Invalid/missing session -> **404** (not 400 — matches SDK behavior)
- [ ] Session closing -> 503
- [ ] Session timeout via US-006, respecting active-request counter
- [ ] Health, CORS, headers via US-007/008
- [ ] Backpressure: bounded channel per session
- [ ] Protocol version passthrough: client's `protocolVersion` in initialize forwarded exactly to child
- [ ] **Notifications from client**: forward to child, respond HTTP **202 Accepted**
- [ ] **Request POSTs**: respond with SSE stream (text/event-stream), one `event: message` per response
- [ ] **Server-initiated messages**: forward via GET SSE stream
- [ ] **Concurrent POSTs**: serialized stdin writes, response correlation by JSON-RPC `id`
- [ ] **CORS exposedHeaders**: includes `Mcp-Session-Id`
- [ ] **Response lifecycle**: session counter inc on request accept, dec on `res.finish` or `res.close` event
- [ ] **Transport onclose/onerror**: clear session counter (runCleanup=false), delete transport, kill child
- [ ] Compatibility harness passes

### US-011: Gateway — stdio->Streamable HTTP (Stateless)
**Acceptance Criteria:**
- [ ] Per-request Region: new child + transport per POST. spawn child, auto-init if needed (via `mcp_init.rs`), forward, respond, kill
- [ ] Auto-init state machine per architecture diagram (5 state variables: isInitialized, initializeRequestId, isAutoInitializing, pendingOriginalMessages, buffer)
- [ ] `--protocolVersion` controls auto-init version string (default: `2024-11-05`)
- [ ] Auto-init timeout: 5s, then kill + JSON-RPC `-32603`
- [ ] **Child notifications before init response**: buffer and forward after init completes
- [ ] **Accept header validation**: POST must include both `application/json` AND `text/event-stream` → 406
- [ ] **Content-Type validation**: POST must be `application/json` → 415
- [ ] GET/DELETE -> **405 Method Not Allowed** (not 400)
- [ ] **Notifications from client**: forward to child, respond HTTP **202 Accepted**
- [ ] **Request POSTs**: respond with SSE stream (text/event-stream)
- [ ] **Outer try/catch**: unlike stateful mode, stateless wraps entire handler in try/catch, returning 500 + JSON-RPC `-32603` on unexpected errors
- [ ] **No CORS exposedHeaders** for Mcp-Session-Id (no sessions)
- [ ] **No sessionIdGenerator** — transport created with `sessionIdGenerator: undefined` equivalent
- [ ] **Batch pending messages**: if batch request arrives, buffer ALL messages (not just one — fixes TS bug)
- [ ] Compatibility harness passes

### US-012: Gateway — stdio->SSE
**Acceptance Criteria:**
- [ ] Single child spawned at startup
- [ ] GET `<ssePath>` -> SSE connection, `endpoint` event with POST URL constructed from `baseUrl + messagePath`
- [ ] POST `<messagePath>?sessionId=` -> forward to child stdin, respond **202 Accepted** always
- [ ] **`--baseUrl` support**: POST URL in endpoint event = `${baseUrl}${messagePath}?sessionId=${sessionId}`. Without baseUrl, use listener address
- [ ] **Configurable paths**: use `--ssePath` and `--messagePath` CLI flags (not hard-coded)
- [ ] Missing sessionId -> 400
- [ ] No active session -> 503
- [ ] Child stdout **broadcast to ALL** SSE clients (intentional — documented in transport matrix)
- [ ] Sessions tracked, cleaned on disconnect
- [ ] **Dual disconnect detection**: both request close AND transport close events trigger cleanup
- [ ] Backpressure: per-client bounded channel. Blocked >30s -> disconnect
- [ ] **Rapid connect/disconnect**: no leaked tasks, fds, or stale entries after 100+ cycles
- [ ] **Custom headers on all responses** (SSE stream, POST, health)
- [ ] **Express body parser skip**: message path must NOT use body-parsing middleware. Read raw body directly, parse JSON manually (SDK uses `raw-body` internally with 4MB limit)
- [ ] **Child exit -> gateway process exit** with child's exit code (or 1)
- [ ] **TS note**: cleanup callback NOT passed to onSignals in SSE mode (child not explicitly killed on signal — relying on process exit). Rust: explicitly kill child on signal for correctness
- [ ] Compatibility harness passes

### US-013: Gateway — stdio->WebSocket
**Acceptance Criteria:**
- [ ] Single child spawned at startup
- [ ] WebSocket upgrade on `<messagePath>` (configurable via CLI flag)
- [ ] **Proper ID multiplexer**: `HashMap<MangledId, (ClientId, OriginalId)>` — NOT string concatenation
- [ ] Supports string IDs and IDs containing `:` (intentional divergence from TS)
- [ ] Notifications (no `id`) broadcast to all
- [ ] Responses routed to specific client by JSON-RPC `id` correlation
- [ ] **Stale ID cleanup**: remove correlation entries on client disconnect and on response delivery
- [ ] Health endpoints on HTTP with **readiness gating** (503 before child started + transport ready)
- [ ] **Child exit -> gateway process exit**
- [ ] **Rapid connect/disconnect**: no leaked tasks, routing entries, fds
- [ ] Compatibility harness passes (with divergence flag for ID handling)

### US-014: HTTP Client Stack
**Acceptance Criteria:**
- [ ] HTTP/1.1 client on `asupersync::http` or raw TCP
- [ ] GET with streaming response (for SSE client)
- [ ] POST with JSON body
- [ ] Custom headers (per-request)
- [ ] Mcp-Session-Id header management
- [ ] Connection keep-alive with bounded pool (max 4 idle per host)
- [ ] **Timeout matrix**: connect 10s, request 30s, idle 60s
- [ ] **No automatic retry** of non-idempotent JSON-RPC POSTs
- [ ] **Redirects**: follow up to 5 redirects for GET, do NOT follow for POST
- [ ] Unit test against mock server

### US-015: SSE Client (EventSource)
**Acceptance Criteria:**
- [ ] Built on US-014 HTTP client
- [ ] Parse `text/event-stream`: data/event/id/retry fields
- [ ] **Reconnection**: exponential backoff (1s, 2s, 4s, ... max 30s) with `Last-Event-ID` header
- [ ] **During disconnect**: buffer stdin input up to 256 messages, reject with error if buffer full
- [ ] Auth/custom headers preserved on reconnect
- [ ] Unit test: parse multi-event stream from mock, reconnect simulation

### US-016: Gateway — SSE->stdio
**Acceptance Criteria:**
- [ ] Read stdin JSON-RPC, POST to remote SSE server
- [ ] Receive via SSE, write to stdout
- [ ] Init dance: first `initialize` -> create client with caller's `clientInfo`/`capabilities`; non-init first -> fallback client
- [ ] Protocol version passthrough (clean interceptor, not monkey-patch)
- [ ] Custom headers on SSE + POST
- [ ] **Error prefix stripping**: remove `MCP error <code>:` prefix from error messages before stdout
- [ ] **Non-request message forwarding**: all inbound messages not consumed as responses forwarded to stdout in arrival order
- [ ] **Server-initiated requests**: forwarded without interpretation
- [ ] Error responses -> JSON-RPC errors on stdout with correct code/message
- [ ] **Remote close -> process.exit(1)**. Remote error -> log only
- [ ] **stdin EOF -> graceful shutdown**
- [ ] Compatibility harness passes

### US-017: Gateway — Streamable HTTP->stdio
**Acceptance Criteria:**
- [ ] Read stdin, POST to remote Streamable HTTP URL
- [ ] Mcp-Session-Id management
- [ ] Same init dance as US-016
- [ ] Custom headers
- [ ] **Error prefix stripping**: same as US-016
- [ ] **Non-request message forwarding**: same as US-016
- [ ] **Remote close -> process.exit(1)**. Remote error -> log only
- [ ] **stdin EOF -> graceful shutdown**
- [ ] Compatibility harness passes

### US-018: Benchmark Suite
**Acceptance Criteria:**
- [ ] `criterion` codec benchmarks (parse 10k, RawValue 10k)
- [ ] Per-mode latency harness (stateful HTTP: 1000 requests, p50/p95/p99)
- [ ] Per-mode throughput harness (SSE: 1000 msgs, WS: 1000 msgs)
- [ ] **Soak test**: 10k requests + random disconnects over 10min, measure RSS stability + leaked children + leaked FDs
- [ ] **Connection churn test**: 1000 rapid connect/disconnect cycles, verify stable memory + task count + fd count
- [ ] **Large message test**: 1MB, 10MB, 16MB messages; verify correct handling and rejection >16MB
- [ ] Startup benchmark: `hyperfine`
- [ ] Mock MCP server as child
- [ ] Run same tests against TS binary
- [ ] `BENCHMARKS.md` with methodology + results
- [ ] All Rust numbers beat TS (or documented explanation)

### US-019: pm2 Integration
**Acceptance Criteria:**
- [ ] Binary accepts same args as TS `supergateway`
- [ ] Update `~/.config/pm2/ecosystem.config.js` SUPERGATEWAY path
- [ ] `pm2 restart` -> all instances online
- [ ] Each MCP server responds to Claude Code tool calls
- [ ] RSS < 5 MB after 60s idle (measured)
- [ ] Run 24h with normal usage -> zero leaked children, stable RSS, no unexpected restarts
- [ ] Verify correct behavior behind reverse proxy (if applicable)

## Story Dependencies
```
US-000 (spike) ---- GO/NO-GO GATE
  |
  +-- US-001 (compat harness) --+
  |                              |
  +-- US-002 (scaffolding)       |
        |                        |
        +-- US-003 (JSON-RPC)    |
        |     +-- US-004 (codec) |
        |           +-- US-005 (child) --+
        |                 |              |
        |                 +-- US-010 (stateful HTTP) <-- US-006 + US-007 + US-001
        |                 +-- US-011 (stateless HTTP) <-- US-007 + US-001
        |                 +-- US-012 (SSE) <-- US-007 + US-001
        |                 +-- US-013 (WS) <-- US-007 + US-001
        |
        +-- US-006 (session mgr) <-- US-003
        +-- US-007 (CORS+headers)
        +-- US-008 (logging+obs) -- parallel with US-003
        +-- US-009 (signals) -- parallel with US-003
        |
        +-- US-003 -> US-014 (HTTP client)
        |     +-- US-015 (SSE client)
        |           +-- US-016 (SSE->stdio) <-- US-001
        |
        +-- US-014 -> US-017 (HTTP->stdio) <-- US-001

  All gateways -> US-018 (benchmarks) -> US-019 (pm2)
```

Critical path: US-000 -> US-002 -> US-003 -> US-004 -> US-005 -> US-010 -> US-019

## Testing Plan

**Unit:** jsonrpc (including batch, unknown methods, opaque payloads), codec (UTF-8, EOF, overflow), cors, session (LabRuntime — state machine, races), child (EPIPE, spawn/kill), mcp_init
**Integration:** Per-gateway with mock MCP server, session lifecycle, concurrent sessions, disconnect cleanup, signal handling, stdin EOF, PID 1 behavior, rapid churn
**Soak:** 10k requests/10min, RSS stability, leaked children, leaked FDs (`/proc/<pid>/fd`), connection churn
**Deterministic:** LabRuntime for all timer-dependent tests
**Compatibility:** Black-box harness against both binaries
**Platform CI:** Linux x86_64 (WSL). Single platform for v1.

## Risk Register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `asupersync::process::Command` doesn't exist | High | High | US-000 validates. Fallback: `std::process::Command` + `ChildBridge` wrapper |
| SSE streaming is batch-only | Medium | High | US-000 validates. Fallback: raw chunked response |
| WebSocket support incomplete | Medium | High | US-000 validates. Fallback: manual HTTP upgrade + frame handling |
| Asupersync API breaks on update | High | Medium | Pin `=0.2.7`. Wrap behind internal traits |
| `fastmcp_rust` provides shortcuts | Medium | Positive | Evaluate in US-000 |
| Session cleanup ownership in Rust | Certain | Medium | `Weak` ref design in US-006, LabRuntime tests, explicit state machine |
| Process group kill edge cases on WSL | Low | High | Test in US-005, fallback `/proc` iteration |
| Nightly Rust breaks | Medium | Low | Pin version, monthly update policy |
| Stateless mode process-per-request bottleneck | Low | Low | Documented: stateful recommended. Stateless for compat only |
| MCP spec evolution | Low | Low | Wire protocol only; small codec updates |
| Over-modeling payloads in Rust (strict typing) | High | High | `RawValue` pass-through mandated. Protocol transparency section enforces opaque forwarding |
| Container PID 1 zombie accumulation | Medium | Medium | Document tini/dumb-init recommendation, test in US-001 |
| Singleton client state unsafety in Rust | Medium | Medium | `Arc<Mutex<>>` pattern specified, no raw mutable statics |
| SDK HTTP validation not replicated correctly | High | High | Detailed validation matrix in MCP SDK HTTP Semantics section. Integration tests against real MCP clients |
| SSE event framing bugs | Medium | High | Unit tests for exact `event: <type>\ndata: <json>\n\n` format. Test with multiple MCP clients (Claude Desktop, Cursor, etc.) |
| Body size limit mismatch | Low | Medium | Match SDK's 4MB default. Make configurable. Document in CLI flags |

## Open Questions (All Resolved)
1. ~~Vendor vs crates.io~~ -> crates.io, pin exact
2. ~~Middleware API~~ -> `App::builder().middleware()`, verify in spike
3. ~~Child process with Regions~~ -> `ChildBridge` wrapper, verify in spike
4. ~~fastmcp_rust~~ -> Evaluate in spike
5. ~~Nightly~~ -> Required, pin version
6. ~~&mut Cx vs &Cx~~ -> Likely `&mut Cx`, verify in spike
7. ~~Capability types~~ -> Removed. Cx is unparameterized
8. ~~Batch JSON-RPC~~ -> Support if TS supports, else reject with documented divergence
9. ~~baseUrl flag~~ -> Required for SSE mode, added to CLI flags and US-012
10. ~~stdin close~~ -> Added to US-009 as shutdown trigger
11. ~~Session concurrency~~ -> Serialized stdin writes, response correlation by id

## Success Metrics
- Compatibility harness: 100% parity (excluding documented divergences)
- 5 pm2 instances on Rust binary for 1 week, zero unexpected restarts
- RSS < 5 MB measured (vs ~65 MB TS)
- Latency p99 < 2ms bridge overhead
- Zero leaked children/FDs after any shutdown/timeout/disconnect
- Binary < 10 MB stripped
