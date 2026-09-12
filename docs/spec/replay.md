# Spec: Replay

Status: **shipped.** Depends on the session model
([`session-model.md`](session-model.md)). See
[`replay-plan.md`](replay-plan.md) for `--plan`, `--allow-tool`, and
`--deny-tool`.

## Goal

Re-run the client side of a recorded session against a (possibly changed) MCP
server, and capture the result as a **new** recorded session. This is the
foundation for regression testing: record a golden session once, replay it after
a server change, then `diff` the two.

```bash
mcptracer replay <session-id> -- <mcp-server-command> [args...]
mcptracer replay <session-id> --client codex -- python my_server.py
```

## Model

Replay is the inverse of `record`. Where `record` sits between a real client and
a real server and observes, `replay` **acts as the client**: it drives the
recorded client→server (`c2s`) request/notification stream into a fresh server
subprocess and records the whole new exchange as a new session (reusing the exact
same storage writer path as `record`).

```text
recorded session (source)
        |
        |  c2s messages, in seq order
        v
mcptracer replay  ---->  new MCP server subprocess (stdin)
        ^                        |
        |  s2c responses         |
        +------------------------+
        |
        v
   new recorded session (target)  --> stored via the same Store writer thread
```

## What gets replayed

Drive only the messages the **client** originated in the source session:

- `c2s` **requests** (`initialize`, `tools/list`, `tools/call`, ...).
- `c2s` **notifications** (`notifications/initialized`, ...).

Do **not** replay `s2c` messages — those are what the new server will produce
fresh. Do **not** replay `c2s` **responses** to server-initiated requests in v1
(server-initiated requests during replay are an open problem; see below).

## Ordering, timing, and the handshake

- **Order** by source `seq`. Always send `initialize` first and wait for its
  response before sending anything else (per MCP lifecycle), regardless of source
  ordering quirks.
- **Timing modes** (flag `--timing`):
  - `fast` (default): send the next client message as soon as the previous
    correlated response arrives (or immediately, for notifications).
  - `realtime`: reproduce the original inter-message gaps using the source
    `ts_ns` deltas. Useful for latency-sensitive repros.
- **Correlation while replaying:** use the model's matching rule to know when a
  request has been answered before sending the next dependent request. A simple,
  correct v1: send request, block until the response with the matching
  `(direction, rpc_id)` arrives or a per-request timeout fires.

## Id handling

Reuse the **original rpc ids** from the source stream. Since replay is a fresh
session with a fresh server, there is no id-collision risk with a concurrent real
client. Keep ids identical so `diff` can align exchanges by id + method + call
order. (If a future need arises to remap, do it behind a documented flag; not v1.)

## Timeouts and failure

- `--request-timeout <ms>` (default e.g. 30000): if a replayed request is not
  answered in time, record the exchange as `Unanswered` in the target session and
  continue (do not hang). Emit a `warn!` to stderr.
- If the server process exits early, stop sending, close the target session, and
  exit non-zero with a clear stderr message. Partial sessions are still recorded.
- **stdout stays clean** during replay just like record — the target server's
  stdout is protocol traffic being captured, not printed. Progress/logs to stderr.

## Redaction interaction

- The source session may already be redacted. Replaying a redacted `tools/call`
  will send `***REDACTED***` as an argument value, which is usually **not** what
  you want for a faithful replay — the live target sees the literal placeholder
  string, not the original secret, which is a faithfulness problem (wrong data
  sent to a real server), not just a confidentiality one. v1 behavior: **replay
  uses the stored payload as-is**. Before driving any traffic, it scans the
  driven messages for the `***REDACTED***` placeholder (not just the session's
  recorded policy, since a `custom` policy may only mask specific keys) and, if
  found, prints an unmissable stderr warning naming every affected method/tool
  call and its count, alongside the side-effects warning. It does not block the
  run. `--i-understand-side-effects` suppresses both warnings; `bench` has the
  same detection and warning, repeated per iteration count.
- The target session gets its **own** `--redact` flag (default `none`), applied
  to what we newly record — same semantics as `record`.

Open follow-up (separate spec/task): a `record --keep-original` mode or a
sidecar unredacted store so redacted sessions remain replayable. Not v1.

## Open problems (call out, do not silently mishandle)

1. **Server-initiated requests during replay** (`sampling/createMessage`, etc.).
   The new server may ask the "client" (us) something the source never answered,
   or answered differently. v1: respond with a JSON-RPC error
   (`-32601`/"method not supported by mcptracer replay") and record it, OR replay
   the source's `c2s` response if one exists with the same id. Pick one, document
   it, and test it. **Recommended v1:** replay the source `c2s` response if
   present; otherwise error. Flag `--strict-server-requests` to make an
   unmatched server request abort instead.
2. **Non-deterministic tool arguments** (timestamps, uuids embedded by the
   original client). Out of scope for replay; it faithfully re-sends what was
   recorded. `diff` is responsible for normalizing noise.

## Reuse, not duplication

`replay` must reuse:

- `mcptracer-storage::Store` and the **single storage writer thread** pattern from
  `record.rs` (do not open a second connection across tasks).
- `mcptracer-protocol` framing (`encode_stdio_frame`, `parse_stdio_frame`).
- The redaction wiring (`Redactor` in the writer thread).

Factor the shared subprocess+writer machinery out of `commands/record.rs` into a
small internal module (e.g. `proxy::session_writer`) so both `record` and
`replay` use it. This refactor is itself a tracked task and must keep all
existing tests green.

## Acceptance (integration)

Add to `tests/test_proxy_integration.py`:
1. Record a session against `fake_mcp_server.py`.
2. `replay` it against the same fake server into a new db/session.
3. Assert the new session contains the same methods/tools in the same order and
   that `initialize` preceded `tools/call`.
