# Benchmarks

## What this measures

`scripts/bench-overhead.py` compares direct and proxied synthetic MCP requests
for tiny stdio, large-payload stdio, and loopback Streamable HTTP. It reports
repeated latency distributions, throughput, sampled process memory, storage
sizes, capture completeness, source/binary provenance, a bounded stdio soak,
and a controlled-delay sensitivity check. These fixtures measure one machine;
they do not represent production traffic, concurrent load, network-bound
servers, or production reliability. For a bounded repeatable run:

```bash
python scripts/bench-overhead.py --calls 50 --large-calls 10 --warmup 5 --repetitions 3 --json
```

The results and method below preserve an earlier narrow tiny-stdio experiment.
Use `--output PATH` to retain current raw JSON evidence.

## Earlier method behind the table below

- Target: `tests/fake_mcp_server.py`, the same trivial in-process Python
  echo server the integration test suite uses.
- Each series: one `initialize` handshake, then 200 sequential
  `tools/call echo` requests, each timed from write to the process's stdin
  until its matching response line is read back — one round trip at a
  time, no concurrency.
- "Direct": the fake server run standalone. "Proxied": the same fake
  server wrapped by a pre-built `mcptracer record --release` binary (no
  `cargo run` compile step in the timed path).
- Reported below: 4 independent runs of 200 calls each, not a single
  cherry-picked run.

## Earlier results

Measured on the sandboxed development environment this project was
hardened in: Windows 10 (MINGW64), Intel x86_64, 12 logical cores — not a
production server, not representative of your hardware. Latencies at this
scale (sub-millisecond) are dominated by OS scheduling noise as much as by
mcptracer's own code, which is exactly why 4 runs are shown instead of one.

| Run | Direct p50 | Proxied p50 | Overhead p50 | Direct p95 | Proxied p95 | Overhead p95 |
|---|---|---|---|---|---|---|
| 1 | 0.057ms | 0.103ms | **0.046ms** | 0.079ms | 0.253ms | 0.174ms |
| 2 | 0.032ms | 0.066ms | **0.034ms** | 0.082ms | 0.116ms | 0.034ms |
| 3 | 0.033ms | 0.079ms | **0.046ms** | 0.101ms | 0.178ms | 0.077ms |
| 4 | 0.033ms | 0.072ms | **0.039ms** | 0.105ms | 0.148ms | 0.043ms |

p50 overhead is consistent across runs: roughly **0.03–0.05ms** per call.
p95 overhead is noisier (0.03–0.17ms across these runs), as expected at
sub-millisecond scale — treat it as "well under a millisecond," not a
precise figure.

## Interpreting this

For any real MCP server (one that does actual work — a network call, a
database query, a model invocation), that server's own latency will
typically be orders of magnitude larger than this proxy overhead, making
mcptracer's relative contribution to end-to-end latency small in practice.
This page will be updated with a real-server measurement once one is
available to benchmark against without exposing a third party's traffic.

## Reproducible workload runner

The bounded runner now measures direct and proxied tiny stdio, 256 KiB stdio,
and loopback Streamable HTTP workloads over repeated runs. It emits revision,
source-state and binary SHA-256 digests; OS, CPU and toolchain; latency
percentiles, throughput, sampled process memory, SQLite/WAL sizes and capture
completeness. The old table above is the earlier narrow tiny-stdio study; it is
not an acceptance threshold for the expanded workloads.

Repeat a bounded baseline with:

```bash
python scripts/bench-overhead.py --calls 50 --large-calls 10 --warmup 5 --repetitions 3 --json
```

Use `--output PATH` to retain the JSON evidence. The tool limits calls,
payload size and repetitions, and stores transient databases in a temporary
directory. These loopback fixtures do not represent production traffic or
concurrent load. Memory is sampled and may miss short peaks. The default runner also records a five-second bounded soak and compares a
controlled 25 ms slower fake-server run with the baseline. These checks show
that the harness can observe an injected delay; they are not a production
latency threshold. No performance threshold is enabled by this measurement.
