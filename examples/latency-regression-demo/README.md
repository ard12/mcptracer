# Demo: Catching a Latency Regression Before Release

A `search` tool responds in tens of milliseconds today. A routine-looking
change — an accidentally-synchronous network call, an N+1 query, a retry
loop with no backoff cap — ships in the next release. The tool still returns
the exact same result. It just takes ten times longer to do it, and nobody
notices until users complain, because nothing in the response text changed.

This is what `mcptracer assert kind = "latency"` exists to catch: a numeric,
CI-enforceable release gate instead of relying on someone eyeballing a
changelog or a flame graph.

## Run it

```bash
./run.sh
```

(Needs `mcptracer` on `PATH`, or set `MCPTRACER_BIN=/path/to/mcptracer`. Needs
`python3` — set `PYTHON=python` if that's your interpreter name instead.)

## What it does

1. **Records the current release** — `mcptracer record` wraps `server.py`
   and drives it through a scripted `initialize` / `tools/list` / three
   `search` calls (`client_script.jsonl`).
2. **Gates it** — `mcptracer assert --spec checks.toml` checks
   `kind = "latency"`, `tool = "search"`, `max_under_ms = 100.0`. Passes,
   because the current release is fast.
3. **Simulates the regression** — re-records the same scripted client run
   against the server with `MCPTRACER_DEMO_SLOW_MS=250`, which adds a
   blocking sleep inside the tool handler. The result text is identical to
   the baseline; only how long it took to produce changed.
4. **The same gate now fails** — `assert` reports the offending max latency
   against the 100ms threshold, exiting nonzero exactly the way a CI release
   gate should.
5. **`mcptracer diff` shows the same story with numbers, not just pass/
   fail** — a per-call latency delta (`latency: 49.6ms -> 303.3ms (+512%)`)
   for each of the three `search` calls.

## Why this matters

Diffing response *content* alone would report these two sessions as
identical — the regression is invisible in the transcript. An explicit
`latency` assertion is what turns "it's a little slower now, is that
normal?" into a deterministic pass/fail a release pipeline can act on
without a human in the loop, satisfying the same "regression caught before
release" bar a maintainer would hold any release process to.

## Files

- `server.py` — the demo MCP server. `MCPTRACER_DEMO_SLOW_MS=<n>` adds an
  `n`-millisecond blocking sleep to every `search` call.
- `client_script.jsonl` — the scripted client traffic `record` drives the
  server with (identical for both runs, by design).
- `checks.toml` — the latency release gate.
- `run.sh` — runs the whole demo end to end and exits nonzero if the gate
  fails to catch the regression.
