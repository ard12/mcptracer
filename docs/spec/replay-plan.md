# Spec: Replay/Bench Execution Plans

Status: **shipped**. Depends on [`replay.md`](replay.md) and the shipped
`mcptracer bench` command.

## Goal

Let a caller — a human at a terminal, or CI — see exactly what a `replay` or
`bench` run would send before anything actually executes, and narrow what
actually gets sent, without weakening the project's existing "warn, never
silently block" philosophy for the real run itself.

```bash
mcptracer replay <session-id> --plan
mcptracer bench <session-id> --plan
mcptracer replay <session-id> --allow-tool search --deny-tool delete_all -- <server-command>
```

## `--plan`: report, don't execute

`--plan` computes the exact same "driven calls" list a real run would send —
`mcptracer-model::plan::driven_messages`, the single shared implementation
`replay`, `bench`, and `--plan` all use so they can never disagree — and
prints it as JSON to stdout, then exits. It does not:

- spawn the target MCP server process,
- open a network connection,
- create a new recorded session,
- require the trailing `-- <server-command>` at all (`server_args` is
  optional whenever `--plan` is set; it is still required for a real run).

The JSON shape (`mcptracer_model::plan::Plan`) is:

```jsonc
{
  "total_calls": 5,
  "allowed_calls": 4,
  "filtered_out_calls": 1,
  "destructive_calls": 0,
  "unknown_risk_calls": 2,
  "redacted_placeholder_calls": 0,
  "requires_acknowledgement": false,
  "calls": [
    {
      "seq": 3,
      "kind": "request",
      "method": "tools/call",
      "tool_name": "echo",
      "rpc_id": "3",
      "annotations": { "destructive_hint": null, "read_only_hint": null, "idempotent_hint": null, "open_world_hint": null },
      "risk": "unknown",
      "contains_redacted_placeholder": false,
      "allowed": true
    }
  ]
}
```

This shape is a stable contract: a CI pipeline can parse it and gate on
`requires_acknowledgement`, `destructive_calls`, or a specific tool's `risk`
without depending on stderr text.

## Risk annotations: session-observed, never assumed

A tool call's `risk` (`read_only` / `destructive` / `unknown`) comes only from
what the *source session itself* recorded: if a `tools/list` response in that
session declared `destructiveHint`/`readOnlyHint` for that tool name, the plan
uses it; otherwise the call is `unknown`. Nothing is inferred from a tool's
name or arguments (a tool named `delete_all` is not assumed destructive), and
nothing is fetched live (no `tools/list` round-trip against the real server —
that would itself be a network call, which `--plan` must not make). A tool
call that was never in any observed `tools/list` has `annotations: null`
(never seen) rather than `unknown` hints (seen, but no hint declared) — the
JSON distinguishes the two.

Most real MCP servers declare no annotations at all, so `unknown_risk_calls`
is commonly nonzero even for an entirely safe session. For that reason
`unknown_risk_calls` alone does **not** set `requires_acknowledgement` — see
the field's doc comment in `mcptracer-model::plan` for the full reasoning.
`requires_acknowledgement` is true only when the session itself recorded a
`destructiveHint: true` call, or a call whose stored payload still contains
the `***REDACTED***` placeholder (a call that is essentially guaranteed to
misbehave against a live target, independent of any annotation).

## `--allow-tool` / `--deny-tool`

Both flags take tool names, repeatable or comma-separated. `--deny-tool`
always wins over `--allow-tool` for a name present in both. A driven message
that is not a tool call (`initialize`, `notifications/*`) is never filtered —
these are protocol lifecycle, not a tool invocation a caller could reasonably
want to exclude.

The filters apply identically in both modes:

- In `--plan`, a filtered-out call is still listed (`allowed: false`) so the
  report shows the full picture, but it never counts toward
  `requires_acknowledgement` — a call that will never be sent cannot force an
  acknowledgement.
- In a real run, a filtered-out call is skipped entirely: it is never written
  to the target server's stdin and never recorded.

Both flags default to empty (no filtering), so existing invocations without
them are unaffected.

## Deliberately not a runtime gate

Earlier drafts of this spec considered making `--i-understand-side-effects`
mandatory for a noninteractive (non-TTY stdin) real run whenever
`requires_acknowledgement` would be true, refusing to execute otherwise. That
was rejected: `replay`/`bench` already have a well-established, tested
"forward/execute, then warn — never silently block a recorded call from
running" design (mirroring the redaction path's forward-before-record rule),
and several integration tests exercise exactly that — a redacted-placeholder
replay is expected to succeed with a warning, not fail. A hard runtime block
would contradict that precedent and break those tests for a behavior change
`--plan`'s JSON output already lets a caller implement for itself. So: the
acknowledgement policy is expressed once, as the `requires_acknowledgement`
field, and it is up to the caller (a human reading `--plan` output, or a CI
script parsing it) to decide whether to proceed, add `--i-understand-side-effects`,
or narrow the run with `--allow-tool`/`--deny-tool`. `replay`/`bench`
themselves never refuse to run based on this field.

## Acceptance (integration)

`tests/test_proxy_integration.py::test_replay_and_bench_plan_mode` covers:

1. `--plan` with no trailing server command at all, for both `replay` and
   `bench`, exits `0` and prints a JSON plan matching the recorded session's
   driven calls.
2. A tool call observed in the session's own `tools/list` (with no hints
   declared) and a tool call never listed at all both classify as
   `"unknown"`, but are distinguishable via `annotations` (`{}`-shaped vs.
   `null`).
3. `--deny-tool` marks the denied call `allowed: false` in the plan without
   removing it from the report, and does not affect
   `requires_acknowledgement`.
4. A real `replay` run with `--deny-tool` never sends the denied call: the
   target session's recorded messages contain only the allowed tool's calls.
