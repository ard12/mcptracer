# mcptracer (npm / npx wrapper)

Zero-install CLI wrapper for **[MCPTracer](https://github.com/ard12/mcptracer)** — the executable evidence and release-assurance layer for the Model Context Protocol (MCP).

## Current Install Path

This repository contains a local npm package candidate, but `mcptracer` is not
published to npm. Registry-based `npx` and `npm install` commands are not
available. Install the published `v0.3.0-rc1` binary preview with the explicit
version pin in the main [installation guide](https://github.com/ard12/mcptracer/blob/main/docs-site/src/installation.md).

This README will describe the npm wrapper's usage after an authorized package
publication is completed.

## How It Works

This package resolves your OS and architecture (`win32-x64`, `darwin-arm64`, `darwin-x64`, `linux-arm64`, `linux-x64`), locates the high-performance native Rust binary, and executes it with zero runtime overhead.