# mcptracer (npm / npx wrapper)

Zero-install CLI wrapper for **[MCPTracer](https://github.com/ard12/mcptracer)** — the executable evidence and release-assurance layer for the Model Context Protocol (MCP).

## Quick Start with `npx` (No Rust Required)

Run MCPTracer directly on any system with Node.js installed:

```bash
# Record an MCP session (prints the new session id)
npx mcptracer record --client claude -- npx -y @modelcontextprotocol/server-everything

# Run an offline stdio mock server from a recorded session
npx mcptracer serve <session-id>

# Diff two recorded sessions or check for tool contract rug-pulls
npx mcptracer diff <baseline-session-id> <candidate-session-id>

# Pin tool contract hashes in CI
npx mcptracer assert <session-id> --spec pin.toml
```

## Global Installation

```bash
npm install -g mcptracer
mcptracer --version
```

## How It Works

This package resolves your OS and architecture (`win32-x64`, `darwin-arm64`, `darwin-x64`, `linux-x64`), locates the high-performance native Rust binary, and executes it with zero runtime overhead.