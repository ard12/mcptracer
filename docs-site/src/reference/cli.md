# CLI Commands

Generated from `mcptracer --help` and each subcommand's `--help`. Do not
hand-edit; regenerate with `scripts/generate-cli-reference.sh`.

```text
Record and inspect MCP stdio sessions

Usage: mcptracer [OPTIONS] <COMMAND>

Commands:
  record       Wrap an MCP stdio server, forwarding traffic byte-exact and recording a normalized, correlated copy
  record-http  Record MCP Streamable HTTP traffic through a local reverse proxy
  setup        Safely wrap configured stdio MCP servers for a supported client
  replay       Re-run a recorded session's client traffic against a (possibly changed) server, capturing a new session for comparison
  replay-http  Re-run a recorded Streamable HTTP session's client traffic against a live HTTP target, capturing a new session for comparison
  serve        Run an offline stdio MCP mock server from a recorded session
  validate     Check a recorded session's capture integrity before using it as a gate
  merge        Combine N recorded sessions into one, optionally deduplicated
  sessions     List and inspect recorded sessions
  diff         Compare two recorded sessions (exit 1 on meaningful differences)
  diff-batch   Run diff over many session pairs from one config; fan-out CI gate
  eval         Score a session against an expected-tool-call spec (accuracy, not just pass/fail); offline, no live model
  assert       Check a session against a TOML assertion spec or a golden session
  baseline     Address an approved baseline session by (project, scenario, environment) instead of a raw session id, with a candidate/approved/superseded/revoked lifecycle
  bench        Replay captured traffic repeatedly and report load-test metrics
  stats        Summarize one session (exchange counts, error rate, latency, tools)
  search       Search messages across all recorded sessions
  export       Write one validated, redaction-safe session artifact
  import       Import a local `.mtrace` artifact without executing its contents
  verify       Offline-verify a `.mtrace` artifact (and, if recorded, a baseline or assertion spec) against an evidence manifest's recorded digests
  index        Rebuild or inspect the derived memory index over recorded sessions
  inspect      Read-only local web UI over recorded sessions (session list + timeline)
  route        Recommend next commands for a session from the derived index and stats
  optimize     Mine recorded history for latency/assertion/bench suggestions
  graph        Export the temporal tool memory graph as JSONL or DOT
  quota        Evaluate token-burst and rate-limit survival offline, labeling reported vs estimated counts
  help         Print this message or the help of the given subcommand(s)

Options:
      --db <DB>
  -h, --help     Print help
  -V, --version  Print version
```

## `mcptracer record`

```text
Wrap an MCP stdio server, forwarding traffic byte-exact and recording a normalized, correlated copy

Usage: mcptracer record [OPTIONS] <SERVER_ARGS>...

Arguments:
  <SERVER_ARGS>...

Options:
      --client <CLIENT>            [default: unknown]
      --db <DB>
      --redact <REDACT>            Redaction policy applied to stored payloads: `none` or `default`. Forwarded bytes are never modified [default: none]
      --redact-keys <REDACT_KEYS>  Extra JSON field names to mask, comma-separated. Requires `--redact default` and is stored as the `custom` policy
  -h, --help                       Print help
```

## `mcptracer record-http`

```text
Record MCP Streamable HTTP traffic through a local reverse proxy

Usage: mcptracer record-http [OPTIONS] --target <TARGET>

Options:
      --db <DB>

      --listen <LISTEN>
          Local address that accepts MCP Streamable HTTP requests [default: 127.0.0.1:8787]
      --target <TARGET>
          Absolute upstream Streamable HTTP endpoint URL
      --client <CLIENT>
          [default: unknown]
      --redact <REDACT>
          Redaction policy applied to stored payloads: `none` or `default` [default: none]
      --redact-keys <REDACT_KEYS>
          Extra JSON field names to mask, comma-separated. Requires `--redact default` and is stored as the `custom` policy
      --allow-non-loopback
          Permit binding the local reverse proxy to a non-loopback address
      --group-stateless-by-header <HEADER>
          Group otherwise-stateless requests carrying the same value in this header into one MCPTracer recording. Useful for MCP 2026-07-28, which has no protocol session id. The value is held only in process memory for correlation and is never persisted or printed
  -h, --help
          Print help
```

## `mcptracer setup`

```text
Safely wrap configured stdio MCP servers for a supported client

Usage: mcptracer setup [OPTIONS] [CLIENT]

Arguments:
  [CLIENT]  Client configuration to update [possible values: claude-desktop, cursor, codex, vscode]

Options:
      --db <DB>
      --undo             Restore the pre-MCPTracer backup. Without a client, restores every supported default configuration that has an MCPTracer backup
      --config <CONFIG>  Override the discovered client configuration path
  -h, --help             Print help
```

## `mcptracer replay`

```text
Re-run a recorded session's client traffic against a (possibly changed) server, capturing a new session for comparison

Usage: mcptracer replay [OPTIONS] <SESSION_ID> [SERVER_ARGS]...

Arguments:
  <SESSION_ID>      Id (or unique prefix) of the recorded session to replay
  [SERVER_ARGS]...

Options:
      --client <CLIENT>
          [default: unknown]
      --db <DB>

      --redact <REDACT>
          Redaction policy applied to the NEW session's stored payloads. Forwarded bytes are never modified [default: none]
      --redact-keys <REDACT_KEYS>
          Extra JSON field names to mask, comma-separated. Requires `--redact default` and is stored as the `custom` policy
      --timing <TIMING>
          `fast`: send the next client message as soon as the previous correlated response arrives. `realtime`: reproduce the source session's inter-message gaps [default: fast] [possible values: fast, realtime]
      --request-timeout <REQUEST_TIMEOUT>
          Milliseconds to wait for a response before recording a replayed request as unanswered and moving on [default: 30000]
      --strict-server-requests
          Abort the replay instead of auto-responding when the server sends a request the source session cannot answer
      --i-understand-side-effects
          Acknowledge that replay re-executes every recorded request for real against the target server (deletes, sends, payments, and any other side effect included — this is not a preview), and that a source session recorded with redaction sends the literal `***REDACTED***` placeholder as the argument value rather than the real secret. Suppresses both warnings printed before every run
      --plan
          Print a JSON report of the calls this replay would make (methods, tools, redacted placeholders, known risk annotations) and exit without spawning a server process or sending anything. The trailing server command is not required with this flag
      --allow-tool <ALLOW_TOOL>
          Only send `tools/call` requests for these tool names (repeatable or comma-separated). Non-tool protocol messages (`initialize`, notifications) are never filtered
      --deny-tool <DENY_TOOL>
          Never send `tools/call` requests for these tool names (repeatable or comma-separated). Takes precedence over `--allow-tool`
  -h, --help
          Print help
```

## `mcptracer replay-http`

```text
Re-run a recorded Streamable HTTP session's client traffic against a live HTTP target, capturing a new session for comparison

Usage: mcptracer replay-http [OPTIONS] --target <TARGET> <SESSION_ID>

Arguments:
  <SESSION_ID>  Id (or unique prefix) of the recorded Streamable HTTP session to replay

Options:
      --db <DB>

      --target <TARGET>
          Absolute live Streamable HTTP endpoint URL to replay against
      --header <HEADERS>
          Extra header to send with every replayed request, in `Name: value` form (repeatable). MCPTracer never records `Authorization` or session headers, so runtime credentials must be supplied this way. Reserved headers (host, content-type, content-length, mcp-session-id, mcp-protocol-version, mcp-method, mcp-name, connection, transfer-encoding) are managed by MCPTracer and cannot be set here. For modern tools/call requests, Mcp-Param-* values are derived only from the captured tool input schema and recorded arguments; manually supplied Mcp-Param-* values are ignored for those calls
      --client <CLIENT>
          [default: unknown]
      --redact <REDACT>
          Redaction policy applied to the NEW session's stored payloads. Sent bytes are never modified [default: none]
      --redact-keys <REDACT_KEYS>
          Extra JSON field names to mask, comma-separated. Requires `--redact default` and is stored as the `custom` policy
      --request-timeout <REQUEST_TIMEOUT>
          Milliseconds to wait for one request response or a subscriptions/listen acknowledgment and its remaining captured notification evidence [default: 30000]
      --i-understand-side-effects
          Acknowledge that replay re-executes every recorded request for real against the live target, and that a source session recorded with redaction sends the literal `***REDACTED***` placeholder as the argument value rather than the real secret. Suppresses both warnings printed before every run
      --allow-tool <ALLOW_TOOL>
          Only send `tools/call` requests for these tool names (repeatable or comma-separated). Non-tool protocol messages (`initialize`, notifications) are never filtered
      --deny-tool <DENY_TOOL>
          Never send `tools/call` requests for these tool names (repeatable or comma-separated). Takes precedence over `--allow-tool`
  -h, --help
          Print help
```

## `mcptracer serve`

```text
Run an offline stdio MCP mock server from a recorded session

Usage: mcptracer serve [OPTIONS] <SESSION_ID>

Arguments:
  <SESSION_ID>  Id (or unique prefix) of a recorded stdio session to serve

Options:
      --db <DB>

      --strict
          Reply with an error for an unmatched request, then exit non-zero
      --match-strategy <MATCH_STRATEGY>
          How a live request is paired with a recorded exchange [default: method-and-params] [possible values: exact, method, method-and-params, subset, sequential]
  -h, --help
          Print help
```

## `mcptracer validate`

```text
Check a recorded session's capture integrity before using it as a gate

Usage: mcptracer validate [OPTIONS] <SESSION_ID>

Arguments:
  <SESSION_ID>  Session id (or unique prefix) to validate

Options:
      --db <DB>
      --json     Print a stable machine-readable integrity report
  -h, --help     Print help
```

## `mcptracer merge`

```text
Combine N recorded sessions into one, optionally deduplicated

Usage: mcptracer merge [OPTIONS] <SESSION_IDS> <SESSION_IDS>...

Arguments:
  <SESSION_IDS> <SESSION_IDS>...  Source session ids (or unique prefixes) in the desired merge order

Options:
      --db <DB>
      --deduplicate  Collapse equivalent client request/response pairs. Requests containing redacted parameter values are retained to avoid false equivalence
      --json         Print the merge result as JSON
  -h, --help         Print help
```

## `mcptracer sessions`

```text
List and inspect recorded sessions

Usage: mcptracer sessions [OPTIONS] <COMMAND>

Commands:
  list
  show
  export-otel  Export a session as OTLP/JSON spans (best-effort OTel GenAI semantic conventions). File output only; never overwrites
  help         Print this message or the help of the given subcommand(s)

Options:
      --db <DB>
  -h, --help     Print help
```

## `mcptracer diff`

```text
Compare two recorded sessions (exit 1 on meaningful differences)

Usage: mcptracer diff [OPTIONS] <SESSION_A> <SESSION_B>

Arguments:
  <SESSION_A>  Baseline session id (or unique prefix)
  <SESSION_B>  Session to compare against the baseline

Options:
      --db <DB>

      --json
          Print the structured DiffReport as JSON instead of a human report
      --ignore <IGNORE>
          JSON pointers (comma separated, relative to the response result/error, e.g. /content/0/text) whose values are ignored
      --ignore-latency
          Skip latency comparison entirely
      --latency-threshold-pct <LATENCY_THRESHOLD_PCT>
          Report latency deltas only when the relative change exceeds this percentage (and the absolute change exceeds 1ms) [default: 20]
      --exit-zero
          Always exit 0, even when differences are found
      --sarif <PATH>
          Write the security section (tool drift: rug-pull-style description, schema, or annotation changes) as a SARIF 2.1.0 log (T-74) to this path, for CI systems that ingest SARIF findings (e.g. GitHub code scanning). Ordinary behavioral differences are not SARIF findings and are not included — see `docs/spec/ci-output-contracts.md`. Never overwrites an existing file
      --github-check-json <PATH>
          Write a GitHub "Create a check run" API payload (T-83) to this path — JSON a CI workflow can post directly, e.g. `gh api repos/{owner}/{repo}/check-runs --input payload.json`. MCPTracer never calls GitHub's API itself; this only produces the payload. Requires `--github-check-sha`. The summary links evidence via `--github-check-details-url` rather than embedding response content — see `docs/spec/ci-output-contracts.md`'s GitHub check-run section for what is and is not included. Never overwrites an existing file
      --github-check-sha <SHA>
          The commit SHA the check run applies to (GitHub Actions: `$GITHUB_SHA`). Required with `--github-check-json`
      --github-check-details-url <URL>
          A URL the check run links to for the full evidence — typically a registry artifact URL (T-81/T-82) — rather than embedding traffic content in the check itself
  -h, --help
          Print help
```

## `mcptracer diff-batch`

```text
Run diff over many session pairs from one config; fan-out CI gate

Usage: mcptracer diff-batch [OPTIONS] <CONFIG>

Arguments:
  <CONFIG>  JSON or TOML config listing session pairs to compare (selected by file extension: `.json` or `.toml`)

Options:
      --db <DB>
      --fail-on-breaking  Exit non-zero if any pair has a meaningful difference. Without this, diff-batch always reports and exits 0 (a fan-out summary, not a gate)
      --json              Print the structured per-pair results as JSON
  -h, --help              Print help
```

## `mcptracer eval`

```text
Score a session against an expected-tool-call spec (accuracy, not just pass/fail); offline, no live model

Usage: mcptracer eval [OPTIONS] --spec <SPEC> <SESSION_ID>

Arguments:
  <SESSION_ID>  Session id (or unique prefix) to score

Options:
      --db <DB>
      --spec <SPEC>  TOML eval spec (expected + forbidden tool calls)
      --json         Print the structured report as JSON
  -h, --help         Print help
```

## `mcptracer assert`

```text
Check a session against a TOML assertion spec or a golden session

Usage: mcptracer assert [OPTIONS] <SESSION_ID>

Arguments:
  <SESSION_ID>  Session id (or unique prefix) to check

Options:
      --db <DB>
      --spec <SPEC>       TOML assertion spec file (rule mode)
      --golden <GOLDEN>   Golden session id (snapshot mode): pass iff `diff golden session` reports no meaningful differences. Latency is ignored here — two runs always jitter; use a `latency` assertion for performance gates
      --json              Print results as JSON
      --manifest <PATH>   Write an evidence manifest (T-73) recording this outcome alongside a canonical content digest of the checked session (and, for `--golden`, the golden session; for `--spec`, the assertion file) — so `mcptracer verify` can later confirm offline, without a database, that a given `.mtrace` export is the exact evidence that produced this result. Never overwrites an existing file
      --redact <POLICY>   Upgrade an unredacted checked (or golden) session to the default stored-copy redaction policy before computing its manifest digest. Only meaningful with --manifest; ignored otherwise
      --allow-unredacted  Explicitly allow computing a manifest digest over an unredacted session. Only meaningful with --manifest; ignored otherwise
      --junit <PATH>      Write a JUnit XML report (T-74) to this path, for CI systems that consume test results as a file (e.g. GitHub Actions' test-reporting actions, GitLab's `artifacts: reports: junit`). Never overwrites an existing file
      --github            Print GitHub Actions workflow-command annotations (`::notice::`/ `::error::`) to stdout, one per assertion, so a run's PASS/FAIL results surface directly in the GitHub Actions log UI
  -h, --help              Print help
```

## `mcptracer baseline`

```text
Address an approved baseline session by (project, scenario, environment) instead of a raw session id, with a candidate/approved/superseded/revoked lifecycle

Usage: mcptracer baseline [OPTIONS] <COMMAND>

Commands:
  candidate  Register a recorded session as a candidate baseline for (project, scenario, environment). Multiple simultaneous candidates for the same triple are allowed
  promote    Promote a registered candidate to approved, superseding any prior approved baseline for the same (project, scenario, environment). Records a canonical content digest of the session at promotion time
  revoke     Revoke the current approved baseline for (project, scenario, environment). Resolve then finds nothing for that triple until a new baseline is promoted
  resolve    Print the session id of the approved baseline for (project, scenario, environment), for scripting -- e.g. `mcptracer assert "$SESSION" --golden "$(mcptracer baseline resolve p s e)"`. Plain-text mode prints only the session id and nothing else, so it is safe to capture directly
  list       List baselines in every state, optionally filtered by project
  help       Print this message or the help of the given subcommand(s)

Options:
      --db <DB>
  -h, --help     Print help
```

## `mcptracer bench`

```text
Replay captured traffic repeatedly and report load-test metrics

Usage: mcptracer bench [OPTIONS] <SESSION_ID> [SERVER_ARGS]...

Arguments:
  <SESSION_ID>      Id (or unique prefix) of the recorded session to replay as load
  [SERVER_ARGS]...

Options:
      --db <DB>

      --repeat <REPEAT>
          Number of session iterations to run [default: 20]
      --concurrency <CONCURRENCY>
          Maximum number of concurrent session iterations [default: 4]
      --request-timeout <REQUEST_TIMEOUT>
          Milliseconds to wait for each replayed request response [default: 30000]
      --json
          Emit machine-readable JSON
      --i-understand-side-effects
          Acknowledge that bench re-executes every recorded request for real, `--repeat` times at up to `--concurrency` concurrent iterations (deletes, sends, payments, and any other side effect included — this is not a preview), and that a source session recorded with redaction sends the literal `***REDACTED***` placeholder as the argument value rather than the real secret, `--repeat` times over. Suppresses both warnings printed before every run
      --plan
          Print a JSON report of the calls this bench run would make (methods, tools, redacted placeholders, known risk annotations) and exit without spawning a server process or sending anything. The trailing server command is not required with this flag
      --allow-tool <ALLOW_TOOL>
          Only send `tools/call` requests for these tool names (repeatable or comma-separated). Non-tool protocol messages (`initialize`, notifications) are never filtered
      --deny-tool <DENY_TOOL>
          Never send `tools/call` requests for these tool names (repeatable or comma-separated). Takes precedence over `--allow-tool`
  -h, --help
          Print help
```

## `mcptracer stats`

```text
Summarize one session (exchange counts, error rate, latency, tools)

Usage: mcptracer stats [OPTIONS] <SESSION_ID>

Arguments:
  <SESSION_ID>  Session id (or unique prefix) to summarize

Options:
      --db <DB>
      --json     Print the stats as JSON instead of a human summary
  -h, --help     Print help
```

## `mcptracer search`

```text
Search messages across all recorded sessions

Usage: mcptracer search [OPTIONS]

Options:
      --db <DB>
      --tool <TOOL>      Only messages that are a `tools/call` for this tool name
      --method <METHOD>  Only messages with this JSON-RPC method
      --errors           Only error responses
  -l, --limit <LIMIT>    Maximum number of matches to return (newest first) [default: 50]
      --json             Print matches as a JSON array instead of a table
  -h, --help             Print help
```

## `mcptracer export`

```text
Write one validated, redaction-safe session artifact

Usage: mcptracer export [OPTIONS] --out <OUT> <SESSION_ID>

Arguments:
  <SESSION_ID>  Session id (or unique prefix) to export

Options:
      --db <DB>
      --out <OUT>                New `.mtrace` artifact path. Existing files are never overwritten
      --redact <POLICY>          Upgrade an unredacted source session to the default stored-copy redaction policy before writing the artifact
      --allow-unredacted         Explicitly allow export of a session recorded without redaction
      --allow-sensitive-content  Allow export even when the pre-export sensitive-content lint detects bearer tokens, PEM blocks, URL query secrets, or command-embedded secrets that key-name redaction cannot catch. A loud warning is printed to stderr listing each finding's JSON pointer and category; the actual secret values are never echoed
  -h, --help                     Print help
```

## `mcptracer import`

```text
Import a local `.mtrace` artifact without executing its contents

Usage: mcptracer import [OPTIONS] <PATH>

Arguments:
  <PATH>  Local `.mtrace` artifact or `.vcr` cassette to import (selected by extension). Import never executes its contents

Options:
      --db <DB>
      --strict           Reject unknown top-level fields instead of allowing forward-compatible extensions. Only meaningful for `.mtrace`
      --client <CLIENT>  Client label for a session imported from a `.vcr` cassette (which carries no client field of its own) [default: agent-vcr-import]
  -h, --help             Print help
```

## `mcptracer verify`

```text
Offline-verify a `.mtrace` artifact (and, if recorded, a baseline or assertion spec) against an evidence manifest's recorded digests

Usage: mcptracer verify [OPTIONS] --artifact <ARTIFACT> <MANIFEST>

Arguments:
  <MANIFEST>  Evidence manifest JSON file to verify (written by `assert --manifest`)

Options:
      --artifact <ARTIFACT>        Local `.mtrace` artifact to check against the manifest's recorded artifact digest
      --db <DB>
      --baseline <BASELINE>        Local `.mtrace` artifact to check against the manifest's recorded baseline digest. Required if the manifest records one, unless --allow-partial is given
      --assert-spec <ASSERT_SPEC>  Local assertion TOML file to check against the manifest's recorded assertion-spec digest. Required if the manifest records one, unless --allow-partial is given
      --allow-partial              Allow verifying fewer inputs than the manifest recorded (e.g. an artifact-only check when the manifest also has a baseline or assertion-spec digest). Without this flag, a manifest-recorded digest with no corresponding file is a hard error: verification must be complete by default, and a partial check must be requested explicitly rather than silently passing
  -h, --help                       Print help
```

## `mcptracer index`

```text
Rebuild or inspect the derived memory index over recorded sessions

Usage: mcptracer index [OPTIONS] <COMMAND>

Commands:
  rebuild  Rebuild derived memory facts, edges, and tool versions from recordings
  facts    List derived memory facts
  help     Print this message or the help of the given subcommand(s)

Options:
      --db <DB>
  -h, --help     Print help
```

## `mcptracer route`

```text
Recommend next commands for a session from the derived index and stats

Usage: mcptracer route [OPTIONS] <SESSION_ID>

Arguments:
  <SESSION_ID>  Session id (or unique prefix) to route

Options:
      --db <DB>

      --p95-threshold-ms <P95_THRESHOLD_MS>
          p95 latency (ms) above which the performance route is recommended [default: 2000]
      --json
          Print recommendations as JSON
  -h, --help
          Print help
```

## `mcptracer optimize`

```text
Mine recorded history for latency/assertion/bench suggestions

Usage: mcptracer optimize [OPTIONS]

Options:
      --db <DB>
      --json              Print suggestions as JSON (includes confidence and provenance)
      --allow-unredacted  Acknowledge that optimize will inspect unredacted stored payloads
  -h, --help              Print help
```

## `mcptracer graph`

```text
Export the temporal tool memory graph as JSONL or DOT

Usage: mcptracer graph [OPTIONS]

Options:
      --db <DB>
      --tool <TOOL>      Restrict to a single tool name
      --server <SERVER>  Restrict to a single opaque server key (see `index facts --json`)
      --since <SINCE>    Restrict to records at or after this RFC3339 timestamp, e.g. 2026-07-01T00:00:00Z
      --format <FORMAT>  Output format [default: jsonl] [possible values: jsonl, dot]
  -h, --help             Print help
```

## `mcptracer inspect`

```text
Read-only local web UI over recorded sessions (session list + timeline)

Usage: mcptracer inspect [OPTIONS] [SESSION_ID]

Arguments:
  [SESSION_ID]  Open directly to this session (id or unique prefix); omit to start on the session list

Options:
      --db <DB>
      --listen <LISTEN>     Local address to serve the read-only inspector UI on [default: 127.0.0.1:4317]
      --allow-non-loopback  Permit binding the inspector UI to a non-loopback address
  -h, --help                Print help
```

