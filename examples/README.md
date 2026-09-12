# MCPTracer Demos

Two self-contained, runnable demonstrations of what MCPTracer's evidence
chain catches that a transcript-only review would miss. Each is a real MCP
stdio server plus a scripted client run — clone the repo, build `mcptracer`,
and run `./run.sh` in either directory to see it end to end.

- [`rug-pull-demo/`](rug-pull-demo/) — a tool's declared contract silently
  changes (a covert BCC added to an email tool) while its actual call/
  response traffic stays byte-for-byte identical. `diff` and
  `assert kind = "tools_pinned"` both catch it; a plain transcript diff
  would not.
- [`latency-regression-demo/`](latency-regression-demo/) — a tool's response
  gets ten times slower while returning the exact same result.
  `assert kind = "latency"` gates a release on it explicitly; `diff` shows
  the per-call latency delta.

Both scripts exit nonzero if MCPTracer fails to catch the scenario they
demonstrate — they're runnable regression checks, not just narrated
walkthroughs, and are also exercised by
`tests/test_proxy_integration.py::test_canonical_demos_catch_their_scenarios`.

## Streamable HTTP note

These two first-use demos use stdio, where one scripted client stream maps to
one recording. Legacy Streamable HTTP differs: the header-less `initialize`
request is recorded in a provisional session, while later requests join the
server-assigned `Mcp-Session-Id` session. When reproducing an HTTP capture,
list both sessions and merge them in order before replay:

```bash
mcptracer --db capture.db sessions list
mcptracer --db capture.db merge <initialize-session> <established-session>
mcptracer --db capture.db replay-http <merged-session> --target http://127.0.0.1:8000/mcp
```

The real-SDK compatibility harness automates the same split detection,
termination `DELETE`, merge, replay, and verification flow. See
[`docs/spec/compatibility-matrix.md`](../docs/spec/compatibility-matrix.md).
