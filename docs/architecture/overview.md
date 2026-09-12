# Architecture overview

MCPTracer turns captured MCP traffic into reproducible server tests and local
inspection artifacts. The forwarding path and the stored analysis copy have
separate responsibilities.

## Capture and storage

For stdio, the proxy runs the server as a subprocess and forwards traffic in
both directions. For Streamable HTTP, it acts as a local reverse proxy. The
protocol crate owns framing and message extraction. Forwarded bytes are not
rewritten by redaction or derived analysis.

The proxy sends recording events to a bounded storage queue. The storage
writer owns its SQLite connection. Recording failure must not stop forwarding;
dropped-message counts and capture validation make incomplete evidence visible.
Redaction applies to stored payloads according to the selected policy and is
not a guarantee that all sensitive free text has been removed.

## Crate ownership

| Crate | Responsibility |
|---|---|
| `mcptracer-protocol` | Transport framing and JSON-RPC field extraction |
| `mcptracer-storage` | SQLite, migrations, artifact import/export, stored queries |
| `mcptracer-redact` | Stored-payload redaction policies |
| `mcptracer-model` | Correlation, comparison, assertions, evaluation, offline matching |
| `mcptracer-intel` | Rebuildable derived facts and local analysis |
| `mcptracer-proxy` | CLI, process lifecycle, forwarding, and command orchestration |

## Reproduction and comparison

The model correlates request/response pairs using their direction and request
identity. `validate` checks capture integrity; a healthy capture may still
contain a failed tool execution. `replay` and `replay-http` drive captured
requests against a live target. `serve` instead answers from a recording for
offline client tests.

`diff` compares request parameters, outcomes, responses, tool contracts, and
trajectory evidence. `assert` applies explicit policies or a golden recording.
Versioned JSON schemas describe machine-readable output; schema migrations
must be handled by consumers when adopting a new output version.

Replay can execute real tool side effects. Use execution plans and a controlled
target. A replay result describes behavior for the captured requests, not the
correctness of a model's future tool choices.

## Derived local analysis

`index rebuild` derives facts and tool versions from recorded sessions. The
index is rebuildable; raw sessions remain the source of truth. `route`,
`optimize`, and `graph` consume these facts outside the forwarding path.
The feature-gated semantic-search command uses lexical/glossary retrieval.
These advisory surfaces do not invoke remote models in the capture path.

## Security boundaries

Recordings are sensitive local files. Review redaction and artifact contents
before sharing. The inspector defaults to loopback and uses Host/Origin checks
and a per-launch bearer token for session APIs. Explicit non-loopback access
does not add TLS.

See [Security policy](../../SECURITY.md), [session model](../spec/session-model.md),
[HTTP recording](../spec/transport-http.md), [replay plans](../spec/replay-plan.md),
and [compatibility boundaries](../spec/mcp-2026-07-28.md) for detailed contracts.
