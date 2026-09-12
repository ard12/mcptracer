# Changelog

## Unreleased public source preview (`0.3.0-rc1` candidate)

This generated tree is a pre-1.0 source candidate, not evidence that packages,
binary archives, a Homebrew tap, npm/PyPI wrappers, or a GitHub Release exist.
Its authoritative source revision is recorded in
`OSS_EXPORT_MANIFEST.json`.

### Licensing

- Custom noncommercial attribution terms; commercial use requires written
  permission from ard12. See LICENSE, NOTICE, and COMMERCIAL-LICENSE.md.
- Source-available, not an OSI-approved open-source license. Earlier grants
  validly received under other terms are not revoked.

### Included core

- Local stdio and Streamable HTTP capture, deterministic replay, structural
  diffing, assertion gates, redacted `.mtrace` artifacts, SQLite history, and
  an authenticated local inspector.
- Security-oriented tool-contract drift detection, portable evidence manifests,
  offline digest verification, benchmark/evaluation reports, and optional
  derived local intelligence.
- Explicit partial MCP 2026-07-28 support: stateless capture and standard
  routing headers plus bounded schema-derived parameter headers, MRTR retries,
  and subscription replay. Full current-spec conformance is not claimed.

### Reliability fixes in this candidate

- `record` no longer hangs when the MCP server it wraps exits. Its two
  forwarding pumps and a `Ctrl-C` handler now race, so whichever finishes first
  stops the others; the process also no longer waits on an uncancellable
  blocking stdin read at shutdown. A server crash now reaches the client as the
  EOF it would have seen without the proxy, rather than as a silent hang. The
  session is finalized and closed in every exit path, including `Ctrl-C`, and a
  frame past the 8 MiB cap fails the command instead of silently leaving one
  direction half-open with a zero exit code.
- `setup` writes the absolute path of the running binary into client
  configurations. Desktop MCP clients are launched with a minimal `PATH` that
  usually excludes `~/.local/bin`, so the previous bare `mcptracer` command
  could leave every wrapped server unable to start. `setup` also reads JSONC
  configurations (VS Code's `mcp.json` permits comments and trailing commas)
  and warns that rewriting normalizes formatting, with the original preserved
  in its backup.
- `sessions show` accepts `--limit`/`--offset`/`--all` instead of always
  dumping every message with full payloads, and `quota --all` reports when its
  fleet evaluation was capped rather than gating on a partial denominator.
- The documented `install.sh` one-liner runs under a POSIX `/bin/sh` as well as
  `bash`.
- Tool-level `result.isError` failures are distinct from JSON-RPC errors and
  now affect assertions, statistics, benchmarks, exports, and derived facts.
- Golden comparisons fail on meaningful trajectory/order divergence.
- Request argument changes and removals participate in diffing; opaque generated
  retry-state tokens are normalized without ignoring meaningful inputs.
- Tool pinning reconstructs cursor-linked catalog pages and reports incomplete
  catalogs instead of silently treating the last page as complete.
- Agent VCR import accepts the documented nested v1 shape while remaining a
  version-specific, best-effort adapter.
- Inspector session APIs require a per-launch bearer token and validate Host
  and Origin, with restrictive browser response headers.
- On Unix, MCPTracer-owned storage and newly created custom parents are private,
  while existing caller-managed custom parents are warned about but not chmod'd.
  Database and WAL/SHM files remain owner-only.
- One coordinated JSON revision: `ExchangeStatus` and `Direction` now use the
  same encodings as the rest of the API (`snake_case`, and `c2s`/`s2c`), and
  every versioned JSON document carries a `schema_version` so a consumer can
  tell which contract it received. Published as `diff-report.v3`,
  `session-integrity-report.v2`, `assert-results.v2`, `eval-report.v2`,
  `bench-report.v2`, and `quota-report.v2`. Older schema files remain published
  so an archived report still validates against the version it was produced
  under, but each command now emits only the newest version — a CI job pinned
  to an older schema will fail validation on upgrade, by design. See
  "Migrating to the v2/v3 JSON outputs" below.

### Migrating to the v2/v3 JSON outputs

Every affected command emits only the new version. A job pinned to an older
schema file will start failing validation on upgrade — deliberately, so the
change cannot land silently. To stay on the old contract, pin the **binary**
version; the retained schema file alone is not enough.

| Command | Was | Now | What to change |
| --- | --- | --- | --- |
| `diff --json`, `assert --golden --json` | `diff-report.v2` | `diff-report.v3` | Accept `schema_version: 3`. Map `status`/`from`/`to` values `"Ok"`, `"Error"`, `"ToolError"`, `"Subscribed"`, `"Unanswered"`, `"OrphanResponse"` to `"ok"`, `"error"`, `"tool_error"`, `"subscribed"`, `"unanswered"`, `"orphan_response"`. |
| `validate --json` | `session-integrity-report.v1` | `.v2` | Accept `schema_version: 2`; an exact-equality check must include it. |
| `assert --spec --json` | `assert-results.v1` (bare array) | `.v2` (object) | Read `payload["results"]` instead of indexing the top-level value. |
| `eval --json` | `eval-report.v1` | `.v2` | Accept `schema_version: 2`. |
| `bench --json` | `bench-report.v1` | `.v2` | Accept `schema_version: 2`. |
| `quota --json` | `quota-report.v1` | `.v2` | `schema_version` is now `2`; the `fleet` shape adds `sessions_considered` and `truncated`. |
| `sessions show --calls --json` | `"origin": "ClientToServer"` | `"origin": "c2s"` | Also `"ServerToClient"` -> `"s2c"`, matching what `sessions show --json` already emitted. |

Because every schema sets `additionalProperties: false`, an unrecognized
`schema_version` is a hard failure rather than a silent shape change; branch
on it rather than sniffing the document's contents.

### Known boundaries

- Compatibility evidence in this snapshot is Windows-only; Linux and macOS
  workflow results are pending the exact public commit.
- Modern TypeScript/Python/Go/C# SDK coverage, progress alongside MRTR, cache
  evidence semantics, and extension negotiation remain incomplete.
- Recorded tool drift proves change, not malicious intent. Replay proves server
  behavior for captured requests, not that an LLM will make the same choices.
- Evidence manifests are digest-verifiable but unsigned.
