# Spec: Diff

Status: **shipped.** Depends on the session model
([`session-model.md`](session-model.md)).

## Goal

Compare two correlated sessions (typically a golden recording and a replay) and
report **meaningful** differences: tools, call outcomes, response bodies, error
rates, latencies, and ordering. The whole value is in separating signal from
noise — a naive JSON diff of two sessions is ~100% noise (ids, timestamps).

```bash
mcptracer diff <session-a> <session-b>
mcptracer diff <session-a> <session-b> --json
mcptracer diff <session-a> <session-b> --ignore latency,timestamps
```

## Alignment

Both sessions are run through `correlate()` to get `SessionModel`s. Align
exchanges between A and B by this key, in priority order:

1. `(method, tool_name, ordinal)` where `ordinal` is the 0-based index of that
   `(method, tool_name)` within the session's exchange order. This survives id
   changes and is stable for deterministic clients.
2. Fall back to `rpc_id` alignment only when methods are identical and ordinals
   are ambiguous.

Unaligned exchanges on either side are reported as **added** (only in B) or
**removed** (only in A).

## What counts as a difference

Per aligned exchange, compare:

- **status** — `Ok`/`Error`/`Unanswered` change (e.g. was `Ok`, now `Error`).
- **error_code** — changed JSON-RPC error code.
- **request parameters** — structural JSON diff reported as `request_changed`,
  including removed parameters. Tool arguments and retry input responses count.
- **tool errors** — `result.isError: true` is the distinct `ToolError` status and
  contributes to the error rate.
- **response body** — structural JSON diff of the response `result` payloads,
  after **normalization** (below).
- **latency** — report delta and percentage; gate behind a threshold so tiny
  jitter is not flagged (default: report only if `|Δ| > 20%` **and**
  `|Δ| > 1ms`; configurable via `--latency-threshold`).

Session-level:

- tools added / removed (set difference of tool names).
- error-rate change (errors / total).
- exchange-count change.

## Normalization (noise suppression)

Before comparing request parameters and response bodies, normalize out
non-deterministic fields. MCP's top-level request `requestState` is an opaque
retry token: its generated string value is ignored, while its presence, nested
`arguments.requestState`, and `inputResponses` remain significant. An
`input_required` response's `requestState` is also ignored.

Other normalization rules:

- Drop / mask `id` at the JSON-RPC envelope level (already handled by aligning on
  method+ordinal, but the raw payload still contains it).
- Redaction placeholders (`***REDACTED***`) compare **equal to anything** — a
  redacted field is "unknown," not a difference. Document this clearly.
- Configurable ignore list via `--ignore` (repeatable / comma-separated):
  `latency`, `timestamps`, plus **JSON pointer paths** (e.g.
  `/result/content/0/text`) to ignore known-volatile fields.
- Provide a small built-in default ignore set for common volatile keys
  (`timestamp`, `requestId`, `traceId`) — keep it short and documented.

The normalization rules are the design-critical part. Implement them as a pure,
well-tested function: `fn normalize(value: &Value, rules: &IgnoreRules) -> Value`.

## Output

- **Human (default):** a compact report — a summary line
  (`3 changed, 1 added, 0 removed, error rate 0% -> 10%`), then per-exchange
  sections only for exchanges that actually differ. Use `+`/`-`/`~` markers.
- **`--json`:** a structured `DiffReport` (derive `serde::Serialize`) suitable for
  CI consumption. This is what `assert` and the GitHub Action read.
- **Exit code:** `0` if no meaningful differences, `1` if any. This makes `diff`
  itself CI-usable before `assert` exists. `--exit-zero` to always return 0.

## Types (sketch)

```rust
pub enum ExchangeDelta {
    StatusChanged { from: ExchangeStatus, to: ExchangeStatus },
    ErrorCodeChanged { from: Option<i64>, to: Option<i64> },
    ResponseChanged { pointer_diffs: Vec<PointerDiff> },
    LatencyChanged { from_ns: i64, to_ns: i64, pct: f64 },
}

pub struct AlignedExchange {
    pub key: String,            // method + tool + ordinal
    pub deltas: Vec<ExchangeDelta>,
}

pub struct DiffReport {
    pub changed: Vec<AlignedExchange>,
    pub added: Vec<String>,     // keys only in B
    pub removed: Vec<String>,   // keys only in A
    pub tools_added: Vec<String>,
    pub tools_removed: Vec<String>,
    pub error_rate_from: f64,
    pub error_rate_to: f64,
}
```

`DiffReport::is_empty()` (no changed/added/removed) drives the exit code.

## Where it lives

Diff logic (alignment, normalization, delta computation) is pure and belongs in
`mcptracer-model` (or a sibling `mcptracer-diff` crate — decide when starting;
lean toward keeping it in `model` to avoid crate sprawl). Output formatting lives
in `mcptracer-proxy::commands::diff`.

## Tests

Pure unit tests over hand-built `SessionModel`/payload pairs:
- identical sessions → empty report, exit 0.
- one changed response field → one `ResponseChanged` with the right pointer.
- redacted field vs. real value → **not** a difference.
- latency within threshold → not reported; beyond threshold → reported.
- added/removed tool → reflected in `tools_added`/`tools_removed`.
- status flip `Ok -> Error` → `StatusChanged` + error-rate change.
