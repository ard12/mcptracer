# Sessions & the Inspector UI

## CLI

```bash
mcptracer sessions list                        # recent sessions
mcptracer sessions show <session-id>            # raw message log
mcptracer sessions show <session-id> --calls    # correlated request/response view
mcptracer sessions show <session-id> --full     # + pretty-printed payloads
```

Every session-taking command accepts a unique id prefix, not just the full
UUID.

## Inspector UI

```bash
mcptracer inspect                # print an authenticated link to the session list
mcptracer inspect <session-id>    # jump straight to a session's timeline
```

A minimal, read-only local web view over the same correlated model `sessions
show --calls` uses: a session list and a per-session timeline with
request/response status, latency, and click-to-expand raw payloads. It's a
static HTML/CSS/JS bundle served by the same binary — no separate frontend
build, no new dependency.

Like `record-http`, it binds to `127.0.0.1` only by default; pass
`--allow-non-loopback` to serve on a non-loopback address (session data can
look sensitive even when redacted — see [Redaction](redaction.md) for what
redaction does and does not cover).

Open the complete link printed by `inspect`, including its `#token=...`
fragment. Each launch creates a fresh access token. The browser removes the
fragment from the address bar and keeps the token in tab-scoped session storage
for navigation and refresh. After restarting the inspector, open its new link.
Without browser session storage, reopen the launch link after a page navigation.

Session API requests require `Authorization: Bearer <token>`. Tokens in query
strings or cookies are not accepted. Host headers must match the bound IP and
port (`localhost` is also accepted for loopback binds); an Origin header, when
present, must match that HTTP origin. Wildcard binds accept literal IP URLs of
the bound address family, not arbitrary DNS names. The token and Host/Origin
checks also apply when `--allow-non-loopback` is used. Static assets contain no
token or recordings, and all responses disable caching and framing.

Treat the launch link as a secret. Non-loopback access uses plain HTTP and is
intended only for a trusted network; the flag does not enable TLS or team access
control.
