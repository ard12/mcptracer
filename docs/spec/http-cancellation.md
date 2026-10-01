# Streamable HTTP cancellation evidence - design draft

Status: design proposal; not implemented or approved (T-92).

## Problem

The final MCP 2026-07-28 specification cancels an in-flight Streamable HTTP
request by closing that request's response stream. This differs from stdio,
where the client sends notifications/cancelled. MCPTracer currently stores
decoded MCP messages only. Its model derives Cancelled from the stdio
notification, so an HTTP call whose response is closed before a JSON-RPC
result remains unanswered and fails the health gate. replay-http has no
recorded event telling it to close the target response stream.

The wire requirement is in the [MCP cancellation specification](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/docs/specification/2026-07-28/basic/patterns/cancellation.mdx).

## Proposal

Persist an observed HTTP response-stream termination as a transport event. Do
not synthesize a JSON-RPC notification: no such notification was sent on the
wire. Keep the event separate from MCP message sequence numbers and associate
it with the client-originated request that owned the POST response.

Candidate event fields are type = http_response_stream_closed, request_seq,
after_message_seq (optional), and ts_ns. The request sequence identifies the
request message. The optional after-message sequence identifies the final
fully decoded MCP response/notification frame observed before the close. The
event records observation only; it does not claim the caller's intent. Do not
infer it from an upstream error, malformed/truncated SSE frame, process
shutdown, or arbitrary request timeout.

Keep existing messages and their identities unchanged. Persist events in a
dedicated SQLite table under a new additive migration. Portable export needs a
new .mtrace version with an events array; v1 remains byte-shape compatible and
importable. Artifact identity must include events for the new version. Older
readers must reject the new version clearly rather than silently drop
cancellation evidence. Review the exact versioning and compatibility design
before implementation.

## Integrity and replay rules

- Accept a close event only for a Streamable HTTP session and an existing
  client-originated request with a JSON-RPC id.
- Require event ordering after the request and after every captured response
  frame linked to that request. Reject duplicate terminal events, a response
  after the close, and ambiguous event/request associations.
- Preserve ordinary unanswered requests as unhealthy. A timeout, server EOF,
  transport error, capture shutdown, or dropped message is not a cancellation
  event.
- Expose the observed termination source in machine-readable session output;
  do not silently turn every unanswered request into cancelled.
- During replay, read and compare the target response frames that precede the
  event, then close that one response stream. Do not wait for a final
  JSON-RPC result after the source trace records a close. Fail closed if the
  target produces a different frame sequence or completes early.
- Include the event in .mtrace validation, canonical identity, diff,
  export/import, and integrity checks.

## Required evidence

1. Storage/model tests cover valid association, ordering, duplicate events,
   response-after-close, and legacy sessions with unanswered calls.
2. .mtrace v1 import/export/digest regression remains unchanged; new-version
   vectors prove event identity and detect event removal/reordering.
3. A real supported SDK client cancels a progress-reporting HTTP tool through
   record-http; the SDK server observes request cancellation and the stored
   source session validates as explicitly transport-terminated.
4. replay-http repeats the call against a fresh SDK server, closes the target
   response stream at the captured frame boundary, and the replay session
   validates and diffs cleanly.
5. Negative controls prove that server-side EOF, client/network error, and
   capture shutdown do not become a cancellation event.
6. Run the required local workspace gates after implementation. Run the hosted
   exact-SHA matrix against a reviewed committed revision after required
   commit authorization.

## Open design checks

An HTTP server or intermediary may observe a disconnected consumer without
knowing whether it was an intentional protocol cancellation, a network loss,
or operator shutdown. The capture implementation must determine which signals
it can reliably distinguish on each supported platform. If it cannot
distinguish them, name the event as an observed response-stream termination
and keep user-intent claims out of the status and documentation.

The storage migration and .mtrace version affect persistent contracts. This
is a T-92 heavy slice and needs maintainer review before code changes. Until
the required event, integrity rules, and replay behavior are implemented and
tested, HTTP cancellation remains unsupported as a replayable cancelled
exchange.

## Design review update — 2026-09-30

The primary 2026-07-28 MCP specification explicitly says Streamable HTTP
cancellation is signaled by closing the request's SSE response stream, and the
server MUST treat a client disconnect as cancellation. See the
[transport-specific cancellation rule](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/docs/specification/2026-07-28/basic/patterns/cancellation.mdx#transport-specific-cancellation).
The proxy still must distinguish a downstream disconnect from its own shutdown
and from an upstream stream ending or erroring.

### Recommended event and ordering contract (maintainer review pending)

- Name the persisted fact `http_response_stream_aborted`: the proxy observed
the downstream response body dropped before the upstream stream completed.
This is a transport signal, not a claim that a human intentionally cancelled.
When it is reliably attributable to a live downstream disconnect, the model
may classify the exchange as `Cancelled` with a source value distinguishing
HTTP closure from stdio `notifications/cancelled`.
- Persist `event_seq` (monotonic per session), `request_seq` (the captured
client request message), `after_message_seq` (the session-wide message high
water mark after draining already captured response chunks), and `ts_ns`.
Require an existing c2s HTTP request with a JSON-RPC id. Reject duplicate
terminal events, sequence regressions, an event before its request, or a
response for that request after its event boundary.
- Emit only when the downstream response body is dropped before natural
upstream EOF while the proxy is serving the exchange. Natural EOF and upstream
errors are not cancellation events. A request timeout alone is not a signal.
Add a server-shutdown marker/token so local graceful shutdown cannot manufacture
an HTTP cancellation event; event persistence failure must make the capture
unhealthy instead of silently losing the terminal fact.
- Add the event table with an additive SQLite migration. Keep transport events
outside message counts/sequences and preserve the existing message payloads.
- Keep `.mtrace` v1 serialization and canonical identity unchanged for sessions
without events. Export eventless sessions as v1; export sessions with events as
v2 with a required, ordered `events` array. New readers accept v1 and v2;
older readers reject v2 by the existing version guard. V2 canonical identity
includes every event field and order; v1 golden bytes/digests remain unchanged.
- Replay the event only after all recorded response frames through
`after_message_seq` match. Drop the target response body at that point. A target
that finishes early, emits another frame, or diverges fails replay. The stored
source is healthy only when its capture accounting is complete and the event
is the unique terminal outcome for that request.

This resolves the local shape recommendation but not approval of a persistent
schema change. Before code, review shutdown signaling, event-writer ordering,
`.mtrace` v2 compatibility, and the model's public cancellation-source shape.
Keep HTTP cancellation labeled unsupported until those items and the required
SDK capture/replay and negative controls pass.

## Implementation-readiness review - 2026-09-30

Production applicability was checked before this private design review. Public
main is `2883dbf6a0ee08c5064b8ec4cab0d2a8947d75b7`; release
`v0.3.0-rc1` resolves to `ba610ca9914ad4aee39939678da3abb7e5511d16`. The public
and release `record_http.rs` paths have no persisted HTTP-close event; the
release `.mtrace` contract is version 1. The private tree is at
`9a737f8c458611ea0eab09342114d4f72e822db9`, with its existing dirty work.
These are separate source/artifact identities; this review changes no runtime
contract.

### Current code constraints

- `record-http` wraps `upstream.bytes_stream()` in an Axum `Body`. For JSON/SSE
  it `try_send`s cloned chunks into a bounded 64-item capture channel while
  forwarding the original chunk. `capture_response_body` later decodes those
  chunks; neither the stream nor capture task currently reports whether the
  body ended naturally, errored upstream, or was dropped by its downstream
  consumer.
- Request capture runs in `forward_request_body` as a separate task and does
  not return the captured request sequence to `proxy_request` or the response
  capture task. A cancellation event therefore needs explicit per-exchange
  correlation; requiring an existing c2s request ID cannot be met from the
  current `HttpSession` handle alone.
- `record_message` allocates a session sequence through `AtomicU64` and then
  calls nonblocking `try_record` on a bounded `sync_channel` with a byte
  budget. Sequence allocation and enqueue are not one serialized operation
  across concurrent HTTP capture tasks. Queue overflow is counted as dropped
  capture. A terminal event must not race already-decoded frames or be treated
  as healthy if either its event write or preceding frame writes are lost.
- The writer currently accepts `Message` and `Close` events, batches messages,
  and writes the batch before processing `Close`. A transport event needs an
  ordered writer event and a batch-flush boundary. The HTTP session registry
  has a global capture-task set, not a per-session in-flight lease; an explicit
  DELETE close can remove/finalize an established session while other requests
  for that logical session are still draining. The event/close lifecycle must
  define how that overlap is handled.
- `axum::serve(...).with_graceful_shutdown(...)` does not pass a shutdown
  marker into response-body capture. The event must be emitted only for an
  observed downstream body drop while serving, never for natural EOF or an
  upstream error. A process-shutdown token should suppress synthetic aborts.
  Since a network loss and an intentional client close are not always
  distinguishable, the persisted fact must remain an observed stream abort,
  not a claim of user intent.
- The current model derives `ExchangeStatus::Cancelled` from
  `notifications/cancelled` and has no cancellation-source field. Storage is
  schema version 5. `.mtrace` is strict version 1 with a messages-only
  top-level contract. HTTP cancellation therefore changes storage, model/API,
  integrity/diff, replay, and artifact contracts together.

### Recommended implementation shape (not approved)

Use an explicit body-stream terminal state (`natural_eof`, `upstream_error`,
`downstream_drop`, or `proxy_shutdown`) and a drop guard that signals the
existing capture task through a nonblocking terminal channel. The capture task
must drain and decode all already-forwarded chunks before it submits an abort
event. Limit candidate events to an attributable c2s request with an ID and an
SSE response; do not infer them from JSON response truncation, undecodable SSE,
request-body loss, timeout, or shutdown. Use a per-session admission sequencer
that assigns message sequence and enqueues the record atomically relative to
transport events and close; flush message batches at event boundaries. Make
close wait for or explicitly fail any in-flight capture leases. Any unpersisted
frame/event continues to make the session unhealthy.

Preserve `Cancelled` as the exchange outcome but expose an explicit source in
the versioned machine-readable model. Keep eventless `.mtrace` v1 bytes and
digests unchanged; eventful sessions use a required ordered event array in v2,
with the event included in canonical identity and older readers rejecting v2.
These are recommendations for review, not accepted schema/API decisions.

### Maintainer decisions required before code

1. Approve the persisted semantics/name: observed SSE response-body abort, with
   no assertion about user intent; confirm non-SSE bodies are excluded.
2. Approve the public model/API shape for `Cancelled` source and its schema
   compatibility/versioning.
3. Approve additive SQLite schema v6 and eventful-only `.mtrace` v2, including
   unchanged v1 serialization/digests and older-reader rejection.
4. Approve the shutdown/EOF/error classification and the per-session ordering
   and close barrier described above.
5. Approve fail-closed behavior for writer-queue/event persistence errors and
   the negative controls required before any conformance claim.

Until these decisions are reviewed, do not modify storage/model/artifact or
runtime code and keep HTTP cancellation marked unsupported. Once approved, the
implementation sequence is storage/event model and legacy vectors; ordered
capture plus shutdown/concurrency tests; event-aware health and diff; replay
body closure; then real-SDK capture/replay and EOF/error/shutdown/queue-failure
negative controls.

### Body-drop feasibility probe - 2026-09-30

A disposable local Axum 0.8.9/Tokio 1.52.3 probe confirms a custom
`Body::from_stream` wrapper's `Drop` guard observes a raw HTTP/1.1 downstream
socket close. The wrapper separately latched `natural_eof` when `poll_next`
returned `None` and `upstream_error` when it returned `Some(Err(_))`; all three
classifications passed. This supports the low-level observer shape only. It
does not identify intent, proxy shutdown, MCP request association, concurrent
close ordering, queue persistence, or replay. Reproduction and exact hashes:
`T-92 probe record` (private record, not distributed).
