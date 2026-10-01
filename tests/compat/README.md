# MCPTracer real-SDK compatibility matrix

Every other test in this repository validates MCPTracer against fixtures
MCPTracer's own author wrote (`tests/fake_mcp_server.py`,
`tests/fake_mcp_http_server.py`, the `examples/*/server.py` demos). This
directory is different: each server here is a real, independently-maintained
MCP SDK, so this is the first independent input MCPTracer has ever been
tested against.

See `docs/spec/compatibility-matrix.md` for published results and
`docs/architecture/roadmap.md` for how this closes T-75's deferred half.

## Layout

```
tests/compat/
  matrix.toml            cell definitions (sdk x transport)
  run_matrix.py           the driver; exits nonzero if any required cell fails
  client_scripts/
    baseline.jsonl        legacy initialize -> initialized -> tools/list -> tools/call
    2026-07-28.jsonl        stateless server/discover -> tools/list -> tools/call (Go stdio)
    2026-07-28-python.jsonl  current-protocol sequence (Python v2 stdio/HTTP)
    2026-07-28-typescript.jsonl current-protocol sequence (TypeScript v2 stdio)
    2026-07-28-go-http.jsonl current-protocol HTTP sequence (Go SDK)
    2026-07-28-typescript-http.jsonl current-protocol HTTP sequence (TypeScript SDK)
  servers/
    ts-stdio/              @modelcontextprotocol/sdk (TypeScript), stdio
    py-stdio/               mcp (Python, FastMCP), stdio -- required = false,
                             see matrix.toml's comment: a diagnosed SDK-side
                             flake, independent of MCPTracer
    ts-http/                @modelcontextprotocol/sdk (TypeScript), Streamable HTTP
    py-http/                mcp (Python, FastMCP), Streamable HTTP
    go-stdio/               modelcontextprotocol/go-sdk (Go), legacy stdio -- needs
                             `interactive_client = true`, see below
    go-modern-stdio/        modelcontextprotocol/go-sdk (Go), 2026-07-28 stdio
    go-modern-http/         modelcontextprotocol/go-sdk (Go), 2026-07-28
                             stateless Streamable HTTP
    py-modern-stdio/        mcp (Python SDK v2), 2026-07-28 stdio
    py-modern-http/         mcp (Python SDK v2), 2026-07-28 Streamable HTTP
    ts-modern-stdio/       @modelcontextprotocol/server (TypeScript SDK v2),
                             2026-07-28 stdio
    ts-modern-http/        @modelcontextprotocol/server (TypeScript SDK v2),
                             2026-07-28 stateless Streamable HTTP
      server.js / server.py / main.go    the SDK server
      pin.toml                 pinned tools_pinned hash for the trusted contract
      package.json / requirements.txt   dependency manifest for that one server
```

The legacy Python servers (`py-stdio` and `py-http`) additionally need their SDK
installed before running locally (CI does this automatically):

```bash
pip install -r tests/compat/servers/py-stdio/requirements.txt
```

(`py-http`'s `requirements.txt` pins the identical version, so one `pip
install` covers both if they share an interpreter.)

The Go server needs the Go toolchain (1.25+, the SDK's own minimum); `go run .`
fetches the pinned SDK on first use. CI runs `go mod download` and `go vet ./...`
for each Go module. The current-protocol Go HTTP server sets `Stateless: true`
as required by SDK v1.7.0 for protocol `2026-07-28`. The Python v2 cells use
a separate virtual environment because they pin `mcp==2.2.0` while the legacy
Python cells pin `mcp==1.29.0`; CI selects the interpreter through
`MCPTRACER_COMPAT_PY_V2`. Each TypeScript v2 cell has its own lockfile and
CI uses `npm ci`; the HTTP cell pins `@modelcontextprotocol/node@2.1.0`
with `@modelcontextprotocol/server@2.2.0`.

A cell may select a protocol-era-specific script with `client_script`; the
legacy lane remains the default. By default the harness hands `record` the whole
client script and hits stdin EOF at once. A cell can set `interactive_client = true` in `matrix.toml` to make
the harness behave like a real MCP client instead: send the script, wait until
every request has a response, then close stdin (it waits on responses, not a
timer). `go-stdio` needs this because the Go SDK cancels in-flight requests on
stdin EOF; the evidence is in `docs/spec/compatibility-matrix.md`.

Each server exposes one tool, `send_email`, whose **declared description**
(never its call/response text) flips when `MCPTRACER_COMPAT_RUGPULL=1` is
set — the same invisible-on-the-wire contract change
`examples/rug-pull-demo/server.py` demonstrates. That makes every cell's run
both an interop check and a real end-to-end product test: if
`diff`/`assert --spec pin.toml` can't catch a rug pull against a real SDK,
the product's central claim doesn't hold up outside fixtures it controls.

## Running one cell locally

```bash
# Node/npm and the SDK dependency must be installed once, per server:
cd tests/compat/servers/ts-stdio && npm install && cd -

# Point at a locally built binary, or leave MCPTRACER_BIN unset to use
# whatever `mcptracer` resolves to on PATH:
MCPTRACER_BIN=/path/to/target/release/mcptracer \
  python tests/compat/run_matrix.py --only ts-stdio
```

Each cell runs the full six-step evidence chain against a **temp DB in a temp
directory** — it never touches your real `~/.mcptracer/sessions.db`.

1. `record` (stdio) or drive real HTTP requests through `record-http` (HTTP
   transport) a trusted baseline against the unmodified server
2. `validate` the capture
3. `assert --spec pin.toml` — must PASS
4. `export` + `assert --manifest` + `verify` offline — must MATCH
5. `replay`/`replay-http` against a fresh server instance, `diff
   --ignore-latency` against the baseline — must find no meaningful
   differences (the real compatibility test: the SDK is driven twice,
   independently)
6. re-record with `MCPTRACER_COMPAT_RUGPULL=1`; `diff` and
   `assert --spec pin.toml` against the rug-pulled candidate must both exit 1

### HTTP cells: transport-specific session boundaries

`record-http` proxies rather than spawns, so an HTTP cell drives real HTTP
`POST` requests through the proxy (`run_matrix.py` uses stdlib `http.client`,
with no new harness dependency).

- **Legacy HTTP cells:** after the SDK assigns `Mcp-Session-Id`, the harness
  sends a terminating `DELETE`; this is the transport's session-termination
  mechanism and closes that recording immediately. The initial `initialize`
  request necessarily has no session id, so T-70 records it in a provisional
  session separate from later traffic. The harness identifies and merges that
  pair before the evidence chain. This is deliberate logical-session
  partitioning.
- **Current-protocol stateless HTTP:** each request has no session id. The
  modern Python cell supplies one unique synthetic trace header per recording
  and configures that header with `--group-stateless-by-header`. It closes the
  group through graceful proxy shutdown. It does not send a transport DELETE:
  a server's response to a bodyless DELETE has no matching JSON-RPC request and
  may create an orphan response in the trace. A modern group is selected by its
  `server/discover` request; it has no `initialize` provisional session to
  merge.

Both paths exercise the actual HTTP proxy and replay target, and each cell
validates the completed recording before artifact and rug-pull checks.

## Running the whole matrix

```bash
python tests/compat/run_matrix.py            # every cell marked required = true
python tests/compat/run_matrix.py --all       # include non-required cells too
```

Exits nonzero if any *required* cell fails a step. A `required = false` cell
still runs and reports, so a known, diagnosed SDK difference stays visible
without blocking CI — see `matrix.toml`'s comment for when to use it, and
never as a way to silently drop a failing cell from consideration.

## Bootstrapping a new cell's `pin.toml`

Same procedure `examples/rug-pull-demo/pin.toml` used: record a trusted
session against the unmodified server, then run `assert --spec` with an
**empty** `hashes` table. The failure message prints the tool's real hash to
copy into a real `pin.toml`:

```toml
[[assert]]
kind = "tools_pinned"
hashes = {}
```

## A failure here is a finding, not a blocker

If a step fails against a real SDK, that is new information MCPTracer has
never had before — it might be a real MCPTracer bug (the hand-rolled fakes
never exercised the code path), a harness bug, or a legitimate SDK
difference that just needs documenting. **Do not edit product code to make a
cell go green before you can say which of the three it is.** Record the
failure, diagnose it, and only then decide whether product code changes.

Known likely first-failure candidates (see the plan this matrix implements):
protocol-version negotiation, notifications the fakes never send
(`notifications/cancelled`, progress, logging), richer capability/tool-list
shapes (`title`, `outputSchema`, `annotations`, `_meta`), CRLF vs LF stdio
framing on Windows, server-initiated requests (`replay
--strict-server-requests` exists for exactly this), and exit/shutdown
handling differences.

## Zero external Python dependencies

`run_matrix.py` uses only the standard library (`tomllib`, stdlib since
Python 3.11) — the same discipline `tests/schema_validator.py` was written to
keep. Do not add one for this harness either.
## Real Python SDK stdio cancellation smoke

The standalone run_python_v2_cancellation.py exercises SDK-driven
notifications/cancelled with a long-running v2.2.0 server tool. It captures
and replays through MCPTracer, checks both sessions cancellation status and
health, and requires a clean diff. Set MCPTRACER_BIN,
MCPTRACER_COMPAT_PY_V2 (Python executable), and
MCPTRACER_COMPAT_PY_V2_SITE (the pinned v2 environment site-packages on
Windows), then run python tests/compat/run_python_v2_cancellation.py.

This focused smoke supplements the matrix and is registered in the three-OS
compatibility job. Registration is not proof that a particular revision passed.
See `docs/spec/compatibility-matrix.md` for protocol and transport scope limits.

## Real Go SDK stdio cancellation smoke

The isolated Go module under `servers/go-cancellation` drives the pinned Go SDK v1.7.0 as both client and server. The client cancels a progress-reporting tool through `CommandTransport`, then pings once so cancellation cleanup completes before teardown; the server waits a bounded 25 ms for the SDK preemptor to retire the cancelled request. The smoke verifies captured notification/status, replay against a fresh SDK server, session health, and clean diff. Build the CLI, set `MCPTRACER_BIN` to its absolute path, then run `go run .` from that module directory. CI registers this smoke on all three compatibility operating systems. See `docs/spec/compatibility-matrix.md` for scope limits.

## Real TypeScript SDK v2 cancellation smoke

The isolated `servers/ts-cancellation` package pins the official TypeScript client and server SDKs at 2.2.0. Its client aborts a progress-reporting stdio call through MCPTracer; the following SDK ping keeps the connection alive while the server handles cancellation. The smoke verifies captured notification/status, replay against a fresh SDK server, session health, and clean diff. Run `npm ci`, build MCPTracer, set `MCPTRACER_BIN`, and run `npm run smoke` from the package directory. CI registers it on all three compatibility operating systems. See `docs/spec/compatibility-matrix.md` for scope limits.
