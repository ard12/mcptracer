# MCP Transport Notes

These notes correct the initial build-guide assumption.

## Stdio

Phase 1 supports stdio only. MCP stdio messages are newline-delimited JSON-RPC messages. The proxy must not expect LSP-style `Content-Length` framing.

Implementation consequences:

- Read until `\n`.
- Strip trailing `\r` only for JSON parsing.
- Forward the original consumed bytes unchanged.
- Do not write logs to stdout.

## HTTP

Current HTTP support should target Streamable HTTP. Legacy HTTP/SSE compatibility can be added later behind a separate transport implementation.
