#!/usr/bin/env python3
"""
Measures mcptracer's per-message forwarding overhead: round-trip latency of
`tools/call echo` sent directly to tests/fake_mcp_server.py versus the same
calls sent through `mcptracer record` wrapping the same server.

This is a local, single-machine, single-process measurement against a
trivial in-process Python "server" - it is not a substitute for measuring
your own real MCP server under real load. It exists to answer one narrow
question: how much latency does mcptracer's forward-before-record stdio
proxying add on top of a raw round trip. See docs-site/src/benchmarks.md for
the disclosed numbers and full methodology this script produces.

Usage: python scripts/bench-overhead.py [--calls N] [--json]
"""

import argparse
import json
import statistics
import subprocess
import sys
import time
from pathlib import Path


def frame(message: dict) -> bytes:
    return json.dumps(message, separators=(",", ":")).encode("utf-8") + b"\n"


def roundtrip(proc: subprocess.Popen, message: dict) -> float:
    started = time.perf_counter()
    proc.stdin.write(frame(message))
    proc.stdin.flush()
    line = proc.stdout.readline()
    elapsed = time.perf_counter() - started
    if not line:
        raise RuntimeError("process closed stdout mid-benchmark")
    json.loads(line)  # fail loudly on a malformed/partial response
    return elapsed


def percentile(values: list[float], p: float) -> float:
    values = sorted(values)
    idx = min(len(values) - 1, int(round((len(values) - 1) * p)))
    return values[idx]


def run_series(cmd: list[str], calls: int, cwd: Path) -> list[float]:
    proc = subprocess.Popen(
        cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, cwd=cwd
    )
    try:
        roundtrip(
            proc,
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "bench-overhead", "version": "0.1.0"},
                },
            },
        )
        latencies = []
        for i in range(2, 2 + calls):
            latencies.append(
                roundtrip(
                    proc,
                    {
                        "jsonrpc": "2.0",
                        "id": i,
                        "method": "tools/call",
                        "params": {"name": "echo", "arguments": {"message": "bench"}},
                    },
                )
            )
        return latencies
    finally:
        proc.stdin.close()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


def summarize(label: str, latencies_s: list[float]) -> dict:
    ms = [v * 1000 for v in latencies_s]
    return {
        "label": label,
        "n": len(ms),
        "mean_ms": round(statistics.fmean(ms), 3),
        "p50_ms": round(percentile(ms, 0.50), 3),
        "p95_ms": round(percentile(ms, 0.95), 3),
        "max_ms": round(max(ms), 3),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--calls", type=int, default=200)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    repo = Path(__file__).resolve().parent.parent
    fake_server = repo / "tests" / "fake_mcp_server.py"
    if not fake_server.exists():
        sys.exit(f"fake server not found: {fake_server}")

    # Pre-build so the release binary's own compile time never leaks into a
    # timed round trip (cargo run would otherwise pollute the first sample).
    subprocess.run(
        ["cargo", "build", "--quiet", "--release", "--bin", "mcptracer"],
        cwd=repo, check=True,
    )
    binary = repo / "target" / "release" / "mcptracer"
    if not binary.exists():
        binary = repo / "target" / "release" / "mcptracer.exe"
    if not binary.exists():
        sys.exit(f"built binary not found under {repo / 'target' / 'release'}")

    scratch_db = repo / "target" / "bench-overhead-scratch.db"
    scratch_db.unlink(missing_ok=True)

    direct_cmd = [sys.executable, str(fake_server)]
    proxied_cmd = [
        str(binary), "--db", str(scratch_db),
        "record", "--client", "bench-overhead", "--", sys.executable, str(fake_server),
    ]

    direct = summarize("direct (no mcptracer)", run_series(direct_cmd, args.calls, repo))
    proxied = summarize("through mcptracer record", run_series(proxied_cmd, args.calls, repo))
    overhead_p50_ms = round(proxied["p50_ms"] - direct["p50_ms"], 3)
    overhead_p95_ms = round(proxied["p95_ms"] - direct["p95_ms"], 3)

    result = {
        "direct": direct,
        "proxied": proxied,
        "overhead_p50_ms": overhead_p50_ms,
        "overhead_p95_ms": overhead_p95_ms,
    }

    if args.json:
        print(json.dumps(result, indent=2))
        return

    for series in (direct, proxied):
        print(
            f"{series['label']:<28} n={series['n']:<5} "
            f"mean={series['mean_ms']}ms p50={series['p50_ms']}ms "
            f"p95={series['p95_ms']}ms max={series['max_ms']}ms"
        )
    print(f"overhead (p50): {overhead_p50_ms}ms   overhead (p95): {overhead_p95_ms}ms")


if __name__ == "__main__":
    main()
