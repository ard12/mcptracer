# Recording

Wrap an MCP stdio server:

```bash
mcptracer record --client codex -- your-mcp-server --arg value
```

Record an MCP Streamable HTTP server through a local reverse proxy:

```bash
mcptracer record-http --listen 127.0.0.1:8787 --target https://server.example/mcp --client codex --redact default
```

`record` and `record-http` forward every byte before recording it — the
recorded copy is a read-only side effect of forwarding, never a
transformation of it. `--redact <policy>` controls what gets masked in the
*stored* copy only (see [Redaction](redaction.md)); forwarded traffic is
never touched by any redaction policy.

By convention, `record-http` refuses to bind to a non-loopback address
unless `--allow-non-loopback` is passed, since it's a reverse proxy that
could otherwise expose an MCP server's traffic to the local network.

## Limits and shutdown behavior

A single stdio frame is capped at 8 MiB. Past that, `record` stops forwarding
that direction and exits non-zero rather than growing its buffer without
bound: the capture is incomplete and forwarding has stopped mid-session, so a
zero exit would let a truncated recording pass a CI gate as healthy. A
base64-encoded image or file read can approach this limit — if you hit it,
the session is not usable as evidence.

`record` stops as soon as any of three things happens: the client closes
stdin, the wrapped server closes stdout, or it is asked to stop — `Ctrl-C`
(or `Ctrl-Break` on Windows), or `SIGTERM`, which is how MCP clients and
process managers stop a server. In every case the session is finalized and
closed before the process exits, so `mcptracer validate` can report on it.
When the client closes stdin first, the server gets up to 30 seconds to flush a
final in-flight response before capture stops. When the server exits first,
`record` exits too and closes its own stdout — which is how the MCP client
observes that its server is gone, exactly as it would have without the proxy
in the middle.

Once capture stops, the server's stdin is closed and the session is finalized;
the server then gets 5 seconds to exit before it is killed. Because the session
is already closed during that wait, a client that follows `SIGTERM` with `SIGKILL`
during that wait does not leave it unclosed. A stop request exits with status
0 even if the server exits non-zero on its way down.

Timeouts and repeated stops:

| Waiting for | Limit | A further stop request |
| --- | --- | --- |
| The server's last output after the client disconnects | 30 s | ends capture now |
| The server to exit after capture stops | 5 s | kills the server now |

Only the directly launched server process is killed. If the server command is
a launcher such as `npx` or a shell script, processes it started itself can
outlive it.
