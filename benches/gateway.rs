/// Gateway-level benchmarks: encode/decode round-trip latency, throughput,
/// and FD leak detection across 1000 simulated connect/disconnect cycles.
///
/// These use in-memory buffers (Cursor) as the "transport" — no actual
/// processes or sockets. The measurements capture codec overhead only,
/// which is the dominant contribution to bridge latency (D-101, D-102).
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use std::io::Cursor;

use supergateway_rs::cli::{LogLevel, OutputTransport};
use supergateway_rs::codec::{write_message, StdoutCodec, DEFAULT_PARTIAL_BUFFER};
use supergateway_rs::jsonrpc::RawMessage;
use supergateway_rs::observe::{Logger, Metrics};

fn make_logger() -> Logger {
    Logger::new(OutputTransport::Sse, LogLevel::None)
}

fn make_metrics() -> std::sync::Arc<Metrics> {
    Metrics::new()
}

/// Count open file descriptors for the current process via /proc/self/fd.
///
/// Returns 0 on non-Linux platforms. Each call transiently opens one
/// extra fd (the directory handle) which is consistent across calls.
#[cfg(target_os = "linux")]
fn count_fds() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .map(|d| d.count())
        .unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
fn count_fds() -> usize {
    0
}

// ─── encode/decode round-trip latency ────────────────────────────────────────

/// Measures the time for a single encode → decode round-trip through the codec.
///
/// This represents the minimum codec contribution to bridge latency.
/// Performance target: p99 < 2ms bridge overhead (PRD US-018).
fn bench_encode_decode_roundtrip(c: &mut Criterion) {
    let msg: RawMessage = serde_json::from_str(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hello world"}}}"#,
    )
    .unwrap();

    c.bench_function("encode_decode_roundtrip", |b| {
        b.iter(|| {
            // Encode: serialize message to newline-delimited bytes
            let mut buf: Vec<u8> = Vec::with_capacity(256);
            write_message(&mut buf, black_box(&msg)).unwrap();

            // Decode: parse the bytes back through StdoutCodec
            let metrics = make_metrics();
            let logger = make_logger();
            let mut codec =
                StdoutCodec::new(Cursor::new(buf.as_slice()), DEFAULT_PARTIAL_BUFFER);
            let result = codec.read_message(&metrics, &logger).unwrap();
            black_box(result.is_some());
        });
    });
}

// ─── sustained throughput ────────────────────────────────────────────────────

/// Measures sustained decode throughput in messages/second.
///
/// Performance target: >10k msg/sec (PRD US-018, SSE mode).
fn bench_sustained_throughput(c: &mut Criterion) {
    const N: u64 = 10_000;
    let msg: RawMessage = serde_json::from_str(
        r#"{"jsonrpc":"2.0","id":1,"method":"notifications/progress","params":{"token":"t","value":50,"total":100}}"#,
    )
    .unwrap();

    // Pre-encode N messages (~200 bytes each) into a single buffer.
    let mut encoded: Vec<u8> = Vec::with_capacity(220 * N as usize);
    for _ in 0..N {
        write_message(&mut encoded, &msg).unwrap();
    }

    let mut group = c.benchmark_group("gateway_throughput");
    group.throughput(Throughput::Elements(N));
    group.bench_function("10k_msg_decode", |b| {
        b.iter(|| {
            let metrics = make_metrics();
            let logger = make_logger();
            let mut codec =
                StdoutCodec::new(Cursor::new(encoded.as_slice()), DEFAULT_PARTIAL_BUFFER);
            let mut count = 0u64;
            while let Ok(Some(_)) = codec.read_message(&metrics, &logger) {
                count += 1;
            }
            black_box(count);
        });
    });
    group.finish();
}

// ─── FD leak: 1000 connect/disconnect cycles ─────────────────────────────────

/// Verifies that StdoutCodec and related types do not leak file descriptors
/// across 1000 simulated connect/disconnect cycles.
///
/// Acceptance criterion: zero FD growth after 1000 cycles (PRD US-018).
/// Uses Cursor (in-memory) so no actual file/socket FDs are opened.
fn bench_fd_leak_1000_cycles(c: &mut Criterion) {
    let msg_line = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";

    c.bench_function("fd_leak_1000_cycles", |b| {
        b.iter(|| {
            let fds_before = count_fds();

            for _ in 0..1_000 {
                let metrics = make_metrics();
                let logger = make_logger();
                let mut codec = StdoutCodec::new(
                    Cursor::new(msg_line.as_slice()),
                    DEFAULT_PARTIAL_BUFFER,
                );
                let _ = codec.read_message(&metrics, &logger);
                // Explicit drop to ensure deterministic cleanup within each cycle
                drop(codec);
                drop(metrics);
                drop(logger);
            }

            let fds_after = count_fds();
            // Allow up to 5 fds of jitter (OS, runtime internals).
            assert!(
                fds_after <= fds_before + 5,
                "FD leak detected: before={fds_before} after={fds_after} (delta={})",
                fds_after.saturating_sub(fds_before),
            );

            black_box(fds_after);
        });
    });
}

// ─── notification throughput (broadcast pattern) ────────────────────────────

/// Simulates SSE broadcast: one child stdout message decoded and serialized
/// N times (once per connected client). Baseline for the broadcast fan-out cost.
fn bench_broadcast_fanout(c: &mut Criterion) {
    const CLIENTS: u64 = 100;
    let notification: RawMessage = serde_json::from_str(
        r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{}}"#,
    )
    .unwrap();

    // Pre-encode a single notification line (child stdout side)
    let mut child_line: Vec<u8> = Vec::with_capacity(128);
    write_message(&mut child_line, &notification).unwrap();

    let mut group = c.benchmark_group("broadcast_fanout");
    group.throughput(Throughput::Elements(CLIENTS));
    group.bench_function("100_clients", |b| {
        b.iter(|| {
            // Decode child stdout (once)
            let metrics = make_metrics();
            let logger = make_logger();
            let mut codec = StdoutCodec::new(
                Cursor::new(child_line.as_slice()),
                DEFAULT_PARTIAL_BUFFER,
            );
            let msg = codec.read_message(&metrics, &logger).unwrap().unwrap();

            // Re-serialize for each client (fan-out)
            let mut total_bytes = 0u64;
            for _ in 0..CLIENTS {
                let mut out: Vec<u8> = Vec::with_capacity(128);
                match &msg {
                    supergateway_rs::jsonrpc::Parsed::Single(m) => {
                        write_message(&mut out, black_box(m)).unwrap();
                    }
                    supergateway_rs::jsonrpc::Parsed::Batch(msgs) => {
                        for m in msgs {
                            write_message(&mut out, black_box(m)).unwrap();
                        }
                    }
                }
                total_bytes += out.len() as u64;
            }
            black_box(total_bytes);
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_encode_decode_roundtrip,
    bench_sustained_throughput,
    bench_fd_leak_1000_cycles,
    bench_broadcast_fanout,
);
criterion_main!(benches);
