# supergateway-rs

## Project Goal

**Rewrite [supergateway](https://github.com/nicepkg/supergateway) in Rust for ultimate performance and full feature parity.**

supergateway is an MCP (Model Context Protocol) transport bridge that converts between stdio, SSE, Streamable HTTP, and WebSocket transports. The TypeScript version runs 5 instances under pm2 consuming ~65MB RSS each. The Rust version targets <3MB RSS idle, <2ms p99 bridge latency, >10k msg/sec throughput.

## Architecture

- **Runtime**: [Asupersync](https://github.com/nicepkg/asupersync) (NOT Tokio). Pre-1.0 async runtime with native HTTP, SSE, WebSocket stacks and structured concurrency via Regions
- **MCP handling**: From scratch using JSON-RPC 2.0 with `serde_json::RawValue` for opaque pass-through. No `rmcp` crate (Tokio-dependent)
- **Child processes**: `setsid` + `killpg` for clean process group management (fixes TS bug of orphaned grandchildren)
- **Nightly Rust**: Required for Asupersync. Pin exact version in `rust-toolchain.toml`

## Key Principles

1. **Protocol transparency**: Forward all JSON-RPC payloads as opaque values. Never reject unknown methods or arbitrary params/result shapes. Use `RawValue` everywhere
2. **Structured concurrency**: Every child process and session lives inside a Region. Region cancellation cascades cleanup automatically
3. **Zero leaks**: No leaked children, FDs, or sessions on any code path (normal, error, timeout, disconnect, signal)
4. **Drop-in replacement**: Same CLI flags, same observable behavior, same pm2 config (just swap the binary path)
5. **Correctness over cleverness**: Match TypeScript behavior exactly unless there's a documented intentional improvement

## Transport Modes

1. `stdio->SSE` — single shared child, broadcast to all SSE clients
2. `stdio->WebSocket` — single shared child, per-client ID routing via HashMap multiplexer
3. `stdio->Streamable HTTP (stateful)` — per-session child, session management with access counting
4. `stdio->Streamable HTTP (stateless)` — per-request child, auto-initialization
5. `SSE->stdio` — client mode, bridge stdin to remote SSE server
6. `Streamable HTTP->stdio` — client mode, bridge stdin to remote HTTP server

## Development Workflow

- Read `PRD.md` for complete requirements, user stories, and acceptance criteria
- User stories are numbered US-000 through US-019 with explicit dependencies
- US-000 (Asupersync validation spike) is the GO/NO-GO gate — must pass before proceeding
- `DIVERGENCES.md` documents intentional behavioral differences from TypeScript version
- Compatibility harness (US-001) runs same tests against both TS and Rust binaries

## Build & Test

```bash
# Build
cargo +nightly build --release

# Test
cargo +nightly test

# Lint
cargo +nightly clippy -- -D warnings
```

## File Structure

```
src/
  cli.rs          -- clap CLI parsing
  jsonrpc.rs      -- JSON-RPC 2.0 types (RawValue pass-through)
  codec.rs        -- Line-delimited stdio codec
  child.rs        -- ChildBridge (spawn, kill, EPIPE detection)
  session.rs      -- Session manager (state machine, access counting)
  cors.rs         -- CORS middleware
  mcp_init.rs     -- Auto-init state machine (stateless mode)
  observe.rs      -- Observability counters
  health.rs       -- Health/readiness endpoints
  client/
    http.rs       -- HTTP/1.1 client
    sse.rs        -- SSE client (EventSource)
  gateway/
    stdio_to_sse.rs
    stdio_to_ws.rs
    stdio_to_stateful_http.rs
    stdio_to_stateless_http.rs
    sse_to_stdio.rs
    http_to_stdio.rs
  main.rs
```
