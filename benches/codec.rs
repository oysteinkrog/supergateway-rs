use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::io::Cursor;

use supergateway_rs::cli::{LogLevel, OutputTransport};
use supergateway_rs::codec::{StdoutCodec, DEFAULT_PARTIAL_BUFFER};
use supergateway_rs::observe::{Logger, Metrics};

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
            let metrics = Metrics::new();
            let logger = Logger::new(OutputTransport::Sse, LogLevel::None);
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

criterion_group!(benches, bench_parse_10k_messages);
criterion_main!(benches);
