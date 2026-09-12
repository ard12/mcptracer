# Spec: Temporal Tool Memory Graph Export (`mcptracer graph`)

Status: **shipped, v1 scope.** Built entirely on the derived index
(`mcptracer index rebuild`) added for
[the derived-index architecture](../architecture/overview.md#derived-local-analysis). Read-only:
`graph` never mutates recorded sessions or the derived index.

## Goal

Expose the derived index — sessions, tools, tool versions, and the
relationships between them — as a graph, in two formats: **JSONL** (one JSON
record per line, for tooling) and **DOT** (for Graphviz rendering). Simple
filters (`--tool`, `--server`, `--since`) narrow the export without a query
language.

```bash
mcptracer graph                              # full graph, JSONL
mcptracer graph --tool echo --format dot | dot -Tsvg -o graph.svg
mcptracer graph --since 2026-07-01T00:00:00Z --json
```

## v1 scope

Nodes: `session`, `tool`, `tool_version`. Edges: `calls` (session →
tool_version), `has_version` (tool → tool_version), `supersedes`
(tool_version → tool_version, newer → older).

`findings`, `bench_run` nodes and `failures`/`recommendations` edges are
**deferred**: `diff`, `assert`, and `bench` results are not persisted
anywhere today, so there is no source of truth to export them from without
adding new schema. That is a separate, larger change than this task.

## JSONL schema

One JSON object per line. Every object has a `"type"` field, which alone
disambiguates node records from edge records (the two sets of type values
never overlap) — no separate wrapper is needed.

### Node types

```jsonc
// type = "session"
{"type": "session", "id": "session:<uuid>", "client": "codex",
 "server_command": "python server.py", "transport": "stdio",
 "started_at_ns": 1730000000000000000, "ended_at_ns": 1730000001000000000,
 "redaction_policy": "default"}

// type = "tool"
{"type": "tool", "id": "tool:<server_key>::<tool_name>",
 "server_key": "server:<sha256>", "name": "echo"}

// type = "tool_version"
{"type": "tool_version",
 "id": "toolversion:<server_key>::<tool_name>::<description_hash>::<schema_hash>",
 "server_key": "server:<sha256>", "tool_name": "echo",
 "description_hash": "<sha256>", "schema_hash": "<sha256>",
 "first_seen_at_ns": 1730000000000000000, "last_seen_at_ns": 1730000005000000000}
```

### Edge types

```jsonc
// type = "calls" — a session observed/called a tool version
{"type": "calls", "from": "session:<uuid>", "to": "toolversion:...",
 "observed_at_ns": 1730000000500000000}

// type = "has_version" — a tool version belongs to a tool
{"type": "has_version", "from": "tool:...", "to": "toolversion:..."}

// type = "supersedes" — from (newer) supersedes to (older)
{"type": "supersedes", "from": "toolversion:<newer>", "to": "toolversion:<older>"}
```

Node ids are stable and referenced by edges' `from`/`to` fields; they are not
database row ids and must not be assumed to match `tool_versions.id` or any
other internal primary key.

## DOT format

A single `digraph mcptracer { ... }` block. Sessions render as `box`, tools
as `ellipse`, tool versions as `note`. `supersedes` edges render in red so
drift is visually distinct from ordinary `calls`/`has_version` edges.

## Filters

- `--tool <name>` — keep only tool/tool_version nodes with that name, edges
  touching them, and the sessions that called them.
- `--server <server_key>` — same, scoped by the opaque server key (see
  `mcptracer index facts --json`, `session_observed_server` facts).
- `--since <RFC3339>` — keep only records at or after this timestamp
  (session start, observation time, or supersession-edge observed time,
  whichever applies to that record). Combines with `--tool`/`--server`.

Filters compose: pruning always keeps node/edge-set consistency — an edge is
only included when both its endpoints survived filtering.
