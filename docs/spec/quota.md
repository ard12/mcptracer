# Spec: Offline Quota Simulation

Status: **shipped** (backlog T-89).

## Purpose and boundary

`mcptracer quota` replays the timing and token demand visible in a recorded
session through a local token-bucket model. It is a pre-flight and regression
signal: it can compare a trace with a named preset or explicit refill/capacity
values without making provider calls.

It is **not** a provider billing calculator, an exact tokenizer, or a promise
that a provider will return (or avoid) HTTP 429. Provider limits can vary by
account, model, endpoint, and time. Presets are convenient model inputs, not a
live entitlement lookup. Use `--rate` and `--capacity` when the limits that
apply to a deployment are known.

## Token-count provenance

Every debit has one of two sources:

- `reported`: the recorded JSON payload contains a non-negative integer at
  `usage.total_tokens` or `result.usage.total_tokens`.
- `estimated_from_bytes`: no supported usage field is present, so MCPTracer
  uses `max(payload_bytes / 4, 10)`. This is an intentionally simple,
  uncalibrated fallback and must not be described as a tokenizer count.

Human output labels reported, estimated, and mixed evidence and prints the
fallback formula whenever any estimate is present. JSON output carries the
same provenance as event/token aggregates in `token_sources`, including a
separate summary for the peak one-second demand window.

MCP traffic often contains tool arguments and results but no model-provider
usage metadata, so estimated evidence is expected. A quota result derived
partly or wholly from estimates is still useful for comparative regression
testing, but its absolute values should be calibrated against production
telemetry before being used as an operational limit.

## Simulation

For a single session, events retain their recorded timestamps and consume a
continuous-time token bucket initialized at full capacity. A rejected debit is
counted as a modeled violation. `burst_factor` is peak one-second demand divided
by median non-empty one-second demand (or mean demand when the median is zero).

Fleet mode runs the same independent simulation for every non-empty stored
session. `p429` is the fraction of those sessions with at least one modeled
violation; it is a deterministic scenario ratio, not a statistically fitted
probability of a provider response.

## Stable output

`quota --json` emits schema version 2 and one of two discriminated shapes:
`single_session` or `fleet`. Both are published in
[`schemas/quota-report.v2.schema.json`](../../schemas/quota-report.v2.schema.json).
The schema requires token-source summaries so downstream gates cannot silently
treat estimates as measured usage.

Version 2 adds two fields to the `fleet` shape. `--all` evaluates at most the
1000 most recent sessions — a bound on memory, not a sampling strategy — so
`sessions_considered` reports how many were actually evaluated and `truncated`
is `true` when older sessions were excluded. A `true` value means the reported
`p429` was computed over a partial denominator, and therefore that
`--max-p429` gated on a partial fleet; the human-readable output prints a
matching warning on stderr. Version 1 remains published and unchanged for
existing consumers.

## Examples

```bash
mcptracer quota <session-id> --preset openai-tier2
mcptracer quota <session-id> --rate 2000 --capacity 40000 --json
mcptracer quota --all --preset openai-tier2 --max-p429 0.05
```
