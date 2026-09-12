# Spec: Streamable HTTP Replay

## Status

**Shipped** (backlog T-75). `mcptracer replay-http` re-sends a recorded
Streamable HTTP session's client traffic against a live target. T-75's
other half — exercising real MCP SDK/server implementations and publishing
a compatibility matrix — turned out not to need anything this environment
lacked: CI already runs a three-OS matrix with network access, and neither
SDK install needs a repository-visibility change. See
[`compatibility-matrix.md`](compatibility-matrix.md) for results (`mcptracer
replay-http` itself is exercised there against both a real TypeScript and a
real Python SDK server). See [`transport-http.md`](transport-http.md) for
the recording half this depends on and [`replay-plan.md`](replay-plan.md)
for the driven-call-list logic this command shares with stdio `replay`.

## Goal

`replay` re-runs a recorded stdio session's client traffic against a
(possibly changed) server subprocess, producing a new session `diff` can
compare against the original. Nothing filled that role for a session
recorded with `record-http`. `replay-http` re-sends the same recorded
client-originated messages, this time as real HTTP requests against a live
`--target` endpoint:

```bash
mcptracer replay-http <session-id> \
  --target https://server.example/mcp \
  --header "Authorization: Bearer $TOKEN" \
  --i-understand-side-effects
```

## Runtime credentials

`record-http` never records `Authorization` or any session-identifying
header as payload data (see `transport-http.md`'s protocol contract — the
proxy forwards headers end-to-end but MCPTracer's storage layer only ever
sees message bodies). A recorded session therefore carries no credential to
replay with. Repeatable `--header "Name: value"` flags supply runtime
credentials separately, exactly the way a CI workflow would inject a secret
at run time rather than bake it into a fixture. A fixed set of headers
(`host`, `content-type`, `content-length`, `mcp-session-id`,
`mcp-protocol-version`, `mcp-method`, `mcp-name`, `connection`,
`transfer-encoding`) are reserved — `replay-http` manages or reserves them and
`--header` rejects an attempt to set one, case-insensitively.

## MCP 2026-07-28 standard routing headers

Modern source requests are recognized from their self-describing `_meta`
protocol-version field. `replay-http` reconstructs `MCP-Protocol-Version`,
`Mcp-Method`, and (where required) `Mcp-Name` directly from the body, including
the specification's Base64 sentinel encoding. It does not send a legacy
`Mcp-Session-Id` for such a request. See
[`mcp-2026-07-28.md`](mcp-2026-07-28.md) for the precise support boundary;
For modern `tools/call`, replay derives `Mcp-Param-*` from the matching tool in
the latest captured prior `tools/list` result and the recorded arguments. It
fails closed when that definition is absent or invalid, and ignores manually
supplied `Mcp-Param-*` values for those calls. For captured MRTR exchanges,
replay validates and reuses recorded `inputResponses`, echoes only the target's
live opaque `requestState`, and gives every retry a fresh JSON-RPC id. Captured
modern `subscriptions/listen` streams are replayed concurrently with subsequent
captured requests; their acknowledgment, filter, and tagged notifications are
validated before recording. This remains partial rather than full current-spec
conformance because cache and extension evidence rules are still pending.

## Session-id handshake

The original session's `Mcp-Session-Id` is likewise never recorded, so
`replay-http` cannot resend it. Instead it drives the same handshake a real
client would: the first request carries no `Mcp-Session-Id`, and if the
target's response assigns one (in an `Mcp-Session-Id` response header,
typically on the `initialize` response), that id is captured and attached to
every subsequent request for the rest of the run. If the target never
assigns one (a stateless server), replay proceeds without one, matching how
`record-http` itself never requires an upstream session id.

At the end of a run, if a session id was established, `replay-http` sends a
best-effort `DELETE` to the target carrying that id — the Streamable HTTP
client termination signal — so the target can clean up its side without
waiting for a timeout. A failed termination `DELETE` is logged, not fatal.

## What gets replayed

`replay-http` reuses the exact same driven-message selection stdio `replay`
uses (`mcptracer_model::plan::driven_messages` /
`tool_is_allowed`): every `c2s` request and notification from the source
session, in recorded order, with `initialize` forced first — `--allow-tool`
/ `--deny-tool` filter `tools/call` requests by name exactly as they do for
stdio `replay`. Each driven message's stored payload is sent verbatim as one
HTTP `POST` body to `--target`, with `Content-Type: application/json` and
`Accept: application/json, text/event-stream`.

Because the message model records only bodies, not the original request
path, every driven message in one run posts to the same `--target` URL —
there is no per-message path routing. A source session recorded against
several distinct upstream paths through one `record-http` proxy lifetime
would need one `replay-http` run per path.

## Response capture

The response to each `POST` is captured the same way `record-http` captures
it live:

- `application/json`: the body is read up to the same 8 MiB `MAX_FRAME_BYTES`
  cap `record-http` applies to captured bodies, then recorded as one `s2c`
  message. A body exceeding the cap is not buffered further — a hostile or
  misbehaving target cannot force unbounded memory use — and is not
  recorded; it counts as a dropped message (see below).
- `text/event-stream`: the body is decoded with the same `SseDecoder`
  `record-http` uses (`crates/mcptracer-proxy/src/sse.rs`, shared by both
  commands so they can never disagree about SSE framing), and each complete
  `data:` JSON event is recorded as its own `s2c` message in arrival order.
  The decoder applies the same per-event cap internally.
- Anything else, on a message that expected an answer (a request, not a
  notification): the run fails clearly — see below — rather than silently
  discarding a response MCPTracer cannot interpret.
- A notification's response is not required to carry an MCP envelope at
  all: `202 Accepted` with an empty body, or any non-JSON-RPC body, is
  treated as the expected shape, not a capture gap.
- A non-2xx status's error body is read under the same bounded-read helper
  before being truncated to 500 characters for the error message — reading
  it without a cap first would defeat the point of capping the happy path.

A driven message that does not complete (network error, non-2xx status, or
timeout) fails the whole run with a clear error rather than continuing
past a request the target rejected — replay-http does not have stdio
replay's per-request unanswered-and-move-on tolerance for a *rejected*
request, only for one that times out waiting for a JSON/SSE response, which
is logged as unanswered and the run continues to the next driven message.
A dropped capture (the oversized-body case above) does not abort mid-run
either, but it does fail the run's overall exit code once every driven
message has been sent: this is the existing, shared
`spawn_storage_writer`/`finish_storage_writer` "any capture loss is a hard
failure" contract stdio `replay` already has — a replay whose own recording
has a gap cannot produce evidence `require_healthy_session`-gated commands
would accept anyway, so failing the run's exit code at the end is more
honest than reporting success over an incomplete capture.

## Unsupported envelope failure

A `2xx` response to a request whose `Content-Type` is neither
`application/json` nor `text/event-stream` is a protocol-compliance failure,
not a soft warning: MCP Streamable HTTP requires one of those two envelopes
for a request response. `replay-http` aborts the run with an error naming
the offending content type and the request id, satisfying the "unsupported
envelope state fails clearly" requirement — a target the recording was
never designed against does not get to silently produce an unrecorded,
un-diffable gap in the replayed session.

## Partial-capture refusal

`replay-http` reuses the same `require_healthy_session` gate `diff`/`assert`
already use: a source session with any integrity issue (unanswered request,
orphan response, dropped messages, an unclosed capture, etc. — see
`crates/mcptracer-proxy/src/session_health.rs`) is refused outright, with a
pointer to `mcptracer validate` for details. Replay never proceeds from a
capture MCPTracer cannot vouch for.

## Subscription scope boundary

For modern `subscriptions/listen`, replay opens the captured stream first, waits
for its required acknowledgment, then executes subsequent captured client
requests while the stream remains active. The stream accepts only the captured
core filter fields (`toolsListChanged`, `promptsListChanged`,
`resourcesListChanged`, and `resourceSubscriptions`); every notification must
carry the captured listen request's subscription id and be in that filter.
Replay waits for exactly the number of captured stream notifications before
closing the target response stream, which is the HTTP cancellation signal.

This is causal replay of captured subscription evidence, not a subscription
client or a source of new change events. It does not replay legacy standalone
`GET` SSE streams, invent notifications after the source trace ends, or support
extension-defined subscription filters. The source recording must contain its
acknowledgment and every expected notification frame; otherwise replay refuses
it rather than guessing stream state.

## Redaction

`--redact`/`--redact-keys` apply to the **new** replayed session's stored
payloads, identically to stdio `replay` — the bytes actually sent over the
wire are never modified by redaction. `--i-understand-side-effects` silences
the same two warnings stdio `replay` prints (real side effects re-executed
against a live target; a `***REDACTED***` placeholder sent verbatim if the
source session was itself recorded with redaction).

## Tests

- `crates/mcptracer-proxy/src/commands/replay_http.rs` unit tests: `--header`
  parsing (splitting, trimming, rejecting a missing colon, rejecting
  reserved headers case-insensitively), request-header construction (extra
  headers plus the established session id are attached; the session id is
  omitted before the handshake establishes one), and the response-body
  truncation helper used in error messages.
- `tests/test_proxy_integration.py::test_replay_http_reproduces_json_and_sse_sessions_and_fails_clearly_on_bad_envelope`:
  records a JSON exchange and an SSE-echo exchange through `record-http`
  against the fake Streamable HTTP server fixture, replays each directly
  against the fixture with `replay-http`, and asserts `diff --ignore-latency`
  reports no meaningful differences for both; then replays the same source
  session against a fixture route that returns `text/plain` and asserts the
  run fails with the unsupported-envelope error.
- `tests/test_proxy_integration.py::test_replay_http_caps_oversized_json_response`:
  replays a source session against a fixture route that returns a JSON body
  one byte past the 8 MiB cap, and asserts the outgoing request is still
  recorded, the oversized response is not, and the run's exit code reflects
  the resulting capture loss (`total_messages`/`dropped_messages` checked
  directly against the database, not just process exit code).
- `tests/test_proxy_integration.py::test_modern_streamable_http_records_grouped_stateless_calls_and_replays_required_headers`:
  records two correlated, sessionless 2026-07-28 requests, validates the
  capture, replays it with standard routing headers derived from `_meta`, and
  proves the source and replayed sessions diff cleanly.
