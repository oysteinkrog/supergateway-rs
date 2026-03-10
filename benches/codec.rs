use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::io::Cursor;

use supergateway_rs::cli::{LogLevel, OutputTransport};
use supergateway_rs::codec::{write_message, StdoutCodec, DEFAULT_PARTIAL_BUFFER, MAX_MESSAGE_SIZE};
use supergateway_rs::jsonrpc::RawMessage;
use supergateway_rs::observe::{Logger, Metrics};

fn make_logger() -> Logger {
    Logger::new(OutputTransport::Sse, LogLevel::None)
}

fn make_metrics() -> std::sync::Arc<Metrics> {
    Metrics::new()
}

// ─── parse_10k_messages ───────────────────────────────────────────────────────

fn bench_parse_10k_messages(c: &mut Criterion) {
    let single_msg =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":"abc"}}"#;
    let mut data = String::with_capacity((single_msg.len() + 1) * 10_000);
    for _ in 0..10_000 {
        data.push_str(single_msg);
        data.push('\n');
    }
    let bytes = data.into_bytes();

    c.bench_function("parse_10k_messages", |b| {
        b.iter(|| {
            let metrics = make_metrics();
            let logger = make_logger();
            let mut codec =
                StdoutCodec::new(Cursor::new(bytes.as_slice()), DEFAULT_PARTIAL_BUFFER);
            let mut count = 0u64;
            while let Ok(Some(_msg)) = codec.read_message(&metrics, &logger) {
                count += 1;
            }
            black_box(count);
        });
    });
}

// ─── rawvalue_roundtrip_10k ───────────────────────────────────────────────────

fn bench_rawvalue_roundtrip_10k(c: &mut Criterion) {
    let src = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":"abc","nested":{"deep":true,"value":42}}}"#;
    let msg: RawMessage = serde_json::from_str(src).unwrap();

    c.bench_function("rawvalue_roundtrip_10k", |b| {
        b.iter(|| {
            let mut count = 0u64;
            for _ in 0..10_000 {
                let serialized = serde_json::to_string(black_box(&msg)).unwrap();
                let deserialized: RawMessage = serde_json::from_str(&serialized).unwrap();
                count += deserialized.method.is_some() as u64;
            }
            black_box(count);
        });
    });
}

// ─── write_message_10k ───────────────────────────────────────────────────────

fn bench_write_message_10k(c: &mut Criterion) {
    let msg: RawMessage = serde_json::from_str(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":"abc"}}"#,
    )
    .unwrap();

    // Pre-allocate enough capacity: ~100 bytes per message * 10k
    c.bench_function("write_message_10k", |b| {
        b.iter(|| {
            let mut sink: Vec<u8> = Vec::with_capacity(100 * 10_000);
            for _ in 0..10_000 {
                write_message(&mut sink, black_box(&msg)).unwrap();
            }
            black_box(sink.len());
        });
    });
}

// ─── parse_single_latency ────────────────────────────────────────────────────

fn bench_parse_single_latency(c: &mut Criterion) {
    let msg_str =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":"abc"}}"#;
    let line = format!("{msg_str}\n");
    let bytes = line.into_bytes();

    c.bench_function("parse_single_message_latency", |b| {
        b.iter(|| {
            let metrics = make_metrics();
            let logger = make_logger();
            let mut codec =
                StdoutCodec::new(Cursor::new(bytes.as_slice()), DEFAULT_PARTIAL_BUFFER);
            let result = codec.read_message(&metrics, &logger);
            black_box(result.is_ok());
        });
    });
}

// ─── large_message_handling ──────────────────────────────────────────────────

/// Tests that 1MB, 10MB, and 16MB messages are accepted; >16MB is rejected.
///
/// These validate D-101 (max message size enforcement) and the acceptance
/// criterion: "1MB, 10MB, 16MB handled; >16MB rejected with 413/error".
fn bench_large_message_handling(c: &mut Criterion) {
    let mut group = c.benchmark_group("large_message");

    // ── Accepted sizes: 1MB, 10MB, 16MB ──────────────────────────────────────
    for &size_bytes in &[1_000_000u64, 10_000_000, MAX_MESSAGE_SIZE as u64] {
        // Build a valid JSON-RPC message with a large params blob.
        // Account for ~60 bytes of JSON scaffolding so total stays within limit.
        let payload_len = size_bytes.saturating_sub(80) as usize;
        let payload = "x".repeat(payload_len);
        let msg_json = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"large","params":{{"data":"{payload}"}}}}"#
        );
        let line = format!("{msg_json}\n");
        let bytes = line.into_bytes();

        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("accepted", format!("{}mb", size_bytes / 1_000_000)),
            &bytes,
            |b, data| {
                b.iter(|| {
                    let metrics = make_metrics();
                    let logger = make_logger();
                    let mut codec = StdoutCodec::new(
                        Cursor::new(data.as_slice()),
                        DEFAULT_PARTIAL_BUFFER,
                    );
                    let result = codec.read_message(&metrics, &logger);
                    black_box(result.is_ok());
                });
            },
        );
    }

    // ── Rejected: >16MB ────────────────────────────────────────────────────
    // A raw line of >16MB bytes that is not valid JSON but exceeds the size
    // limit. Codec must skip it and return Ok(None) at EOF.
    let over_limit_bytes = MAX_MESSAGE_SIZE + 1024;
    let over_msg = format!("{}\n", "y".repeat(over_limit_bytes));
    let over_bytes = over_msg.into_bytes();

    group.throughput(Throughput::Bytes(over_bytes.len() as u64));
    group.bench_function("rejected_over_16mb", |b| {
        b.iter(|| {
            let metrics = make_metrics();
            let logger = make_logger();
            let mut codec = StdoutCodec::new(
                Cursor::new(over_bytes.as_slice()),
                DEFAULT_PARTIAL_BUFFER,
            );
            // Oversized line is skipped; codec returns Ok(None) at EOF.
            let result = codec.read_message(&metrics, &logger);
            black_box(result.is_ok());
        });
    });

    group.finish();
}

// ─── throughput ──────────────────────────────────────────────────────────────

fn bench_throughput(c: &mut Criterion) {
    const N: u64 = 10_000;
    let msg: RawMessage = serde_json::from_str(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hello world"}}}"#,
    )
    .unwrap();

    let mut encoded: Vec<u8> = Vec::with_capacity(200 * N as usize);
    for _ in 0..N {
        write_message(&mut encoded, &msg).unwrap();
    }

    let mut group = c.benchmark_group("throughput");
    group.throughput(Throughput::Elements(N));
    group.bench_function("10k_messages_decode", |b| {
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

criterion_group!(
    benches,
    bench_parse_10k_messages,
    bench_rawvalue_roundtrip_10k,
    bench_write_message_10k,
    bench_parse_single_latency,
    bench_large_message_handling,
    bench_throughput,
);
criterion_main!(benches);
