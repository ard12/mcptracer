use std::sync::mpsc;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use mcptracer_protocol::{parse_json_payload, parse_stdio_frame, Direction, McpMessage};

fn frame() -> Vec<u8> {
    br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"/workspace/src/lib.rs"}}}"#
        .iter()
        .copied()
        .chain(std::iter::once(b'\n'))
        .collect()
}

fn bench_frame_parse(c: &mut Criterion) {
    let frame = frame();
    c.bench_function("stdio_frame_parse", |b| {
        b.iter(|| {
            let parsed = parse_stdio_frame(black_box(&frame)).unwrap().unwrap();
            let payload = parse_json_payload(black_box(&parsed.json_bytes)).unwrap();
            black_box(payload);
        });
    });
}

fn bench_parse_and_nonblocking_enqueue(c: &mut Criterion) {
    let frame = frame();
    let (tx, rx) = mpsc::sync_channel(64);
    c.bench_function("stdio_parse_and_nonblocking_enqueue", |b| {
        b.iter(|| {
            let parsed = parse_stdio_frame(black_box(&frame)).unwrap().unwrap();
            let payload = parse_json_payload(&parsed.json_bytes).unwrap();
            let message = McpMessage {
                seq: 0,
                timestamp_ns: 0,
                direction: Direction::ClientToServer,
                payload_bytes: parsed.json_bytes.len(),
                payload,
            };
            tx.try_send(message).unwrap();
            black_box(rx.try_recv().unwrap());
        });
    });
}

criterion_group!(
    benches,
    bench_frame_parse,
    bench_parse_and_nonblocking_enqueue
);
criterion_main!(benches);
