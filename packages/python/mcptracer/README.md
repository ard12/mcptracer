# mcptracer (Python CLI Wrapper)

Zero-install Python CLI wrapper for **[MCPTracer](https://github.com/ard12/mcptracer)** — the executable evidence and release-assurance layer for the Model Context Protocol (MCP).

## Quick Start

```bash
pip install mcptracer
mcptracer --version
```

## Common Workflows

```bash
# Record an MCP session from a Python server (FastMCP / stdio); prints the new session id
mcptracer record --client claude -- python my_mcp_server.py

# Offline stdio mock replay for deterministic pytest runs
mcptracer serve <session-id>

# Diff two recorded sessions or detect contract rug-pulls
mcptracer diff <baseline-session-id> <candidate-session-id>

# Assert tool hashes and latency budgets in CI
mcptracer assert <session-id> --spec pin.toml
```