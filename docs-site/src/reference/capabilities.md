# Capability Reference

What MCPTracer actually does, one line per command, grouped by the job it
does. **Generated from `mcptracer --help`; do not hand-edit** --
regenerate with `scripts/generate-capability-reference.sh`. For every flag
on every command, see the full [CLI reference](cli.md).

## Capture -- get real MCP traffic into the system

- `mcptracer record` -- Wrap an MCP stdio server, forwarding traffic byte-exact and recording a normalized, correlated copy
- `mcptracer record-http` -- Record MCP Streamable HTTP traffic through a local reverse proxy
- `mcptracer setup` -- Safely wrap configured stdio MCP servers for a supported client
- `mcptracer merge` -- Combine N recorded sessions into one, optionally deduplicated

## Reproduce -- regenerate traffic from what was captured

- `mcptracer replay` -- Re-run a recorded session's client traffic against a (possibly changed) server, capturing a new session for comparison
- `mcptracer replay-http` -- Re-run a recorded Streamable HTTP session's client traffic against a live HTTP target, capturing a new session for comparison
- `mcptracer serve` -- Run an offline stdio MCP mock server from a recorded session
- `mcptracer bench` -- Replay captured traffic repeatedly and report load-test metrics

## Compare -- find what changed between two sessions

- `mcptracer diff` -- Compare two recorded sessions (exit 1 on meaningful differences)
- `mcptracer diff-batch` -- Run diff over many session pairs from one config; fan-out CI gate
- `mcptracer eval` -- Score a session against an expected-tool-call spec (accuracy, not just pass/fail); offline, no live model

## Gate -- pass/fail checks a CI pipeline can act on

- `mcptracer validate` -- Check a recorded session's capture integrity before using it as a gate
- `mcptracer assert` -- Check a session against a TOML assertion spec or a golden session
- `mcptracer baseline` -- Address an approved baseline session by (project, scenario, environment) instead of a raw session id, with a candidate/approved/superseded/revoked lifecycle
- `mcptracer verify` -- Offline-verify a `.mtrace` artifact (and, if recorded, a baseline or assertion spec) against an evidence manifest's recorded digests
- `mcptracer quota` -- Evaluate token-burst and rate-limit survival offline, labeling reported vs estimated counts

## Share -- export, browse, and inspect recorded evidence

- `mcptracer sessions` -- List and inspect recorded sessions
- `mcptracer stats` -- Summarize one session (exchange counts, error rate, latency, tools)
- `mcptracer search` -- Search messages across all recorded sessions
- `mcptracer export` -- Write one validated, redaction-safe session artifact
- `mcptracer import` -- Import a local `.mtrace` artifact without executing its contents
- `mcptracer inspect` -- Read-only local web UI over recorded sessions (session list + timeline)

## Derive -- mine recorded history for insight (Labs)

- `mcptracer index` -- Rebuild or inspect the derived memory index over recorded sessions *(Labs: derived, outside the protocol hot path)*
- `mcptracer route` -- Recommend next commands for a session from the derived index and stats *(Labs: derived, outside the protocol hot path)*
- `mcptracer optimize` -- Mine recorded history for latency/assertion/bench suggestions *(Labs: derived, outside the protocol hot path)*
- `mcptracer graph` -- Export the temporal tool memory graph as JSONL or DOT *(Labs: derived, outside the protocol hot path)*
- `mcptracer semantic` -- Experimental local lexical search over the derived index (off by default at build time; requires the `semantic-search` feature) *(Labs)*
