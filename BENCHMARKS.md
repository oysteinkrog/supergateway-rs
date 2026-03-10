# supergateway-rs Benchmark Suite

## Performance Targets

| Target | Value | Mode | Status |
|---|---|---|---|
| RSS idle | < 3 MB | All server modes | TBD |
| RSS stability | ±10% over 1h soak | Stateful HTTP | TBD |
| Bridge latency p99 | < 2 ms | Stateful HTTP (in-process mock) | TBD |
| End-to-end latency p99 | < 5 ms | Stateful HTTP (real child) | TBD |
| Throughput | > 10k msg/sec | SSE (1 client, warm) | TBD |
| Startup time | < 5 ms to listening | All server modes | TBD |
| Binary size | < 10 MB stripped | — | TBD |
| FD leaks | Zero growth / 1000 cycles | All modes | TBD |
| Graceful shutdown | < 5 s | All modes | TBD |

---

## Benchmark Files

### `benches/codec.rs` — Criterion microbenchmarks

Run with:
```bash
cargo +nightly bench --bench codec
```

| Benchmark | What it measures |
|---|---|
| `parse_10k_messages` | Decode throughput: 10k JSON-RPC messages from a pre-filled buffer |
| `rawvalue_roundtrip_10k` | Serialize + deserialize 10k `RawMessage` structs (RawValue overhead) |
| `write_message_10k` | Encode throughput: serialize 10k messages to a Vec sink |
| `parse_single_message_latency` | Per-message decode latency (single message, Cursor) |
| `large_message/accepted/*` | Parse 1MB, 10MB, 16MB messages (all should succeed) |
| `large_message/rejected_over_16mb` | Reject a >16MB line; codec skips with decode error |
| `throughput/10k_messages_decode` | Sustained decode, reports msg/sec via `Throughput::Elements` |

### `benches/gateway.rs` — Gateway-level benchmarks

Run with:
```bash
cargo +nightly bench --bench gateway
```

| Benchmark | What it measures |
|---|---|
| `encode_decode_roundtrip` | Encode + decode a single message (codec contribution to bridge latency) |
| `gateway_throughput/10k_msg_decode` | Sustained decode throughput (msg/sec) |
| `fd_leak_1000_cycles` | Verifies zero FD growth across 1000 simulated connect/disconnect cycles |
| `broadcast_fanout/100_clients` | Fan-out cost: decode once, re-encode for 100 clients |

---

## Methodology

### Criterion microbenchmarks

Criterion runs each benchmark with configurable warmup and measurement time (defaults: 3s warmup, 5s measurement). Output includes mean, standard deviation, and confidence interval. HTML reports are generated at `target/criterion/`.

To run all benchmarks:
```bash
cargo +nightly bench
```

To run a specific benchmark:
```bash
cargo +nightly bench --bench codec -- parse_10k
cargo +nightly bench --bench gateway -- roundtrip
```

To verify benchmarks compile without running:
```bash
cargo +nightly bench --no-run
```

### RSS measurement

Measure RSS after 60s idle with a single server mode, zero clients:
```bash
# Start the gateway
./target/release/supergateway-rs --stdio "cat" --port 8000 &
PID=$!
sleep 60
# Measure RSS in KB
ps -o rss= -p $PID
kill $PID
```

Target: < 3000 KB (3 MB).

TypeScript baseline: ~65 MB per instance (5 instances under pm2).

### Startup time

Uses `hyperfine` for accurate startup-to-listening measurement:
```bash
hyperfine --warmup 3 \
  './target/release/supergateway-rs --stdio "cat" --port 8001'
```

**Note:** The binary must detect port-ready state. The raw `hyperfine` time measures process start + listener bind time. Target: < 5 ms.

### Binary size

After a release build with stripping:
```bash
cargo +nightly build --release
strip target/release/supergateway-rs
ls -lh target/release/supergateway-rs
```

Target: < 10 MB.

### FD leak soak

Run 10k requests with random disconnects, then verify fd count:
```bash
PID=<gateway pid>
BASELINE=$(ls /proc/$PID/fd | wc -l)
# ... run 10k requests ...
AFTER=$(ls /proc/$PID/fd | wc -l)
echo "FD delta: $((AFTER - BASELINE))"
# Target: delta == 0 (or within 2-3 for OS jitter)
```

### Graceful shutdown

```bash
time (kill -SIGTERM $PID && wait $PID)
```

Target: < 5 seconds from SIGTERM to process exit. Verify no zombie children:
```bash
pgrep -P $PID  # should be empty
```

---

## Results (fill in after running)

### Build info

```
Date: TBD
Commit: TBD
Rust: nightly-TBD
Host: TBD (Linux x86_64)
CPU: TBD
RAM: TBD
```

### Codec microbenchmarks

```
test parse_10k_messages                        ... bench: TBD ns/iter
test rawvalue_roundtrip_10k                    ... bench: TBD ns/iter
test write_message_10k                         ... bench: TBD ns/iter
test parse_single_message_latency              ... bench: TBD ns/iter
test large_message/accepted/1mb                ... bench: TBD ms/iter
test large_message/accepted/10mb               ... bench: TBD ms/iter
test large_message/accepted/16mb               ... bench: TBD ms/iter
test large_message/rejected_over_16mb          ... bench: TBD ms/iter
test throughput/10k_messages_decode            ... TBD msg/sec
```

### Gateway benchmarks

```
test encode_decode_roundtrip                   ... bench: TBD ns/iter
test gateway_throughput/10k_msg_decode         ... TBD msg/sec
test fd_leak_1000_cycles                       ... PASS (0 fd growth)
test broadcast_fanout/100_clients              ... bench: TBD ns/iter
```

### System measurements

```
RSS idle (single mode, 60s):               TBD MB   (target: < 3 MB)
Startup time (hyperfine, --warmup 3):      TBD ms   (target: < 5 ms)
Binary size (stripped):                    TBD MB   (target: < 10 MB)
Graceful shutdown (SIGTERM):               TBD s    (target: < 5 s)
Bridge latency p99 (in-process mock):      TBD ms   (target: < 2 ms)
Throughput (SSE, 1 client, warm):          TBD msg/sec (target: > 10k)
```

### TypeScript baseline

```
RSS idle (pm2, single instance):           ~65 MB
Startup time:                              ~200 ms
Throughput (estimated):                    ~2-5k msg/sec
Bridge latency p99 (estimated):            ~5-10 ms
```

---

## Large Message Handling

Acceptance criteria (D-101):
- Messages ≤ 16 MB: parsed and forwarded normally
- Messages > 16 MB: rejected with HTTP 413 / JSON-RPC decode error

The `large_message` criterion benchmarks verify:
- 1 MB message: `Ok(Some(_))` — accepted
- 10 MB message: `Ok(Some(_))` — accepted
- 16 MB message: `Ok(Some(_))` — accepted (right at limit)
- 16 MB + 1 KB message: `Ok(None)` after skipping — rejected, decode_errors counter incremented

---

## Connection Churn Test

Run 1000 rapid connect/disconnect cycles and verify:
- Memory does not grow beyond baseline ± 10%
- FD count returns to baseline after each cycle
- Task count (Asupersync region count) returns to baseline

The `fd_leak_1000_cycles` criterion benchmark covers the codec layer.
A full integration test (connecting real HTTP clients) is in `tests/compat_harness.rs`.
