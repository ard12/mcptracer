# Spec: Session / Correlation Model (the keystone)

Status: **shipped, frozen.** This is the data contract that `replay`, `diff`,
`assert`, and `.mtrace` all consume. Any deviation must be raised as a spec
change, not decided ad hoc.

## Goal

Given the ordered messages of one recorded session, produce a **correlated view**:
requests paired with their responses, latencies, statuses, standalone
notifications, and session-level aggregates. This turns a flat message log into
"what calls happened, in what order, how long they took, and which failed."

## Where it lives

**New crate: `crates/mcptracer-model`.**

- Depends on `mcptracer-protocol` (for `Direction`, `MessageKind`) and
  `mcptracer-storage` (for `StoredMessage`).
- Nothing depends *back* on it except `mcptracer-proxy` (CLI) and future
  `replay`/`diff` code. No cycles: `storage` must never depend on `model`.
- Rationale: correlation is neither framing (`protocol`) nor SQL (`storage`) nor
  subprocess IO (`proxy`). It is a distinct concern and gets its own owner, per
  the architecture rules in `CONTRIBUTING.md`.

Decision record for the four open questions:

1. **Crate placement:** new `mcptracer-model` crate (above).
2. **Computed-on-read vs. materialized table:** **computed on read** for now.
   `correlate(&[StoredMessage]) -> SessionModel` runs in memory. Do **not** add a
   `calls` table yet. Revisit only if profiling shows correlation is a
   bottleneck on large sessions (see "Performance" below).
3. **Id scoping:** correlation keys on **`(direction-of-responder, rpc_id)`**,
   never `rpc_id` alone. MCP is bidirectional; ids are per-originator. See
   "Why direction matters."
4. **Orphan policy:** orphans are represented **explicitly** as `Exchange`s with
   status `Unanswered` (request with no response) or as `OrphanResponse` (a
   response with no matching prior request). Never silently dropped.

## Background: the shape of stored data

From `mcptracer-storage`, `get_messages(session)` returns `Vec<StoredMessage>`
ordered by `seq ASC`. Each `StoredMessage` has:

| Field | Type | Notes |
| --- | --- | --- |
| `seq` | `u64` | Proxy-assigned monotonic order across both directions. |
| `ts_ns` | `i64` | Capture time at the proxy (Unix ns). |
| `direction` | `String` | `"c2s"` (client→server) or `"s2c"` (server→client). |
| `message_kind` | `String` | `"request"`, `"response"`, or `"notification"`. |
| `rpc_id` | `Option<String>` | **JSON-encoded** id: integer `1` is `"1"`, string `list-1` is `"\"list-1\""`. Compare these strings directly for id equality. |
| `method` | `Option<String>` | Present on requests and notifications. |
| `tool_name` | `Option<String>` | Present only for `tools/call`. |
| `payload` | `String` | Full JSON (redacted if the session used a policy). |
| `payload_bytes` | `usize` | Original wire size. |
| `is_error` | `bool` | Response carried a JSON-RPC `error`. |
| `error_code` | `Option<i64>` | JSON-RPC error code, if any. |

`message_kind` is derived at record time by `McpMessage::kind()`:
- **Request**: has both `id` and `method`.
- **Response**: has `id`, no `method`.
- **Notification**: has `method`, no `id`.

## Why direction matters (do not skip)

MCP is **bidirectional**. Both sides can originate requests:

- **Client-initiated** (the common case): client sends a request (`c2s`,
  kind `request`), server replies (`s2c`, kind `response`).
- **Server-initiated** (e.g. `sampling/createMessage`, `roots/list`,
  `elicitation/create`): server sends a request (`s2c`, kind `request`), client
  replies (`c2s`, kind `response`).

Therefore `rpc_id` is **not globally unique** within a session. A `c2s` message
with `id: 1` might be a *client request* OR a *client's response to a server
request*. `message_kind` distinguishes those, and the responder always sits in
the **opposite direction** from the requester. The matching rule below is uniform
across both cases precisely because it keys on direction + kind + id.

## Public API

```rust
// crates/mcptracer-model/src/lib.rs
use mcptracer_protocol::Direction;
use mcptracer_storage::StoredMessage;

/// Outcome of a single request/response exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeStatus {
    /// Request answered with a non-error response.
    Ok,
    /// Request answered with a JSON-RPC error response.
    Error,
    /// A `subscriptions/listen` request acknowledged by its long-lived stream.
    Subscribed,
    /// Request had no matching response in the session (pending at end,
    /// server crash, or a dropped record).
    Unanswered,
    /// A response with no matching prior request (protocol anomaly or a
    /// recording gap / dropped request record).
    OrphanResponse,
}

/// One correlated request/response pair (or an orphan on either side).
#[derive(Debug, Clone)]
pub struct Exchange {
    /// `seq` of the request message, if present.
    pub request_seq: Option<u64>,
    /// `seq` of the response message, if present.
    pub response_seq: Option<u64>,
    /// JSON-encoded rpc id (as stored). `None` only for malformed input.
    pub rpc_id: Option<String>,
    /// Direction of the *request* (who originated the call).
    /// `ClientToServer` for client-initiated, `ServerToClient` for
    /// server-initiated. For an `OrphanResponse`, this is the direction the
    /// request *would* have had (opposite the response).
    pub origin: Direction,
    pub method: Option<String>,
    pub tool_name: Option<String>,
    pub status: ExchangeStatus,
    /// `response_ts_ns - request_ts_ns`. `None` unless both are present.
    pub latency_ns: Option<i64>,
    pub error_code: Option<i64>,
    pub request_ts_ns: Option<i64>,
    pub response_ts_ns: Option<i64>,
}

/// A standalone notification (no id, no response).
#[derive(Debug, Clone)]
pub struct NotificationEvent {
    pub seq: u64,
    pub ts_ns: i64,
    pub direction: Direction,
    pub method: String,
}

/// The full correlated view of one session.
#[derive(Debug, Clone)]
pub struct SessionModel {
    /// Exchanges in request order (by `request_seq`, orphans by `response_seq`).
    pub exchanges: Vec<Exchange>,
    pub notifications: Vec<NotificationEvent>,
    pub stats: SessionStats,
}

/// Session-level aggregates. Latency percentiles use answered exchanges only.
#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    pub total_exchanges: usize,
    pub ok: usize,
    pub errors: usize,
    pub unanswered: usize,
    pub orphan_responses: usize,
    pub notifications: usize,
    pub latency_p50_ns: Option<i64>,
    pub latency_p95_ns: Option<i64>,
    pub latency_max_ns: Option<i64>,
    /// Count of calls per `tools/call` tool name, sorted by count desc then name.
    pub tool_call_counts: Vec<(String, usize)>,
}

/// Build the correlated view from an ordered message slice.
/// Input MUST be ordered by `seq ASC` (as `Store::get_messages` returns it).
pub fn correlate(messages: &[StoredMessage]) -> SessionModel;
```

## Correlation algorithm

Input is ordered by `seq ASC`. Single pass with a pending-request map.

```text
pending: HashMap<(Direction /* responder */, String /* rpc_id */), usize /* index into exchanges */>
exchanges: Vec<Exchange>
notifications: Vec<NotificationEvent>

for msg in messages (in seq order):
    dir = parse msg.direction            # "c2s" -> ClientToServer, "s2c" -> ServerToClient
    match msg.message_kind:

      "request":
          # responder sits in the opposite direction
          key = (opposite(dir), msg.rpc_id)
          push a new Exchange {
              request_seq: msg.seq, origin: dir, rpc_id, method, tool_name,
              status: Unanswered, request_ts_ns: msg.ts_ns, ...None
          }
          pending.insert(key, index_of_that_exchange)
          # If key already existed (duplicate in-flight id, same direction),
          # overwrite: the newer request claims the id. The displaced exchange
          # stays Unanswered. (See "Edge cases: id reuse".)

      "response":
          key = (dir, msg.rpc_id)          # responder direction == this msg's direction
          if let Some(idx) = pending.remove(key):
              e = &mut exchanges[idx]
              e.response_seq = msg.seq
              e.response_ts_ns = msg.ts_ns
              e.latency_ns = msg.ts_ns - e.request_ts_ns   # both present
              e.error_code = msg.error_code
              e.status = if msg.is_error { Error } else { Ok }
          else:
              # response with no matching request
              push Exchange {
                  response_seq: msg.seq, origin: opposite(dir), rpc_id,
                  status: OrphanResponse, response_ts_ns: msg.ts_ns,
                  error_code: msg.error_code, ...None
              }

      "notification":
          if dir == ServerToClient and method == "notifications/subscriptions/acknowledged":
              subscription_id = payload.params._meta["io.modelcontextprotocol/subscriptionId"]
              if subscription_id identifies a pending c2s subscriptions/listen request:
                  set that exchange's status to Subscribed (keep it pending for a later terminal response)
          push NotificationEvent { seq, ts_ns, dir, method (default "" if missing) }

# Anything left in `pending` is already recorded as Unanswered, except an
# acknowledged `subscriptions/listen` exchange, which is Subscribed.
# Sort exchanges by (request_seq or response_seq) ascending for stable output.
# Compute SessionStats.
```

`opposite(ClientToServer) = ServerToClient` and vice versa.

### Latency

`latency_ns = response_ts_ns - request_ts_ns`. These are **proxy capture
timestamps**, so latency includes a small, consistent proxy overhead. Document
this in the CLI help and `.mtrace` spec — it is a proxy-observed latency, not the
server's internal processing time. It is still valid for **relative** comparison
(diff) and thresholds (assert).

### Percentiles

Collect `latency_ns` from all `Ok`/`Error` exchanges (answered only). Sort. Use
the nearest-rank method: `p50 = sorted[ceil(0.50*n)-1]`, `p95 =
sorted[ceil(0.95*n)-1]`, clamped to valid indices. `None` when there are zero
answered exchanges. Keep this deterministic — tests assert exact values.

## Edge cases (all must have a test)

1. **Client-initiated happy path** — `c2s request id=1` then `s2c response id=1`
   → one `Ok` exchange, `origin = ClientToServer`, latency computed.
2. **Server-initiated call** — `s2c request id=1` then `c2s response id=1` → one
   exchange, `origin = ServerToClient`. Confirms direction scoping.
3. **Id collision across directions** — `c2s request id=1` AND `s2c request id=1`
   both open, then `s2c response id=1` and `c2s response id=1`. Must produce two
   distinct correctly-paired exchanges (the `s2c` response pairs the `c2s`
   request; the `c2s` response pairs the `s2c` request).
4. **Error response** — response with `is_error=true` → status `Error`,
   `error_code` populated.
5. **Unanswered request** — request with no response (e.g. session ends
   mid-flight) → status `Unanswered`, `latency_ns = None`.
6. **Orphan response** — response with no prior request (simulate a dropped
   request record) → one `OrphanResponse` exchange.
7. **Notification** — `notifications/initialized` (no id) → one
   `NotificationEvent`, zero exchanges.
8. **Acknowledged subscription** — a c2s `subscriptions/listen` request followed
   by s2c `notifications/subscriptions/acknowledged` carrying the same
   subscription id → one `Subscribed` exchange plus one `NotificationEvent`,
   without requiring a terminal response.
9. **Id reuse after completion** — `c2s req id=1`, `s2c resp id=1`, later
   `c2s req id=1` again, `s2c resp id=1` → two separate `Ok` exchanges (the map
   key is free again after the first pair completes).
10. **Duplicate in-flight id (same direction)** — `c2s req id=1`, `c2s req id=1`
   (no response between) → the second overwrites the pending entry; first stays
   `Unanswered`; a later `s2c resp id=1` pairs the second. Document as a
   best-effort rule for malformed streams.
10. **Empty session** — no messages → empty model, all stats zero, percentiles
    `None`.
11. **String vs. integer ids** — `id: "list-1"` (stored `"\"list-1\""`) and
    `id: 1` (stored `"1"`) never collide because their stored strings differ.

## Performance

`correlate` is O(n) time, O(k) space where k = peak in-flight requests. For the
target session sizes (thousands of messages) this is trivial. **Do not**
prematurely add a materialized `calls` table or SQL-side correlation. If a future
profiling task shows correlation dominating `sessions show`/`diff` on large
recordings, open a spec-change task to add an optional `calls` table populated at
`close_session` time — but only then.

## CLI surface (follow-on, separate task)

- `mcptracer sessions show <id> --calls` — render the correlated exchange list
  (a table: SEQ pair, ORIGIN, METHOD/TOOL, STATUS, LATENCY) instead of the raw
  message list.
- `--calls --json` — emit `SessionModel` as JSON (serialize the structs; derive
  `serde::Serialize`).

## Test matrix summary

Unit tests live in `crates/mcptracer-model/src/lib.rs`. Build `StoredMessage`
fixtures directly (they are plain structs). One test per numbered edge case
above, plus one aggregate-stats test asserting exact `p50`/`p95`/`tool_call_counts`.
An end-to-end assertion (correlate the real recorded fixture) belongs in
`tests/test_proxy_integration.py` only after the `--calls --json` surface exists.
