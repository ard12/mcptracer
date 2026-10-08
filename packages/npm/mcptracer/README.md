# mcptracer (npm / npx wrapper)

Zero-install CLI wrapper for **[MCPTracer](https://github.com/ard12/mcptracer)** — the executable evidence and release-assurance layer for the Model Context Protocol (MCP).

## Current Install Path

This repository contains a local npm package candidate, but `mcptracer` is not
published to npm. Registry-based `npx` and `npm install` commands are not
available. Install the `v0.3.0-rc2` binary preview, once its release assets are available, with the explicit
version pin in the main [installation guide](https://github.com/ard12/mcptracer/blob/main/docs-site/src/installation.md).

This README will describe the npm wrapper's usage after an authorized package
publication is completed.

## How It Works

For local development, explicitly set `MCPTRACER_BIN` to the native binary you
intend to run. Ancestor Cargo workspaces do not authorize automatic binary
selection. An invalid explicit path fails instead of falling back to a download.
Without an override, selection uses the account-local release cache or the
checksum-verified release download path.

This package resolves your OS and architecture (`win32-x64`, `darwin-arm64`, `darwin-x64`, `linux-arm64`, `linux-x64`), locates the high-performance native Rust binary, and executes it with zero runtime overhead.
