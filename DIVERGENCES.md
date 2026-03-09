# Intentional Divergences from TypeScript supergateway

This document catalogs all intentional behavioral differences between `supergateway-rs` (Rust) and the original TypeScript `supergateway`. Each divergence is either a **bug fix** (TS behavior is wrong) or an **improvement** (Rust adds safety/correctness).

## Bug Fixes

### D-001: WebSocket ID multiplexer uses HashMap instead of string concatenation
**TS behavior:** Mangles JSON-RPC `id` by string concatenation: `clientId + ':' + msg.id`. Unmangles with `split(':')` and `parseInt(rawId, 10)`.
**Bugs this causes:**
- `parseInt` destroys non-numeric JSON-RPC IDs (string IDs become `NaN`)
- IDs containing `:` character are split incorrectly (wrong clientId extracted)
- Mutates message object in-place (`msg.id = parseInt(...)`)
**Rust behavior:** Uses `HashMap<MangledId, (ClientId, OriginalId)>` with auto-increment `u64` mangled IDs. Original IDs preserved exactly regardless of type.
**Impact:** Clients using string IDs or IDs with colons now work correctly.

### D-002: Health endpoint return statements (WS mode)
**TS behavior:** WS mode health endpoint handler has missing `return` statements in all 3 branches (`!isReady`, normal, detail). All branches execute sequentially, causing "headers already sent" errors.
**Rust behavior:** Proper if/else with early returns. Only one response sent per request.

### D-003: `--oauth2Bearer` header when no token provided
**TS behavior:** `'oauth2Bearer' in argv` is always true with yargs (undefined keys exist in the argv object). Produces `"Authorization: Bearer undefined"` header when no token is actually provided.
**Rust behavior:** Only adds Authorization header if `--oauth2Bearer` was actually provided with a non-empty value.

### D-004: Client mode fallback path crash
**TS behavior:** In both SSE→stdio and HTTP→stdio, when the first stdin message is NOT an `initialize` request, the fallback path leaves `result` undefined. The subsequent `result.hasOwnProperty('error')` throws a TypeError, crashing the process.
**Rust behavior:** Properly handle the fallback case. If result is None, return a JSON-RPC error response.

### D-005: Auto-init pending message overwrite on batch
**TS behavior:** In stateless mode, `pendingOriginalMessage` is a single variable. If a batch request arrives during auto-init, it overwrites the previous pending message, losing it.
**Rust behavior:** Uses a `Vec<RawMessage>` to buffer all pending messages during auto-init.

### D-006: Log direction labels reversed in client modes
**TS behavior:** Uses `'SSE → Stdio:'` label for messages going FROM stdio TO the remote server (direction reversed).
**Rust behavior:** Correct direction labels in all log messages.

### D-007: WebSocket `onclose` callback never fired
**TS behavior:** `WebSocketServerTransport.close()` clears the clients map and closes the WSS, but never calls `this.onclose?.()`. Also doesn't fire `ondisconnection` for each remaining client.
**Rust behavior:** Properly fires close callbacks and per-client disconnection events on transport shutdown.

### D-008: WebSocket broadcast uses console.log instead of logger
**TS behavior:** When a notification (no `id`) arrives, the `onmessage` setter logs `console.log('Broadcast message:', msg)` directly instead of using the configured logger.
**Rust behavior:** Uses the configured logger at debug level.

### D-009: Header count logging always shows "(none)"
**TS behavior:** `Object(headers).length` always evaluates to `undefined`, so the header count log message always says "(none)" even when headers are configured.
**Rust behavior:** Logs actual count of configured headers.

### D-010: WS mode child stderr logged at correct level
**TS behavior:** `stdioToWs.ts:90` logs child stderr with `logger.info()` instead of `logger.error()`. All other modes use `logger.error()` for stderr.
**Rust behavior:** All modes log child stderr at `error` level consistently.

### D-011: SSE mode doesn't explicitly kill child on signal
**TS behavior:** `stdioToSse.ts:67` passes no cleanup callback to `onSignals()`. The child process is only killed implicitly by the parent process exiting (which may not kill grandchildren).
**Rust behavior:** Explicitly sends SIGTERM to process group on signal, ensuring all descendants are killed.

## Improvements (New Behavior)

### D-100: Process group management with setsid/killpg
**TS behavior:** `child.kill()` only kills the direct child process (typically `sh`). Grandchildren (e.g., `npm exec` → `node mcp-server`) are orphaned.
**Rust behavior:** `setsid` creates a new process group on spawn. `killpg` sends SIGTERM/SIGKILL to the entire group, including all descendants.

### D-101: Max message size enforcement
**TS behavior:** No explicit size limit (SDK's `raw-body` defaults to 4MB for SSE POST path, but no limit on other paths).
**Rust behavior:** Enforces 16MB max per JSON-RPC message. Returns 413 Payload Too Large for oversized messages.

### D-102: Max concurrent sessions enforcement
**TS behavior:** No limit on concurrent sessions. Can exhaust memory/file descriptors.
**Rust behavior:** Default 1024 max sessions. Returns 503 when limit reached.

### D-103: Health readiness gating
**TS behavior:** Some modes return "ok" from health endpoint immediately, before the child process has started or transport is ready.
**Rust behavior:** Returns 503 until child is started AND transport is accepting connections.

### D-104: Health detail endpoint
**TS behavior:** Health endpoint only returns "ok" string.
**Rust behavior:** `?detail=true` query parameter returns JSON with all observability counters (when `--logLevel debug`).

### D-105: `--header` works in WebSocket mode
**TS behavior:** `--header` CLI flag is not passed to the WebSocket gateway function (only SSE and HTTP modes receive it).
**Rust behavior:** `--header` applies to all server modes including WebSocket.

### D-106: Max partial line buffer enforcement
**TS behavior:** No limit on stdio line buffer. Malicious or broken child could exhaust gateway memory.
**Rust behavior:** 64MB max partial buffer. Kill child/session on overflow (protocol violation).

## Behavioral Notes (Same Behavior, Different Implementation)

### B-001: SSE broadcast-to-all
Both TS and Rust broadcast ALL child stdout messages to ALL connected SSE clients. This is intentional — single shared child, multiple clients.

### B-002: POST always returns 202 in SSE mode
Both versions return 202 Accepted for all valid POSTs to the SSE message endpoint, regardless of whether the child processes the message.

### B-003: Session access counter
Both versions use reference-counted session tracking. Increment on request accept, decrement on response terminal state. Timeout only fires when count reaches zero.

### B-004: Client mode exit behavior
Both versions: remote transport close → exit(1). Remote transport error → log only, do not exit. This asymmetry is intentional for pm2/systemd restart semantics.
