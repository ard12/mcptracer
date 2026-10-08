# mcptracer (Python CLI Wrapper)

Python CLI wrapper candidate for **[MCPTracer](https://github.com/ard12/mcptracer)**, the executable evidence and release-assurance layer for MCP.

## Current installation status

This package is not published to PyPI. Use the native binary preview or source
installation in the main installation guide. Do not assume that a package with
this name in a registry is maintained by this repository.

For local wrapper development, explicitly set `MCPTRACER_BIN` to the native
binary you intend to run. The wrapper never discovers development binaries
from ancestor Cargo workspaces. An invalid explicit path fails without falling
back to a download. The account-local release cache and checksum-verified
download path remain available when no override is set.

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
