/// Comprehensive benchmark suite for supergateway-rs.
///
/// Covers three workload families:
///   1. codec  — parse/emit latency for individual messages
///   2. gateway — encode/decode round-trip at varying concurrency
///   3. e2e    — simulated end-to-end fan-out with c=1,5,10,20 virtual clients
///
/// After the bench run a JSON v3 summary is written atomically to:
///   .bench-history/comprehensive_bench.latest.json
use criterion::{
    black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput,
};
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

// ─── codec family ────────────────────────────────────────────────────────────

fn bench_codec_parse_single(c: &mut Criterion) {
    let msg_str =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":"abc"}}"#;
    let line = format!("{msg_str}\n");
    let bytes = line.into_bytes();

    let metrics = make_metrics();
    let logger = make_logger();
    let mut group = c.benchmark_group("codec");
    group.bench_function("parse_single", |b| {
        b.iter(|| {
            let mut codec =
                StdoutCodec::new(Cursor::new(bytes.as_slice()), DEFAULT_PARTIAL_BUFFER);
            let result = codec.read_message(&metrics, &logger);
            black_box(result.is_ok());
        });
    });
    group.finish();
}

fn bench_codec_emit_single(c: &mut Criterion) {
    let msg: RawMessage = serde_json::from_str(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":"abc"}}"#,
    )
    .unwrap();

    let mut group = c.benchmark_group("codec");
    group.bench_function("emit_single", |b| {
        b.iter(|| {
            let mut buf: Vec<u8> = Vec::with_capacity(128);
            write_message(&mut buf, black_box(&msg)).unwrap();
            black_box(buf.len());
        });
    });
    group.finish();
}

fn bench_codec_roundtrip(c: &mut Criterion) {
    let msg: RawMessage = serde_json::from_str(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hello world"}}}"#,
    )
    .unwrap();

    let metrics = make_metrics();
    let logger = make_logger();
    let mut group = c.benchmark_group("codec");
    group.bench_function("roundtrip", |b| {
        b.iter(|| {
            let mut buf: Vec<u8> = Vec::with_capacity(256);
            write_message(&mut buf, black_box(&msg)).unwrap();
            let mut codec =
                StdoutCodec::new(Cursor::new(buf.as_slice()), DEFAULT_PARTIAL_BUFFER);
            let result = codec.read_message(&metrics, &logger).unwrap();
            black_box(result.is_some());
        });
    });
    group.finish();
}

// ─── gateway family ──────────────────────────────────────────────────────────

fn bench_gateway_throughput(c: &mut Criterion) {
    const N: u64 = 10_000;
    let msg: RawMessage = serde_json::from_str(
        r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"token":"t","value":50,"total":100}}"#,
    )
    .unwrap();

    let mut encoded: Vec<u8> = Vec::with_capacity(220 * N as usize);
    for _ in 0..N {
        write_message(&mut encoded, &msg).unwrap();
    }

    let metrics = make_metrics();
    let logger = make_logger();
    let mut group = c.benchmark_group("gateway");
    group.throughput(Throughput::Elements(N));
    group.bench_function("decode_10k", |b| {
        b.iter(|| {
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

fn bench_gateway_broadcast_fanout(c: &mut Criterion) {
    let notification: RawMessage = serde_json::from_str(
        r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{}}"#,
    )
    .unwrap();

    let mut child_line: Vec<u8> = Vec::with_capacity(128);
    write_message(&mut child_line, &notification).unwrap();

    let metrics = make_metrics();
    let logger = make_logger();
    let mut group = c.benchmark_group("gateway");

    // Benchmark broadcast fan-out with increasing concurrency levels
    for &clients in &[1u64, 5, 10, 20] {
        group.throughput(Throughput::Elements(clients));
        group.bench_with_input(
            BenchmarkId::new("broadcast_fanout", format!("c{clients}")),
            &clients,
            |b, &n_clients| {
                b.iter(|| {
                    let mut codec = StdoutCodec::new(
                        Cursor::new(child_line.as_slice()),
                        DEFAULT_PARTIAL_BUFFER,
                    );
                    let msg = codec.read_message(&metrics, &logger).unwrap().unwrap();
                    let mut total_bytes = 0u64;
                    for _ in 0..n_clients {
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
            },
        );
    }
    group.finish();
}

// ─── e2e family ──────────────────────────────────────────────────────────────

/// Simulated end-to-end: encode a request (client→server), decode it, encode a
/// response (server→client), decode it. Repeated for c=1,5,10,20 concurrent
/// virtual clients (sequential simulation — no actual threads; measures raw
/// codec overhead at varying fan-out scale).
fn bench_e2e_roundtrip(c: &mut Criterion) {
    let request: RawMessage = serde_json::from_str(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hello"}}}"#,
    )
    .unwrap();
    let response: RawMessage = serde_json::from_str(
        r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"hello"}]}}"#,
    )
    .unwrap();

    let metrics = make_metrics();
    let logger = make_logger();
    let mut group = c.benchmark_group("e2e");

    for &concurrency in &[1u64, 5, 10, 20] {
        group.throughput(Throughput::Elements(concurrency));
        group.bench_with_input(
            BenchmarkId::new("roundtrip", format!("c{concurrency}")),
            &concurrency,
            |b, &c_level| {
                b.iter(|| {
                    let mut total = 0u64;
                    for _ in 0..c_level {
                        // Encode request
                        let mut req_buf: Vec<u8> = Vec::with_capacity(256);
                        write_message(&mut req_buf, black_box(&request)).unwrap();

                        // Decode request (simulate server-side stdin read)
                        let mut req_codec = StdoutCodec::new(
                            Cursor::new(req_buf.as_slice()),
                            DEFAULT_PARTIAL_BUFFER,
                        );
                        let _ = req_codec.read_message(&metrics, &logger).unwrap();

                        // Encode response
                        let mut resp_buf: Vec<u8> = Vec::with_capacity(256);
                        write_message(&mut resp_buf, black_box(&response)).unwrap();

                        // Decode response (simulate client-side SSE decode)
                        let mut resp_codec = StdoutCodec::new(
                            Cursor::new(resp_buf.as_slice()),
                            DEFAULT_PARTIAL_BUFFER,
                        );
                        let r = resp_codec.read_message(&metrics, &logger).unwrap();
                        total += r.is_some() as u64;
                    }
                    black_box(total);
                });
            },
        );
    }
    group.finish();
}

// ─── JSON v3 emitter ─────────────────────────────────────────────────────────

mod json_v3 {
    use serde::Serialize;
    use std::fs;
    use std::io::Write;
    use std::path::Path;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Serialize)]
    pub struct EngineIdentity {
        pub rust_toolchain: String,
        pub supergateway_version: String,
        pub profile: String,
    }

    #[derive(Serialize)]
    pub struct BenchResult {
        pub family: String,
        pub workload: String,
        pub concurrency: u32,
        pub mean_ns: f64,
        pub std_dev_ns: f64,
        pub cv_pct: f64,
        pub samples: usize,
    }

    #[derive(Serialize)]
    pub struct BenchReport {
        pub schema_version: String,
        pub bench_name: String,
        pub git_sha: String,
        pub engine_identity: EngineIdentity,
        pub host_id: String,
        pub profile_name: String,
        pub timestamp_unix: i64,
        pub results: Vec<BenchResult>,
    }

    fn git_sha() -> String {
        Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .unwrap_or_else(|| "unknown".to_owned())
    }

    fn hostname() -> String {
        std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| {
                Command::new("hostname")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_owned())
                    .unwrap_or_else(|| "unknown".to_owned())
            })
    }

    /// Read estimates.json from target/criterion for a given bench path.
    /// Returns (mean_ns, std_dev_ns) or None if file is missing.
    fn read_estimates(criterion_dir: &Path, bench_path: &str) -> Option<(f64, f64)> {
        let estimates_path = criterion_dir
            .join(bench_path)
            .join("new")
            .join("estimates.json");
        let data = fs::read_to_string(&estimates_path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&data).ok()?;
        let mean = v["mean"]["point_estimate"].as_f64()?;
        let std_dev = v["std_dev"]["point_estimate"].as_f64()?;
        Some((mean, std_dev))
    }

    pub fn emit(criterion_dir: &Path, output_dir: &Path) {
        let profile_name = std::env::var("PROFILE").unwrap_or_else(|_| "release".to_owned());
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Define all expected workloads
        // Each entry: (criterion_path, family, workload, concurrency)
        let workloads: Vec<(&str, &str, &str, u32)> = vec![
            ("codec/parse_single", "codec", "parse_single", 1),
            ("codec/emit_single", "codec", "emit_single", 1),
            ("codec/roundtrip", "codec", "roundtrip", 1),
            ("gateway/decode_10k", "gateway", "decode_10k", 1),
            ("gateway/broadcast_fanout/c1", "gateway", "broadcast_fanout", 1),
            ("gateway/broadcast_fanout/c5", "gateway", "broadcast_fanout", 5),
            ("gateway/broadcast_fanout/c10", "gateway", "broadcast_fanout", 10),
            ("gateway/broadcast_fanout/c20", "gateway", "broadcast_fanout", 20),
            ("e2e/roundtrip/c1", "e2e", "roundtrip", 1),
            ("e2e/roundtrip/c5", "e2e", "roundtrip", 5),
            ("e2e/roundtrip/c10", "e2e", "roundtrip", 10),
            ("e2e/roundtrip/c20", "e2e", "roundtrip", 20),
        ];

        let mut results: Vec<BenchResult> = Vec::new();
        for (path, family, workload, concurrency) in &workloads {
            if let Some((mean_ns, std_dev_ns)) = read_estimates(criterion_dir, path) {
                let cv_pct = if mean_ns > 0.0 {
                    (std_dev_ns / mean_ns) * 100.0
                } else {
                    0.0
                };
                results.push(BenchResult {
                    family: family.to_string(),
                    workload: workload.to_string(),
                    concurrency: *concurrency,
                    mean_ns,
                    std_dev_ns,
                    cv_pct,
                    samples: 100, // criterion default; actual sample count not in estimates.json
                });
            }
        }

        let report = BenchReport {
            schema_version: "v3".to_owned(),
            bench_name: "comprehensive_bench".to_owned(),
            git_sha: git_sha(),
            engine_identity: EngineIdentity {
                rust_toolchain: "nightly-2026-01-22".to_owned(),
                supergateway_version: env!("CARGO_PKG_VERSION").to_owned(),
                profile: profile_name.clone(),
            },
            host_id: hostname(),
            profile_name,
            timestamp_unix: now_unix,
            results,
        };

        let json = match serde_json::to_string_pretty(&report) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("[json_v3] serialization error: {e}");
                return;
            }
        };

        // Write atomically using a temp file in the same directory
        if let Err(e) = fs::create_dir_all(output_dir) {
            eprintln!("[json_v3] failed to create output dir: {e}");
            return;
        }

        let tmp_path = output_dir.join("comprehensive_bench.latest.json.tmp");
        let final_path = output_dir.join("comprehensive_bench.latest.json");

        let write_result = (|| -> std::io::Result<()> {
            let mut f = fs::File::create(&tmp_path)?;
            f.write_all(json.as_bytes())?;
            f.flush()?;
            drop(f);
            fs::rename(&tmp_path, &final_path)?;
            Ok(())
        })();

        match write_result {
            Ok(()) => {
                eprintln!(
                    "[json_v3] wrote {} results to {}",
                    report.results.len(),
                    final_path.display()
                );
            }
            Err(e) => {
                eprintln!("[json_v3] write error: {e}");
                let _ = fs::remove_file(&tmp_path);
            }
        }
    }
}

// ─── main entry point ─────────────────────────────────────────────────────────

/// Single criterion function that runs all bench families then emits the JSON v3 summary.
pub fn run_all_and_emit(c: &mut Criterion) {
    // Run all bench families
    bench_codec_parse_single(c);
    bench_codec_emit_single(c);
    bench_codec_roundtrip(c);
    bench_gateway_throughput(c);
    bench_gateway_broadcast_fanout(c);
    bench_e2e_roundtrip(c);

    // Emit JSON v3 after all measurements are done.
    // criterion writes estimates.json to target/criterion/ — we read them back.
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let criterion_dir = manifest_dir.join("target").join("criterion");
    let bench_history_dir = manifest_dir.join(".bench-history");
    json_v3::emit(&criterion_dir, &bench_history_dir);
}

criterion_group!(all_benches, run_all_and_emit);
criterion_main!(all_benches);
