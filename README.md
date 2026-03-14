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
| supergateway-rs | **4.9 MB** |
| supergateway (Node) | 65.6 MB |

**13× lower idle memory.**

### Single-client latency (c=1, sequential)

| Request | rs p50 | node p50 |
|---------|--------|----------|
| POST initialize | 131 ms | 97 ms |
| POST notifications/initialized | **1 ms** | 36 ms |
| POST tools/list | 132 ms | 100 ms |
| **Full 3-POST sequence** | **265 ms** | **247 ms** |

For the `notifications/initialized` POST — a pure notification requiring no
response — rs returns 202 immediately (1 ms). Node involves JS overhead (36 ms).

At c=1 the sequences are essentially tied. The small advantage for Node reflects
child process startup time dominating both, with Node's JS runtime being slightly
more optimised for that specific path.

### Throughput under concurrency

| Concurrency | rs req/s | node req/s | rs p50 | node p50 | rs p95 | node p95 |
|-------------|----------|------------|--------|----------|--------|----------|
| c=1  | 3.7 | 3.7 | 270 ms | 266 ms | 289 ms | 293 ms |
| c=5  | **16.8** | 6.3 | 291 ms | 781 ms | 314 ms | 909 ms |
| c=10 | **30.4** | 5.1 | 317 ms | 1930 ms | 359 ms | 2234 ms |
| c=20 | **42.6** | 4.0 | 376 ms | 4446 ms | 515 ms | 4958 ms |

rs scales **linearly** with concurrency. Node serialises concurrent requests
through its event loop, causing p50 latency to balloon from 266 ms at c=1 to
4446 ms at c=20 — a 17× degradation. rs p50 grows only 1.4× over the same range.

At c=10: **rs is 6× faster throughput** with **6× lower latency**.
At c=20: **rs is 10.7× faster throughput** with **12× lower latency**.

### Memory and CPU under load (c=10)

| Implementation | Peak RSS | CPU | Wall time |
|----------------|----------|-----|-----------|
| supergateway-rs | **5.1 MB** | 2.9% of 1 core | **6.3 s** |
| supergateway (Node) | 99.3 MB | 86.3% of 1 core | 77.2 s |

rs uses **19× less memory** and finishes the same workload **12× faster**.
rs is also **30× more CPU-efficient** — Node saturates the event loop while
rs handles all concurrency with native async tasks.

### Summary

| Metric | supergateway-rs | supergateway (Node) | Winner |
|--------|-----------------|---------------------|--------|
| Idle RSS | 4.9 MB | 65.6 MB | **rs 13×** |
| Peak RSS under load (c=10) | 5.1 MB | 99.3 MB | **rs 19×** |
| Throughput c=1 | 3.7 req/s | 3.7 req/s | tie |
| Throughput c=10 | 30.4 req/s | 5.1 req/s | **rs 6×** |
| Throughput c=20 | 42.6 req/s | 4.0 req/s | **rs 10.7×** |
| Latency c=1 p50 | 265 ms | 247 ms | node (1.1×) |
| Latency c=10 p50 | 317 ms | 1930 ms | **rs 6×** |
| Latency c=10 p95 | 359 ms | 2234 ms | **rs 6.2×** |
| Notification POST | 1 ms | 36 ms | **rs 36×** |
| CPU under load (c=10) | 2.9% | 86.3% | **rs 30×** |
| Binary / install | ~5 MB single binary | node + npm deps | **rs** |

For single-client sequential usage the two are equivalent. For any workload with
multiple concurrent clients, rs is substantially superior across every metric.

## Building

```sh
cargo build --release
# Binary at: target/release/supergateway-rs
```

Requires Rust 1.75+. No runtime dependencies.
