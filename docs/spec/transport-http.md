# Streamable HTTP Recording

## Status

Implemented contract for backlog task T-31, extended by T-70 (legacy session
partitioning) and T-92A (explicit stateless trace grouping for MCP
2026-07-28 — see below).

## Purpose

Add a local reverse proxy that records MCP Streamable HTTP traffic while keeping
the existing stdio recorder unchanged. The proxy is transport plumbing only: it
does not authenticate users, create MCP sessions, retry requests, or interpret
tool payloads beyond the existing JSON-RPC extraction used for recording.

The original contract follows MCP 2025-06-18. The reverse proxy now also
preserves modern 2026-07-28 traffic and can group caller-correlated stateless
requests; the exact shipped and deferred surface is documented in
[`mcp-2026-07-28.md`](mcp-2026-07-28.md). It deliberately excludes the legacy
HTTP-plus-SSE transport.

## Command Surface

```
mcptracer record-http --listen 127.0.0.1:8787 --target https://server.example/mcp --client codex --redact default
```

The proxy accepts HTTP requests on `--listen` and forwards them to the
configured `--target` base URL. It creates one MCPTracer recording session per
logical Streamable HTTP session observed during the proxy's lifetime, not one
session for the whole process — see Session Partitioning below.

`--listen` defaults to a loopback address. Non-loopback listeners require an
explicit `--allow-non-loopback` acknowledgement because forwarding client
authorization headers exposes the local proxy to the network.

The session's `transport` is `streamable-http`. Its server metadata contains a
sanitized target origin and path: it omits URL user info, query parameters, and
fragments. It must never record bearer tokens or target URL query secrets as
session metadata.

For modern stateless traffic, repeat a caller-owned trace header across related
requests and name it with `--group-stateless-by-header <HEADER>`. Its value is
held only in process memory for correlation and is not stored or printed. With
no configured header (or when it is absent), the safe default remains one
recording per header-less request.

## Protocol Contract

The proxy supports the Streamable HTTP endpoint shape:

- Forward `POST`, `GET`, and `DELETE` to the target endpoint.
- Preserve the inbound method, relative path, query string, status code, and
  end-to-end HTTP headers. Remove hop-by-hop headers when crossing the proxy
  boundary (`connection`, `keep-alive`, `proxy-authenticate`,
  `proxy-authorization`, `te`, `trailer`, `transfer-encoding`, and `upgrade`).
- Do not synthesize, remove, or reinterpret MCP headers, including
  `Mcp-Session-Id`, `MCP-Protocol-Version`, `Last-Event-ID`, `Mcp-Method`,
  `Mcp-Name`, and authorization headers. The upstream server remains the
  authority for session creation and protocol negotiation.
- Forward every accepted byte response to the caller even when parsing,
  redaction, or storage fails. Recording failure is reported on stderr and
  cannot change HTTP response status, headers, or body bytes.
- Stream response bodies to the caller. The proxy may buffer a bounded request
  body because `POST` JSON-RPC messages are individual JSON values. It must not
  buffer an entire unbounded SSE response before beginning forwarding.

The proxy accepts the standard request and response forms, without attempting
to impose its own interpretation:

- A `POST` may carry one JSON-RPC message and asks for `application/json`,
  `text/event-stream`, or both through `Accept`.
- A `POST` JSON response can be a JSON-RPC result/error message. A successful
  notification response can be `202 Accepted` without a body.
- A `POST` SSE response and a `GET` SSE response contain JSON-RPC messages in
  `data:` fields. Comments, `id:`, `event:`, and retry fields are forwarded but
  not treated as MCP messages.
- `DELETE` requests and non-JSON error bodies are forwarded but do not produce
  MCP message rows. A `DELETE` carrying `Mcp-Session-Id` still has recording
  significance: it finalizes that logical session (see Session Partitioning).

The initial implementation does not reconnect an SSE stream, resume an SSE
stream, upgrade a legacy endpoint, translate between transport versions, or
perform OAuth. Those behaviors belong to the client and server, not the
recorder.

## Session Partitioning (T-70)

A single `record-http` process can observe multiple concurrent logical MCP
sessions — a stateful upstream server issues a distinct `Mcp-Session-Id` per
client, and two clients may independently choose colliding JSON-RPC ids.
Recording everything into one MCPTracer session would let `correlate()` pair a
request from one logical session with a response from another whenever their
ids collide. The proxy instead partitions recordings per logical session:

- **Key resolution is request-header-driven only.** Every incoming request is
  classified before any bytes are forwarded, using only its own
  `Mcp-Session-Id` header — never a response header, and never prior request
  history:
  - Present and non-empty: `Established(id)`. All requests carrying the same
    id share one MCPTracer session, created on first use and kept open across
    requests.
  - No session id, but a non-empty value for the explicitly configured
    `--group-stateless-by-header`: `StatelessGroup(value)`. The value remains
    in memory and is not logged; all matching requests share one recording.
  - Neither identity: `Provisional(n)`, a fresh, unique key for that one
    request. This covers a legacy `initialize` exchange and modern requests
    where the caller supplied no explicit trace boundary. Two such requests
    never share a session, because nothing visible to the proxy verifies they
    belong together.
- **No response-driven promotion.** If a server assigns `Mcp-Session-Id` in
  its `initialize` response, that response is still recorded under the same
  `Provisional` session as its request; the *next* request that carries the
  new id opens a new `Established` session. Deferring session creation until
  a response header is known was considered and rejected: response bodies are
  streamed to the caller as they arrive, so the proxy cannot block returning
  the response to first decide where to file it without breaking the
  stream-immediately requirement above. The tradeoff is an initialize
  handshake recorded as its own short-lived session rather than merged into
  the session that follows it — never a session boundary that groups
  unrelated traffic.
- **Provisional sessions close after their one exchange.** Since a
  `Provisional` key is never reused, its session is finalized as soon as its
  request and (if any) response capture complete. Leaving it open until
  process shutdown would leak one storage-writer thread and SQLite connection
  per header-less request.
- **`DELETE` closes an established session.** A `DELETE` carrying an
  `Mcp-Session-Id` finalizes that session immediately after the exchange
  completes, matching the client's explicit termination signal in the
  Streamable HTTP spec. An established session the client never `DELETE`s is
  finalized when the proxy process shuts down.
- **`DELETE` can close an explicit stateless group.** A DELETE carrying the
  configured correlation header finalizes that local recording after the
  upstream response is forwarded. Modern servers normally return `405`; the
  proxy does not reinterpret that response. Graceful shutdown also closes all
  remaining groups.
- **No storage or artifact contract changes.** This only changes how many
  times the existing `create_session`/`close_session` calls are made per
  process invocation. The session/exchange data model, `.mtrace` format, and
  `correlate()` semantics are unchanged; each resulting session round-trips
  through them exactly as any other recorded session does.

## Capture Model

The protocol crate owns the transport-neutral recording abstraction:

```rust
pub trait Transport {
    fn name(&self) -> &'static str;
    fn decode_message(
        &self,
        bytes: &[u8],
        direction: Direction,
        seq: u64,
        timestamp_ns: i64,
    ) -> Result<McpMessage, TransportDecodeError>;
}
```

`StdioTransport` and `StreamableHttpTransport` both use the existing JSON-RPC
field extraction after their respective framing layers have yielded a complete
JSON value. The abstraction prevents HTTP framing and JSON-RPC extraction from
leaking into the proxy command. It does not introduce a shared async storage
connection or place new work in the stdio forwarding loop.

Each decoded message is written through the existing bounded storage writer:

- An inbound `POST` JSON-RPC body is captured as `c2s` after upstream request
  forwarding begins.
- A JSON response body is captured as `s2c` after downstream response
  forwarding begins.
- Each complete SSE `data:` event that parses as a JSON-RPC message is captured
  as `s2c`, in arrival order.
- The raw payload passed to storage uses the existing redactor. Derived fields
  and fact extraction therefore receive the same redacted representation as
  stdio recordings.

Request and individual SSE event capture are capped at 8 MiB. When a body or
event exceeds the cap, forwarding continues, the event is not recorded, and a
bounded dropped-message counter is incremented. This prevents a hostile
upstream from forcing unbounded memory use.

## Ordering and Concurrency

Every successfully decoded message receives a process-wide monotonic sequence
number before it enters the storage queue. A Streamable HTTP proxy can have
concurrent HTTP requests and SSE streams, so sequence order represents the
order MCPTracer observed complete messages, not a causal order guaranteed by
the transport. Session correlation must continue to use JSON-RPC ids and
timestamps.

The storage writer remains the only owner of its SQLite connection. HTTP
connection tasks communicate with it only through the bounded queue, exactly
as the stdio recorder does.

## Error Handling

Malformed JSON, malformed SSE data, queue saturation, redaction errors, and
SQLite errors are recording failures. They are logged to stderr with request
context and the proxy continues forwarding. Connection failures before an
upstream response is available are returned to the local caller as `502 Bad
Gateway`; no synthetic MCP JSON-RPC error is emitted.

The proxy must validate its own command arguments and fail before binding when
the target is not an absolute `http` or `https` URL. It must reject target
paths that would escape the configured target origin.

## Tests

The implementation is accepted only with a local fake Streamable HTTP server
fixture that proves:

1. `POST` request and JSON response messages are recorded with directions and
   payloads while the client sees the expected status, headers, and bytes.
2. An SSE response is forwarded incrementally and its `data:` JSON-RPC events
   are recorded in order.
3. Session and protocol-version headers reach the upstream server unchanged.
4. A malformed or oversized capture candidate does not disrupt forwarding.
5. The existing stdio integration fixture remains green without modification
   to its forwarding behavior.
6. A header-less exchange, and two established sessions that reuse an
   identical JSON-RPC id, are recorded as three distinct sessions whose
   message rows never mix; a `DELETE` against an established session marks
   it closed (`ended_at` set) without waiting for process shutdown.

## References

- MCP Streamable HTTP transport, 2025-06-18:
  <https://modelcontextprotocol.io/specification/2025-06-18/basic/transports>
- MCP Streamable HTTP transport, 2026-07-28:
  <https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http>
