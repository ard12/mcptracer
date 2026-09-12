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
    baseline.jsonl        initialize -> initialized -> tools/list -> tools/call
  servers/
    ts-stdio/              @modelcontextprotocol/sdk (TypeScript), stdio
    py-stdio/               mcp (Python, FastMCP), stdio -- required = false,
                             see matrix.toml's comment: a diagnosed SDK-side
                             flake, independent of MCPTracer
    ts-http/                @modelcontextprotocol/sdk (TypeScript), Streamable HTTP
    py-http/                mcp (Python, FastMCP), Streamable HTTP
      server.js / server.py    the SDK server
      pin.toml                 pinned tools_pinned hash for the trusted contract
      package.json / requirements.txt   dependency manifest for that one server
```

The two Python servers additionally need their SDK installed before running
locally (CI does this automatically):

```bash
pip install -r tests/compat/servers/py-stdio/requirements.txt
```

(`py-http`'s `requirements.txt` pins the identical version, so one `pip
install` covers both if they share an interpreter.)

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

### HTTP cells: two extra steps stdio doesn't need

`record-http` proxies rather than spawns, so an HTTP cell drives real HTTP
`POST`/`DELETE` requests against it (`run_matrix.py`'s own client, stdlib
`http.client`, no new dependency) instead of piping a JSONL file:

- **The terminating `DELETE`.** The MCP Streamable HTTP transport's own
  session-termination mechanism, and the only way `record-http` marks a
  logical session "closed" (`ended_at_ns` set) right away rather than
  leaving it open. Skip it and `mcptracer validate` reports
  `SessionNotClosed` even though every message was captured correctly —
  found the hard way while building this harness.
- **The merge.** A client's first `initialize` request carries no
  `Mcp-Session-Id` header yet (the server hasn't assigned one), so T-70's
  logical-session partitioning records it as its own short-lived
  "provisional" session, separate from the session the rest of the traffic
  lands in once the real id is known — deliberate, documented behavior, not
  a bug. `latest_recorded_session()` in `run_matrix.py` detects the split
  (by checking which of the two most recent sessions contains the
  `initialize` request) and reassembles them with `mcptracer merge
  <initialize-session> <main-session>` before running the rest of the
  chain. A stdio cell never needs this, since a spawned subprocess only
  ever has one session for its whole lifetime.

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
