---
name: Bug report
about: Report incorrect recording, forwarding, or inspection behavior
title: "[bug] "
labels: bug
---

## Summary

A clear description of what went wrong.

## Reproduction

The exact command you ran, e.g.:

```
mcptracer record --client codex -- your-mcp-server --arg value
```

The MCP client and server involved (name and version), and the transport
(stdio is the only supported transport today).

## Expected vs. actual

- Expected:
- Actual:

## Environment

- MCPTracer version / commit:
- OS and version:
- Rust version (`rustc --version`):

## Notes

Do not paste unredacted sensitive payloads. If logs are relevant, capture stderr
(`MCPTRACER_LOG=debug`) — stdout is protocol traffic and should not be pasted raw.
