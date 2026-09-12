# MCPTracer

> **Noncommercial source preview.** Commercial use requires written permission
> from ard12. See [licensing and attribution](licensing.md).

**Wireshark for MCP—with replay and regression tests.**

Record and inspect communication between AI agents and MCP servers: tool calls,
arguments, responses, timing, and failures. Replay sessions after a change,
compare the results, and turn functional or tool-definition regressions into
failing CI checks. The recorded evidence stays local.

The stdio wrapper forwards original bytes and stores a normalized, correlated
copy. Streamable HTTP recording uses a local reverse proxy. Only MCP traffic
routed through those paths is visible; ordinary editor and terminal activity
outside MCP is outside the recorder's view.

## Who is this for?

- **MCP server authors:** check whether updates change behavior or tool contracts.
- **Agent developers:** investigate failing integrations with MCP tools.
- **CI maintainers:** turn recorded workflows into repeatable regression checks.
- **Security reviewers:** inspect changed definitions and exchanges with local evidence.

## The core workflow

```bash
mcptracer record --redact default -- your-mcp-server   # golden session
mcptracer replay <golden> -- your-mcp-server            # after a server change
mcptracer diff <golden> <replay>                        # exit 1 on meaningful change
mcptracer assert <replay> --spec checks.toml             # CI gate (exit 0/1/2)
```

## What it's for

- **Regression testing for MCP servers.** Record a golden session once,
  replay it after a change, and diff the result — functional drift (changed
  responses, new errors, latency shifts) and tool-definition drift (added or
  removed tools, changed descriptions/schemas/annotations — the shape of a
  rug pull) both get caught in CI, not noticed live.
- **Redaction that never touches the wire.** `--redact default` masks
  common secret-bearing keys in the *stored* copy only; the bytes forwarded
  between client and server are never modified. Redaction is best-effort
  key-name matching — it does not scan free text, URLs, arrays, or
  command-line arguments — so a recording is not automatically safe to
  share. See [Redaction](guide/redaction.md) for exactly what it does and
  does not cover.
- **Fidelity invariants, documented and tested.** Forward-before-record,
  byte-exact passthrough, dropped-message accounting (including frames that
  forward successfully but fail to parse as JSON), and bidirectional
  request/response correlation (server-initiated calls pair correctly even
  when ids collide across directions).

This site is generated from the same markdown files that live in the
repository's `docs/` directory (`{{#include}}`d, not copy-pasted), so it
never drifts from the specs the code is actually built against. See
[Contributing](contributing.md) if you want to work on MCPTracer itself.
