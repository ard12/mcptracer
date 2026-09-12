# Real-SDK compatibility matrix

Closes T-75's deferred half. Every other test in this repository validates
MCPTracer against fixtures MCPTracer's own author wrote
(`tests/fake_mcp_server.py`, `tests/fake_mcp_http_server.py`, the
`examples/*/server.py` demos) — this is the first time it has been tested
against independent implementations. Harness: `tests/compat/` (see its
`README.md` for how to run one cell locally, and `matrix.toml` for the
enumerated cell definitions).

**Legacy-matrix status: 3 of 4 MCP 2025-06-18 cells pass fully; 1 documented,
non-blocking SDK-side flake.** These results do not establish MCP 2026-07-28
SDK compatibility. T-92A adds a hand-built modern HTTP fixture for stateless
record/replay and standard routing headers; the current Tier 1 SDK matrix is
still open and tracked in [`mcp-2026-07-28.md`](mcp-2026-07-28.md).

## Results

Each row's six evidence-chain steps: **record** (baseline against the
unmodified server), **validate** (capture integrity), **assert** (pinned
tool-contract hash), **export+verify** (offline digest match), **replay+diff**
(re-driven against a fresh instance, no meaningful difference), **rug-pull**
(a silent tool-description change, `diff`/`assert` must both catch it).

| SDK | Version | Transport | OS tested | Result | Notes |
| --- | --- | --- | --- | --- | --- |
| `@modelcontextprotocol/sdk` (TypeScript) | 1.30.0 | stdio | Windows | **PASS** (6/6) | |
| `@modelcontextprotocol/sdk` (TypeScript) | 1.30.0 | Streamable HTTP | Windows | **PASS** (6/6) | See "HTTP transport findings" below — both real, both closed in the harness, neither a product bug |
| `mcp` (Python, FastMCP) | 1.29.0 | Streamable HTTP | Windows | **PASS** (6/6) | |
| `mcp` (Python, FastMCP) | 1.29.0 | stdio | Windows | **FAIL, not required** | SDK-side flake, confirmed independent of MCPTracer — see below |

Node.js `v24.14.1`, Python `3.14.3`. Dates: harness built and run 2026-08-15.

**Linux and macOS: wired into CI (`.github/workflows/ci.yml`'s `compat`
job, three-OS matrix, same as `test`/`msrv`), not yet exercised by an
actual run** — this pass built and ran the harness locally, on Windows,
which is what this development environment has. The CI job will produce
real Linux/macOS results on the next push; this row will be updated then,
not before. Recorded here rather than silently implied to be covered,
matching this document's own standard for the sections below.

### Checked directly, not a finding

Two of the failure candidates this matrix was built expecting to hit did
not turn into problems, worth recording so nobody re-checks them from
scratch:

- **Protocol-version negotiation.** Both real SDKs echoed the requested
  `2025-06-18` unconditionally, same as the hand-rolled fakes. No
  negotiate-down or reject-unknown-version behavior was observed (or
  exercised — the client script only ever requests one version).
- **Richer capability/tool-list shapes.** Real. `capabilities.tools` came
  back as `{"listChanged": true}` (TypeScript) or with a populated
  `experimental`/`prompts`/`resources` block (Python) instead of the fake's
  bare `{}`; `tools/list` entries carried extra fields the fakes never
  emit (TypeScript: `execution`, `$schema`, `additionalProperties`; Python:
  `outputSchema`, JSON-Schema `title` on every property). None of this
  caused a failure — `assert --spec pin.toml`'s hash covers each SDK's own
  full contract as bootstrapped, and `diff`/replay comparisons are always
  same-SDK-against-itself, never against the fake — but it is a real,
  observed difference in what MCPTracer's own fixtures had ever exercised
  before this pass.

## Findings

### F1 — Python SDK stdio transport can lose the last tool-call response (SDK-side, confirmed independent of MCPTracer)

**What happens:** recording a session against `tests/compat/servers/py-stdio`
(the `mcp` 1.29.0 / FastMCP stdio server) intermittently — roughly half the
time across repeated local runs — never receives a response to the final
`tools/call` request. `mcptracer validate` correctly reports
`UnansweredRequest`. It is always the *last* in-flight request (`tools/call`
in this client script); `initialize` and `tools/list` are never affected.

**Diagnosis, not taken on faith:** reproduced with zero MCPTracer/proxy
involvement — piping the identical client script directly into
`python server.py` with a plain shell redirect is flaky the same way (2 or 3
of 3 responses across five runs). Adding an explicit 300ms delay before
closing stdin, to rule out a simple "client hangs up before the server can
finish" race, did not make it reliable either (still 2-3 across five more
runs). The failure is therefore internal to the SDK's own async task
scheduling around its last in-flight response and stdin-EOF handling, not a
proxy-forwarding or record-timing issue on MCPTracer's side.

**Disposition:** `tests/compat/matrix.toml`'s `py-stdio` cell is marked
`required = false` rather than removed, so this stays visible in every run
instead of silently dropping from coverage. No MCPTracer product code was
touched to chase this — per this exercise's own standing rule, a legitimate
SDK difference gets documented, not worked around by changing product
behavior to paper over someone else's race condition. Worth re-testing
against a newer `mcp` release if this matrix is re-run later; not
re-attempted with `mcp==2.0.0` in this pass (see "What was not tested").

### HTTP transport findings (both real, both closed, neither a product bug)

Building the Streamable HTTP cells surfaced two behaviors real SDK servers
exercise that the existing fake HTTP fixture apparently never had — both are
consequences of already-documented, deliberate MCPTracer design, not new
bugs, and both are now handled inside `tests/compat/run_matrix.py` itself
(harness code, not product code):

- **`record-http` needs an explicit session-termination signal to close a
  session immediately.** Ending the driving HTTP client and killing the
  `record-http` process leaves the session's `ended_at_ns` unset
  (`SessionNotClosed` on `validate`), even though every message was captured
  correctly. The fix is the MCP Streamable HTTP transport's own mechanism: a
  `DELETE` request carrying the session's `Mcp-Session-Id`. The harness now
  sends one after driving each cell's client script.
- **The `initialize` handshake lands in its own separate "provisional"
  session**, distinct from the session the rest of the traffic joins once
  the server assigns a real `Mcp-Session-Id` — because the very first
  request necessarily carries no session id yet. This is T-70's documented
  logical-session-partitioning tradeoff
  (`docs/spec/transport-http.md`), not new. Replaying "the rest" without its
  own `initialize` first correctly fails against a real server
  (`400 Bad Request: Server not initialized`) — a real server enforcing a
  real invariant, working as intended. `run_matrix.py`'s
  `latest_recorded_session()` detects the split and reassembles the two
  sessions with `mcptracer merge` before running the rest of the chain,
  exactly what a person piecing a capture back together by hand would need
  to do.

## What was not tested

Named explicitly rather than silently absent, per this document's own
standard:

- **Notifications the fakes never send** (`notifications/cancelled`,
  progress, logging) — the client script drives only `initialize`,
  `notifications/initialized`, `tools/list`, and one `tools/call`. Neither
  real SDK's handling of server-pushed notifications mid-call is exercised.
- **Server-initiated requests** (sampling, roots, elicitation) — `replay
  --strict-server-requests` exists for this case, but no cell's server
  actually issues one.
- **CRLF vs. LF stdio framing** — both real servers happened to use LF; no
  cell forces CRLF to check Windows-specific framing edge cases.
- **`mcp` 2.0.0** (the Python SDK's newest major version, released after
  1.29.0) — this pass deliberately pinned 1.29.0, the last stable 1.x
  release, since 2.0.0's restructured module layout
  (`mcp.server.mcpserver`/`mcp.server.apps` replacing the classic
  `mcp.server.fastmcp.FastMCP` entry point in ways this pass did not have
  time to verify) was unfamiliar enough to risk burning the session
  debugging an SDK API change rather than testing actual MCP interop. Worth
  a follow-up pass once 2.0.0 has been in the wild longer.
- **Linux and macOS**, per the table above — wired into CI, not yet run.
- **Non-loopback / real network conditions** — every cell talks to
  `127.0.0.1`; no cell exercises latency, packet loss, or a genuinely remote
  target.
- **Additional real SDKs** (e.g. a Rust or Go MCP SDK, or other Python/JS
  frameworks built on top of the reference SDKs) — only the two reference
  SDKs (`@modelcontextprotocol/sdk`, `mcp`) are covered.

## Do not claim the milestone unconditionally

The 0.3 milestone's exit gate is "multiple real MCP implementations pass the
compatibility suite." Three of four cells pass fully on the one OS this pass
could actually run, satisfying that literally — but "not yet run on
Linux/macOS," "one Python transport has a disclosed non-blocking flake," and
the "what was not tested" list above are real, current gaps. Update this
document's results table, not just its prose, the next time the CI `compat`
job actually runs.
