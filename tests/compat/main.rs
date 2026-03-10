//! Compatibility test suite for supergateway-rs.
//!
//! Tests all 6 gateway modes against mock MCP servers.
//! Each test spawns the gateway binary with the appropriate flags
//! and verifies behavior matches the TypeScript reference (or
//! documents intentional divergences per DIVERGENCES.md).
//!
//! Usage:
//!   cargo test --test compat -- --nocapture
//!
//! Environment:
//!   SUPERGATEWAY_BINARY — path to gateway binary (default: cargo-built)
//!   MOCK_STDIO_SERVER   — path to mock stdio MCP server
//!   MOCK_SSE_SERVER     — path to mock SSE MCP server
//!   MOCK_HTTP_SERVER    — path to mock HTTP MCP server

mod helpers;
mod http_to_stdio;
mod signal;
mod sse_to_stdio;
mod stdio_to_sse;
mod stdio_to_stateful_http;
mod stdio_to_stateless_http;
mod stdio_to_ws;
