# supergateway-rs

High-performance MCP transport bridge written in Rust. Drop-in replacement for
[supergateway](https://github.com/supermaven-inc/supergateway) with full feature
parity and significantly better performance under concurrent load.

## What it does

Bridges MCP (Model Context Protocol) servers across transport types:

| Mode | Input | Output |
|------|-------|--------|
| 1 | stdio | SSE |
| 2 | stdio | Streamable HTTP (stateless) |
| 3 | stdio | Streamable HTTP (stateful) |
| 4 | SSE | stdio |
| 5 | Streamable HTTP | stdio |
| 6 | stdio | WebSocket |

## Usage

```sh
# stdio → SSE
supergateway-rs --stdio "uvx mcp-server-fetch" --outputTransport sse --port 3100

# stdio → Streamable HTTP (stateless)
supergateway-rs --stdio "uvx mcp-server-fetch" --outputTransport streamableHttp --port 3101

# stdio → Streamable HTTP (stateful)
supergateway-rs --stdio "uvx mcp-server-fetch" --outputTransport streamableHttp --stateful --port 3102

# SSE client → stdio
supergateway-rs --sse http://remote-host/sse

# Streamable HTTP client → stdio
supergateway-rs --streamableHttp http://remote-host/mcp

# With health endpoint, CORS, custom headers
supergateway-rs --stdio "cmd" --outputTransport sse --port 3100 \
  --healthEndpoint /healthz \
  --cors \
  --header "Authorization: Bearer token" \
  --oauth2Bearer mytoken
```

All flags from the original `supergateway` are supported: `--port`, `--baseUrl`,
`--ssePath`, `--messagePath`, `--streamableHttpPath`, `--logLevel`, `--cors`,
`--healthEndpoint`, `--header`, `--oauth2Bearer`, `--stateful`, `--sessionTimeout`,
`--protocolVersion`.

## Performance

Benchmarked against `supergateway` (Node.js v22) using a minimal Python MCP server
(`python3 mcp-fast-server.py`) to isolate gateway overhead. All tests run the full
MCP handshake: `initialize` → `notifications/initialized` → `tools/list`.

Environment: WSL1 on Linux 4.4, release build vs `node dist/index.js`.

### Idle memory

| Implementation | Idle RSS |
|----------------|----------|
| supergateway-rs | **4.7 MB** |
| supergateway (Node) | 67.3 MB |

**14× lower idle memory.**

### Single-client latency (c=1, sequential)

| Request | rs p50 | node p50 |
|---------|--------|----------|
| POST initialize | 131 ms | 104 ms |
| POST notifications/initialized | **1 ms** | 36 ms |
| POST tools/list | 132 ms | 103 ms |
| **Full 3-POST sequence** | **268 ms** | **242 ms** |

For the `notifications/initialized` POST — a pure notification requiring no
response — rs returns 202 immediately without spawning a child process (1 ms).
The Node SDK also returns 202 quickly but still involves some JS overhead (36 ms).

At c=1 the sequences are essentially tied (268 ms vs 242 ms). The small advantage
for Node reflects its event-loop being more efficient than rs's blocking-thread
model for strictly sequential single-client traffic.

### Throughput under concurrency

| Concurrency | rs req/s | node req/s | rs p50 | node p50 |
|-------------|----------|------------|--------|----------|
| c=1  | 3.7 | 3.6 | 269 ms | 273 ms |
| c=5  | **17.3** | 6.4 | 286 ms | 752 ms |
| c=10 | **31.2** | 5.0 | 310 ms | 1957 ms |
| c=20 | **42.0** | 3.9 | 396 ms | 4759 ms |

rs scales **linearly** with concurrency. Node serialises concurrent requests
through its event loop, causing p50 latency to balloon from 273 ms at c=1 to
4759 ms at c=20 — an 17× degradation. rs p50 grows only 1.5× over the same range.

At c=10: **rs is 6× faster throughput** with **6× lower latency**.
At c=20: **rs is 11× faster throughput** with **12× lower latency**.

### Memory and CPU under load (c=10, 200 requests)

| Implementation | Peak RSS | CPU | Wall time |
|----------------|----------|-----|-----------|
| supergateway-rs | **9.1 MB** | 89% of 1 core | **6.7 s** |
| supergateway (Node) | 103.0 MB | 83% of 1 core | 67.9 s |

rs uses **11× less memory** and finishes the same workload **10× faster**.
Both implementations are CPU-bound at roughly the same fraction of a single core —
rs just does far more work per clock cycle.

### Summary

| Metric | supergateway-rs | supergateway (Node) | Winner |
|--------|-----------------|---------------------|--------|
| Idle RSS | 4.7 MB | 67.3 MB | **rs 14×** |
| Peak RSS under load | 9.1 MB | 103.0 MB | **rs 11×** |
| Throughput c=1 | 3.7 req/s | 3.6 req/s | tie |
| Throughput c=10 | 31.2 req/s | 5.0 req/s | **rs 6×** |
| Throughput c=20 | 42.0 req/s | 3.9 req/s | **rs 11×** |
| Latency c=1 p50 | 268 ms | 242 ms | node (1.1×) |
| Latency c=10 p50 | 310 ms | 1957 ms | **rs 6×** |
| Notification POST | 1 ms | 36 ms | **rs 36×** |
| Binary / install | 4.2 MB single binary | node + npm deps | **rs** |

For single-client sequential usage the two are equivalent. For any workload with
multiple concurrent clients, rs is substantially superior across every metric.

## Building

```sh
cargo build --release
# Binary at: target/release/supergateway-rs
```

Requires Rust 1.75+. No runtime dependencies.
