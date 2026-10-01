# Real-SDK compatibility matrix

Closes T-75's deferred half. Every other test in this repository validates
MCPTracer against fixtures MCPTracer's own author wrote
(`tests/fake_mcp_server.py`, `tests/fake_mcp_http_server.py`, the
`examples/*/server.py` demos) — this is the first time it has been tested
against independent implementations. Harness: `tests/compat/` (see its
`README.md` for how to run one cell locally, and `matrix.toml` for the
enumerated cell definitions).

**Legacy-matrix status: 4 of 5 MCP 2025-06-18 cells pass fully; 1 documented,
non-blocking SDK-side flake.** These results do not establish MCP 2026-07-28
SDK compatibility. T-92A adds a hand-built modern HTTP fixture for stateless
record/replay and standard routing headers; the current Tier 1 SDK matrix is
still open and tracked in [`mcp-2026-07-28.md`](mcp-2026-07-28.md).


## Current-protocol lane (separate from the legacy table)

The new `go-modern-stdio` cell pins the official Go SDK at v1.7.0, the release
that explicitly supports MCP 2026-07-28. On Windows 10 with Go 1.27.0 and the
private dirty MCPTracer build, all 11 steps passed: record, validate, pin,
export, manifest, offline verify, fresh-process replay/diff, and rug-pull diff
plus pin refusal. The legacy `go-stdio` cell was rerun and also passed. This is
local evidence for one SDK/transport/OS, not hosted or cross-platform evidence.
The cell uses a dedicated self-describing `server/discover`/`tools/list`/
`tools/call` script; it does not alter or replace the 2025-06-18 baseline lane.
See `T-92 Go current-protocol evidence` (private record, not distributed).

### Current-protocol Python SDK v2

The required `py-modern-stdio` and `py-modern-http` cells pin the official
Python SDK at v2.2.0 and drive `server/discover`/`tools/list`/`tools/call` over
both transports. Each passed all 11 evidence-chain checks on Windows 10 with
Python 3.14.3 and the private dirty-tree debug binary. HTTP requests include
current routing headers and a per-run explicit stateless trace boundary; the
proxy closes that group through graceful shutdown. Both cells share a dedicated
virtual environment, leaving the legacy Python 1.29.0 cells unchanged. Exact
identities are in the stdio report (private record, not distributed)
and HTTP report (private record, not distributed).
Go v1.7.0 now passes stdio and stateless Streamable HTTP; TypeScript SDK
v2.2.0 now passes stdio and stateless Streamable HTTP. Together with Python
v2.2.0 stdio/HTTP, this is six local current-protocol cells across three
SDK families. Results are Windows-only; hosted OS runs remain open.

### Current-protocol TypeScript SDK v2

The required `ts-modern-stdio` and `ts-modern-http` cells pin
`@modelcontextprotocol/server@2.2.0` and `zod@4.2.0`; the HTTP cell also
pins `@modelcontextprotocol/node@2.1.0`. The stdio server uses the
era-aware `serveStdio` entry point. The HTTP server uses
`createMcpHandler` with legacy traffic rejected and the SDK Node adapter's
localhost Host/Origin guards. Both passed all 11 checks on Windows 10 Pro,
Node.js v24.14.1, against the private dirty-tree debug binary. Evidence:
stdio (private record, not distributed) and
HTTP (private record, not distributed).
The six current-protocol results remain local Windows evidence; Linux/macOS
hosted execution and full conformance remain open.

Current-protocol cell results:

| SDK | Version | Transport | OS tested | Result |
| --- | --- | --- | --- | --- |
| `github.com/modelcontextprotocol/go-sdk` | 1.7.0 | stdio | Windows 10 Pro | **PASS** (11 checks, local 2026-09-30) |
| `github.com/modelcontextprotocol/go-sdk` | 1.7.0 | stateless Streamable HTTP | Windows 10 Pro | **PASS** (11 checks, local 2026-09-30) |
| `mcp` (Python SDK) | 2.2.0 | stdio | Windows 10 Pro | **PASS** (11 checks, local 2026-09-30) |
| `mcp` (Python SDK) | 2.2.0 | Streamable HTTP | Windows 10 Pro | **PASS** (11 checks, local 2026-09-30) |
| `@modelcontextprotocol/server` (TypeScript SDK) | 2.2.0 | stdio | Windows 10 Pro | **PASS** (11 checks, local 2026-09-30) |
| `@modelcontextprotocol/server` (TypeScript SDK) | 2.2.0 | stateless Streamable HTTP | Windows 10 Pro | **PASS** (11 checks, local 2026-09-30) |

## Results

Each row's six evidence-chain steps: **record** (baseline against the
unmodified server), **validate** (capture integrity), **assert** (pinned
tool-contract hash), **export+verify** (offline digest match), **replay+diff**
(re-driven against a fresh instance, no meaningful difference), **rug-pull**
(a silent tool-description change, `diff`/`assert` must both catch it).

| SDK | Version | Transport | OS tested | Result | Notes |
| --- | --- | --- | --- | --- | --- |
| `@modelcontextprotocol/sdk` (TypeScript) | 1.30.0 | stdio | Windows 10 Pro | **PASS** (11 checks, local 2026-09-25) | Required cell `ts-stdio`; no failed stage. |
| `@modelcontextprotocol/sdk` (TypeScript) | 1.30.0 | Streamable HTTP | Windows 10 Pro | **PASS** (11 checks, local 2026-09-25) | Required cell `ts-http`; no failed stage. See "HTTP transport findings" below — both real, both closed in the harness, neither a product bug. |
| `mcp` (Python, FastMCP) | 1.29.0 | Streamable HTTP | Windows 10 Pro | **PASS** (11 checks, local 2026-09-25) | Required cell `py-http`; no failed stage. |
| `mcp` (Python, FastMCP) | 1.29.0 | stdio | Windows 10 Pro | **FAIL, not required** (0/20 complete with fire-and-EOF) | Python 3.14.3; protocol `2025-06-18`; proxied binary SHA-256 `F05ABF9A2D37D2A0130B897966C441EE7AAC9DEFC2A00E6EE73D72ABB0CE7488`; failure stage: direct response count and proxied `validate` (`UnansweredRequest`). Response-driven client: 20/20 direct and 20/20 proxied complete. Local Windows sample 2026-09-25; the cell remains optional. See below. |
| `github.com/modelcontextprotocol/go-sdk` (Go) | 1.8.0 | stdio | Windows 10 Pro | **PASS** (11 checks × 10 consecutive runs, local 2026-09-25) | Go 1.27.0; protocol `2025-06-18`; required; failure stage: none. Binary SHA-256 `F05ABF9A2D37D2A0130B897966C441EE7AAC9DEFC2A00E6EE73D72ABB0CE7488`, built from private HEAD `9a737f8` plus its working tree. Needs an interactive client — the SDK cancels in-flight requests on stdin EOF. Linux/macOS at a revision containing this cell remain untested. |

Node.js `v24.14.1`, Python `3.14.3`, Go `1.27.0`. On 2026-09-25, the current Windows 10 Pro checkout passed all four required cells (`ts-stdio`, `ts-http`, `py-http`, `go-stdio`), with all 11 checks passing per cell. The release binary SHA-256 was `F05ABF9A2D37D2A0130B897966C441EE7AAC9DEFC2A00E6EE73D72ABB0CE7488`; source was private HEAD `9a737f8` plus its working tree. This historical v1.8.0 Go row covers legacy stdio only; current-protocol Go HTTP is listed above.

**Linux and macOS: existing matrix cells passed hosted runs as of 2026-09-12.** Those runs predate the Go cell and therefore provide no Go SDK result. The table above records per-cell evidence; the Go row has only local Windows measurements so far. The `compat` job has passed on `ubuntu-latest`, `macos-latest`, and `windows-latest` for the cells present at those revisions, both on the private source revision and on the public repository's own CI for the exact published commit. A passing `compat` job means every **required** cell passed —
the Python stdio cell is `required = false`, so its disclosed SDK-side flake
does not gate the job and its per-OS status is not established by a green run.
The per-cell, per-OS table below has not been re-measured on Linux or macOS and
still reflects the Windows run; treat the OS column as "where this cell was
measured", not as a claim of parity.

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

### F1 — Python SDK stdio transport can lose the last tool-call response (SDK-side, reproduced without MCPTracer)

**What happens:** a bounded Windows retest on 2026-09-25 used Python 3.14.3
and `mcp` 1.29.0. With the fire-and-EOF client, the direct SDK server returned
only 2 of 3 responses in 20/20 runs; the proxied recording was incomplete and
`validate` reported `UnansweredRequest` in 20/20 runs. With a response-driven
client that kept stdin open until the expected responses arrived, all 3 direct
responses arrived in 20/20 runs and proxied recording/validation completed in
20/20 runs. The missing response was always the last in-flight request
(`tools/call`); `initialize` and `tools/list` completed.

**Diagnosis, not taken on faith:** the same fire-and-EOF loss reproduces with
zero MCPTracer/proxy involvement, while waiting for responses before EOF avoids
it in this bounded sample both directly and through MCPTracer. This shows that
the proxy is not required for the failure and that EOF handling is relevant.
The experiment covers one Windows environment; it does not prove the historical
flake is eliminated on other runs, Python versions or operating systems.

**Disposition:** `tests/compat/matrix.toml`'s `py-stdio` cell is marked
`required = false` rather than removed, so this stays visible in every run.
No MCPTracer product code was changed and no interactive mode was added to the
optional matrix cell. This 20-run sample is a measurement, not promotion
evidence. That 2026-09-25 run used `mcp==1.29.0`; the follow-up current-era
Python v2.2.0 stdio cell is recorded above. Python v2 Streamable HTTP remains
untested.

### Go SDK stdio: EOF cancels in-flight requests

**What happens:** the Go server (`tests/compat/servers/go-stdio`) returns **no
responses at all** if stdin reaches EOF before it has finished handling the
requests it just read. `mcptracer record` fed the four-message client script
and hit EOF immediately: all three requests (`initialize`, `tools/list`,
`tools/call`) came back `UnansweredRequest`.

**Confirmed independent of MCPTracer,** by piping the same four messages
straight into the built server with no proxy involved and closing stdin after
a fixed delay, ten runs per delay:

| stdin closed after | responses received (want 3), 10 runs each |
| --- | --- |
| 0 ms | 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 |
| 5 ms | 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 |
| 20 ms | 3, 3, 3, 3, 3, 3, 3, 3, 3, 3 |
| 50 ms, 100 ms, 200 ms | 3 on every run |

That is a threshold, not a flake. It is deliberate SDK behavior:
`internal/jsonrpc2/conn.go` `readIncoming`, in `go-sdk@v1.8.0`, cancels every
incoming request still in flight once the reader returns an error, with the
comment that "with the reader gone we cannot receive cancellation
notifications, and likely cannot write a response either." This is the
opposite failure shape from `py-stdio`, which loses only the *last* response
and only some of the time.

**Disposition:** the harness's default client hands over the whole script and
closes stdin at once, which is not how a real MCP client behaves — a real
client keeps stdin open until it has its responses. The `go-stdio` cell sets
`interactive_client = true`, and `run_matrix.py` then sends the script, waits
until every request has a response, and only then closes stdin. It waits on
responses rather than sleeping, so a slow runner or a slow `go run` compile
cannot reintroduce the loss. No MCPTracer product code was touched. Because the
cell passes, it is `required = true`.

Two consequences worth stating rather than leaving implicit:

- MCPTracer forwards the client's EOF to the server as it should
  (forward-before-record). A *client* that closes stdin immediately after
  sending, wrapped by MCPTracer around a Go SDK server, would see the same
  missing responses it would see without MCPTracer.
- The same fire-and-EOF client may also be what triggers `py-stdio`'s loss of
  its last response. That was not re-tested here: `py-stdio` was not switched
  to the interactive client, and it stays `required = false` as documented above.

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
- **Hosted current-protocol TypeScript and Go HTTP cells** — both transports
  now pass locally on Windows alongside Python HTTP. Their hosted Linux/macOS
  behavior remains untested.
- **Go SDK on Linux and macOS** — wired into the compatibility job, but no hosted run at a revision containing the Go cell has completed.
- **Non-loopback / real network conditions** — every cell talks to
  `127.0.0.1`; no cell exercises latency, packet loss, or a genuinely remote
  target.
- **Legacy Go Streamable HTTP** — the v1.8.0 legacy cell remains stdio-only.
- **Additional real SDKs** (e.g. a Rust MCP SDK, or other Python/JS
  frameworks built on top of the reference SDKs) — only
  `@modelcontextprotocol/sdk`, `mcp`, and the Go SDK (stdio) are covered.

## Do not claim the milestone unconditionally

The 0.3 milestone's exit gate is "multiple real MCP implementations pass the
compatibility suite." Four of five cells pass fully on the one OS this pass
could actually run, satisfying that literally — but "not yet run on
Linux/macOS," "one Python transport has a disclosed non-blocking flake," and
the "what was not tested" list above are real, current gaps. Update this
document's results table, not just its prose, the next time the CI `compat`
job actually runs.


## Released Linux binary runtime floor — 2026-09-30

The published `v0.3.0-rc1` x86_64 GNU/Linux archive (SHA-256
`c7a3d8a6623bc825a15051fe64e0c978ee83ec2c0c52ab7c25b9be9dba3fdad4`) was
executed in Ubuntu 22.04 WSL with glibc 2.35. Download and checksum validation
passed; execution failed because the binary requires `GLIBC_2.39`. Therefore
the current release artifact does not support this Ubuntu 22.04 runtime. The
private release workflow is being corrected to build on Ubuntu 22.04 for both
architectures and to enforce a maximum imported GLIBC symbol version of 2.35.
That correction is source-only until hosted CI and a new release artifact are
built; it does not alter this result. The POSIX install test suite passed 8/8
cases locally in Ubuntu 22.04. This is distribution/runtime evidence, separate
from the MCP SDK interoperability rows above.
