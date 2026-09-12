# Demo: Catching a Tool-Contract Rug-Pull

An MCP server exposes a `send_email` tool. You review it, trust it, and wire
it into your agent. Weeks later the package gets a routine-looking update.
The tool still does exactly what it always did — same call, same response
text — but its own self-declared contract now admits it silently BCCs a copy
of every email to an address you never agreed to. Nothing in a normal
transcript review would catch this: the actual request/response bytes never
change. Only the tool's `tools/list` description does, and only once, quietly.

This is what MCPTracer's `diff` SECURITY findings and
`assert kind = "tools_pinned"` exist to catch.

## Run it

```bash
./run.sh
```

(Needs `mcptracer` on `PATH`, or set `MCPTRACER_BIN=/path/to/mcptracer`. Needs
`python3` — set `PYTHON=python` if that's your interpreter name instead.)

## What it does

1. **Records a trusted baseline** — `mcptracer record` wraps `server.py` and
   drives it through a scripted `initialize` / `tools/list` / `send_email`
   call (`client_script.jsonl`).
2. **Validates and pins the trusted contract** — `mcptracer validate` checks
   capture integrity, then `mcptracer assert --spec pin.toml --manifest ...`
   checks the recorded `tools/list` response against a SHA-256 hash of the
   full contract and records the passing evidence digest.
3. **Exports and verifies redacted evidence** — the baseline was recorded with
   `--redact default`; `mcptracer export` writes a guarded `.mtrace`
   artifact, and `mcptracer verify` checks it and `pin.toml` against the
   manifest entirely offline.
4. **Simulates the rug-pull** — re-records the same scripted client run
   against the server with `MCPTRACER_DEMO_RUG_PULLED=1`, which changes
   only the tool's declared description. The actual `send_email` call and
   its response are byte-for-byte identical to the baseline.
5. **`mcptracer diff` catches it** — reports a `SECURITY [description
   changed]` finding on the `tools/list` exchange, even though the call/
   response transcript itself has zero differences.
6. **`mcptracer assert --spec pin.toml` catches it too** — the pinned hash
   no longer matches, so a CI gate built on the pinned contract fails
   deterministically instead of trusting a tool that quietly changed under
   it.

## Why this matters

A diff-only workflow that just compares transcripts would miss this
entirely, since the transcript never changes — only what the tool discloses
about itself does. Pinning the full tool contract (not just watching call
traffic) is what turns "the server said something different this time" into
"the server is now a different tool than the one you approved," which is the
actual security question in an MCP supply chain.

## Files

- `server.py` — the demo MCP server. `MCPTRACER_DEMO_RUG_PULLED=1` switches
  it to the compromised variant.
- `client_script.jsonl` — the scripted client traffic `record` drives the
  server with (identical for both runs, by design).
- `pin.toml` — the trusted tool-contract hash, pre-computed from a real
  baseline recording. See the comment in the file for how to regenerate it.
- `run.sh` — runs the whole demo end to end, including a redacted artifact
  export and offline manifest verification, and exits nonzero if either
  `diff` or `assert` fails to catch the rug-pull. Temporary evidence is
  removed on exit; copy the printed commands and choose persistent paths when
  adapting the workflow to your project.
