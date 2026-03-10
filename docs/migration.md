# Migrating from TypeScript supergateway to supergateway-rs

## Why switch

| Metric | TypeScript | Rust |
|--------|-----------|------|
| RSS per instance (idle) | ~65 MB | <3 MB |
| RSS under pm2 (5 instances) | ~325 MB | <15 MB |
| Bridge latency (p99) | ~10 ms | <2 ms |
| Throughput | ~1k msg/sec | >10k msg/sec |
| Orphaned grandchildren | Yes (child.kill() only kills sh) | No (setsid + killpg kills entire process group) |
| Binary size | N/A (Node.js + npm deps) | ~5 MB static binary |

supergateway-rs is a drop-in replacement. Same CLI flags, same observable behavior, same pm2 config. Swap the binary path and restart.

## CLI flag compatibility

All flags use camelCase, identical to the TypeScript version:

| Flag | Default | Description |
|------|---------|-------------|
| `--stdio <cmd>` | | Spawn a child process as MCP stdio server |
| `--sse <url>` | | Connect to remote MCP SSE server |
| `--streamableHttp <url>` | | Connect to remote MCP Streamable HTTP server |
| `--outputTransport <t>` | sse (stdio input), stdio (client input) | Output transport: stdio, sse, ws, streamableHttp |
| `--port <n>` | 8000 | Server port |
| `--baseUrl <path>` | (empty) | URL prefix for all endpoints |
| `--ssePath <path>` | /sse | SSE event stream path |
| `--messagePath <path>` | /message | Message POST path |
| `--streamableHttpPath <path>` | /mcp | Streamable HTTP endpoint path |
| `--logLevel <level>` | info | Log level: debug, info, none |
| `--cors [origins...]` | (disabled) | CORS: absent=disabled, no values=wildcard, with values=specific |
| `--healthEndpoint <path>...` | (none) | Health check endpoint paths |
| `--header "Name: Value"` | (none) | Custom headers (repeatable) |
| `--oauth2Bearer <token>` | (none) | OAuth2 Bearer token (adds Authorization header) |
| `--stateful` | false | Use stateful sessions (stdio->streamableHttp only) |
| `--sessionTimeout <ms>` | (none) | Session inactivity timeout in milliseconds |
| `--protocolVersion <ver>` | 2024-11-05 | MCP protocol version for stateless auto-init |

Exactly one input is required: `--stdio`, `--sse`, or `--streamableHttp`. They are mutually exclusive.

## Intentional divergences

supergateway-rs fixes several bugs and adds safety improvements documented in `DIVERGENCES.md`. Key changes that may affect migration:

- **D-001**: WebSocket string/colon IDs now work correctly (TS mangles them with parseInt)
- **D-003**: `--oauth2Bearer` only sends the header when a token is actually provided (TS sends `Bearer undefined`)
- **D-015**: Stateful HTTP returns 404 for unknown session IDs (TS returns 400 for all invalid session cases)
- **D-100**: Process group cleanup kills all descendants, not just the direct child
- **D-101**: 16 MB max message size enforced (TS has no limit)
- **D-102**: 1024 max concurrent sessions enforced (TS has no limit)
- **D-018**: Health endpoint returns `Content-Type: text/plain` (TS returns `text/html`)

None of these should break well-behaved clients. If your system depends on any TS-specific behavior (e.g., parsing the `Bearer undefined` header, or relying on 400 vs 404 status codes), review `DIVERGENCES.md` before switching.

## pm2 migration (step-by-step)

### Prerequisites

1. Build the Rust binary:
   ```bash
   cargo build --release
   # Binary at: ./target/release/supergateway-rs
   ```

2. Verify the binary runs:
   ```bash
   ./target/release/supergateway-rs --stdio "echo hello" --port 9999
   # Should print startup banner and listen on port 9999
   ```

### Step 1: Update ecosystem.config.js

Change the `script` path from the TS entry point to the Rust binary. Everything else stays the same.

Before (TypeScript):
```js
{
  name: 'mcp-fetch',
  script: 'npx',
  args: '-y supergateway --stdio "uvx mcp-server-fetch" --port 3000',
}
```

After (Rust):
```js
{
  name: 'mcp-fetch',
  script: './target/release/supergateway-rs',
  args: '--stdio "uvx mcp-server-fetch" --port 3000',
}
```

No `npx` wrapper needed. The Rust binary is a single executable.

### Step 2: Restart

```bash
pm2 reload ecosystem.config.js
# or for zero-downtime on a specific app:
pm2 reload mcp-fetch
```

### Step 3: Verify

```bash
# Check all instances are online
pm2 list

# Check memory (should be <5 MB per instance under pm2)
pm2 monit

# Check logs for startup banner
pm2 logs mcp-fetch --lines 5
# Should show: [supergateway] supergateway vX.X.X | ...

# Test health endpoint (if configured)
curl http://localhost:3000/healthz
```

## Canary deployment

For production systems, run Rust alongside TypeScript before cutting over.

### Step 1: Deploy Rust on alternate port

Add a parallel entry in `ecosystem.config.js`:

```js
{
  name: 'mcp-fetch-canary',
  script: './target/release/supergateway-rs',
  args: '--stdio "uvx mcp-server-fetch" --port 3001 --healthEndpoint /healthz',
  instances: 1,
}
```

```bash
pm2 start ecosystem.config.js --only mcp-fetch-canary
```

### Step 2: Route canary traffic

Point a subset of clients (or a test client) at port 3001. Compare:

- Response correctness (same JSON-RPC responses)
- Latency (should be lower)
- Memory usage (`pm2 monit` or `ps -o rss -p $(pm2 pid mcp-fetch-canary)`)
- Error rate in logs (`pm2 logs mcp-fetch-canary`)

### Step 3: 24h operational run

Run the canary under realistic load for 24 hours. Monitor:

- RSS stays stable (no memory leaks)
- No orphaned child processes (`ps --forest`)
- Health endpoint returns `ok` continuously
- SIGTERM handling: `pm2 reload mcp-fetch-canary` completes cleanly
- Log output is well-formed

Record results here before proceeding to full cutover.

### Step 4: Cut over

```bash
# Stop canary
pm2 delete mcp-fetch-canary

# Update main entry to Rust binary (see Step 1 above)
# Then:
pm2 reload ecosystem.config.js
```

### Step 5: Monitor

Watch for 1 hour after cutover:
```bash
pm2 monit
pm2 logs --lines 50
```

## Rollback

If something goes wrong, revert `ecosystem.config.js` to the TypeScript version:

```bash
# Revert config
git checkout ecosystem.config.js
# or manually change script back to 'npx' + supergateway

# Reload
pm2 reload ecosystem.config.js

# Verify
pm2 list
pm2 logs --lines 10
```

The rollback is instant because pm2 manages the process lifecycle. No data migration, no state to clean up.

## Verifying correct operation

### Health endpoint

Configure with `--healthEndpoint /healthz`:
```bash
curl -s http://localhost:3000/healthz
# Returns: ok

# Detailed metrics (requires --logLevel debug):
curl -s 'http://localhost:3000/healthz?detail=true'
# Returns JSON with counters
```

The health endpoint returns 503 until the child process is started and the transport is accepting connections (D-103). Use this for load balancer health checks.

### Log format

Logs are prefixed with `[supergateway]`:
```
[supergateway] supergateway v0.1.0 | uvx mcp-server-fetch | sse | port 3000
[supergateway] signal handlers installed (SIGINT, SIGTERM, SIGHUP)
[supergateway] child spawned pid=12345
[supergateway] active_clients=1 (+1)
```

When `--outputTransport stdio` (client modes), all logs go to stderr to keep stdout clean for JSON-RPC.

### Signal handling

supergateway-rs handles SIGINT, SIGTERM, and SIGHUP:

1. First signal: graceful shutdown (drain in-flight requests, 5s budget)
2. Second signal: force exit immediately
3. Stdin EOF: triggers graceful shutdown (for IDE hosts that close stdin)

pm2 sends SIGINT by default on `pm2 stop/reload`. The gateway will:
- Stop accepting new connections
- Drain in-flight requests (up to 5s)
- Kill child process group (SIGTERM, then SIGKILL after timeout)
- Exit with code 0

### Reverse proxy (nginx)

If running behind nginx, ensure SSE streams are not buffered:

```nginx
location /sse {
    proxy_pass http://127.0.0.1:3000;
    proxy_http_version 1.1;
    proxy_set_header Connection '';
    proxy_buffering off;
    proxy_cache off;
    # supergateway-rs sends X-Accel-Buffering: no on SSE responses
}

location /ws {
    proxy_pass http://127.0.0.1:3000;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
}
```

This applies to both the TypeScript and Rust versions. No nginx config changes needed when switching.
