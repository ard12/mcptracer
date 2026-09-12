# Spec: `mcptracer sessions export-otel`

Status: **shipped, v1 scope (file output only).** Depends on the correlated
session model ([`session-model.md`](session-model.md), T-11).

## Goal

Export a correlated session as OTLP/JSON spans so it can be loaded into an
Elastic/Grafana-class observability backend — interop, not competition. File
output first (`--out spans.json`); an OTLP/HTTP push exporter is future
work, not in this task's scope.

```bash
mcptracer sessions export-otel <session-id> --out spans.json
```

## Redaction

Refuses a session recorded with redaction policy `none` unless
`--allow-unredacted` is passed, mirroring `.mtrace` export — the project's
hard rule is that no export/sharing feature ships without a redaction
policy in front of it. In the current v1 mapping this is defense-in-depth
rather than a response to anything unsafe today: spans carry only method
names, tool names, direction, and error codes, never request/response
bodies. It keeps the policy story consistent if a future version adds
richer span attributes.

## Mapping (best-effort OTel GenAI semantic conventions)

One `resourceSpans` document per session (the standard OTLP/JSON file-export
shape — a single JSON object, not NDJSON):

- **Resource attributes**: `service.name` (the recording client, e.g.
  `codex`), `mcp.session.id`.
- **One span per correlated exchange** (client- or server-initiated;
  orphan responses are excluded — they have no method/tool identity to
  name a span with).
  - `name`: `"{method} {tool}"` when both are known (e.g.
    `"tools/call echo"`), else just the method.
  - `kind`: `SPAN_KIND_CLIENT` (3).
  - `startTimeUnixNano` / `endTimeUnixNano`: the exchange's request/response
    capture timestamps.
  - `attributes`: `gen_ai.operation.name` (the JSON-RPC method),
    `gen_ai.tool.name` (when the exchange is a tool call),
    `mcp.direction` (`client_to_server` | `server_to_client`),
    `error.type` (the JSON-RPC error code, when present).
  - `status.code`: `STATUS_CODE_OK` (1) for a successful exchange,
    `STATUS_CODE_ERROR` (2) for an error response or an unanswered request.

## Determinism

`traceId` (16 bytes) is SHA-256-derived from the session id; each span's
`spanId` (8 bytes) is SHA-256-derived from the trace id and the exchange's
position. The same session always produces byte-identical output — no
random ids, so exports are diffable and safe to commit as CI fixtures.
