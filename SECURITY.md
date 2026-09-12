# Security Policy

## Reporting a vulnerability

Please report security issues privately. Use GitHub's
[private vulnerability reporting](https://github.com/ard12/mcptracer/security/advisories/new)
for this repository rather than opening a public issue.

Include a description of the issue, reproduction steps, and the affected version
or commit. We aim to acknowledge reports within a few days.

## Scope and data-handling notes

MCPTracer records MCP JSON-RPC traffic, which can contain sensitive data: tool
arguments, file contents, credentials, and model output. Keep this in mind when
reporting or handling recordings.

- Recorded sessions are stored **unencrypted** in a local SQLite database
  (default `~/.mcptracer/sessions.db`). Treat that file as sensitive.
- **File permissions (T-61):** on Unix, `Store::open` sets MCPTracer's
  default storage directory (`~/.mcptracer`) to `0700` on every open.
  It also sets a newly created custom database parent to `0700`. An existing
  custom parent supplied through `--db` is caller-managed: MCPTracer warns
  if its mode is broad but does not change it, because that directory may be
  intentionally shared. The database file itself is set to `0600` on every
  open. SQLite's WAL-mode sidecar files (`sessions.db-wal`,
  `sessions.db-shm`) hold the same not-yet-checkpointed session data as the
  main file and get the same `0600` treatment. `export` sets exported
  `.mtrace` artifacts to `0600` before writing any bytes. **On Windows this
  is a no-op**: MCPTracer does not attempt to set or reinterpret ACLs, and
  a file inherits whatever permissions its parent directory and Windows
  account already provide. Do not rely on this for multi-user machines
  where other accounts might otherwise read the file; a Unix directory an
  administrator has made group- or world-readable via an ACL beyond
  `chmod` is also outside what this guarantee covers.
- **Redaction is applied only to the stored copy.** Bytes forwarded between the
  MCP client and server are always passed through unchanged, because the real
  server needs the real payload.
- No sharing, export, or upload feature ships without routing payloads through a
  redaction policy first. If you find a path that persists or transmits
  unredacted sensitive data unexpectedly, report it.

## What redaction covers

MCPTracer's export path applies two layers of protection:

**Layer 1 — Key-name redaction** (`--redact default` or `--redact-keys`):
Values under sensitive key names (`password`, `api_key`, `authorization`,
`token`, `secret`, `credential`, …) are replaced with `"***REDACTED***"`.
Tool-schema definitions (`inputSchema`/`outputSchema`) are exempt because they
describe contracts, not runtime values.

**Layer 2 — Pre-export content lint** (always-on, T-63):
Before writing any `.mtrace` artifact, `export` scans every string leaf for
four patterns that key-name redaction cannot catch:
- `bearer_token` — a string starting with `Bearer ` followed by a non-trivial
  token
- `pem_block` — a string containing `-----BEGIN ` (PEM material)
- `url_query_secret` — a URL whose query string contains a sensitive key name
- `command_secret` — a `--flag=value` in `server_command` metadata whose flag
  name matches the sensitive-key list

If a finding is detected, the export aborts with a message listing each
finding's **JSON pointer and category** — actual values are **never echoed**.
`--allow-sensitive-content` overrides the abort with a loud stderr warning.

**What this does NOT guarantee:**
- Secrets under non-standard key names not in the default list and not added
  via `--redact-keys`.
- Secrets embedded in base64 content, binary blobs, or custom encodings.
- A `command_secret` passed as a separate argv token (`--api-key sk-123`)
  rather than `--api-key=sk-123` — only the `=`-joined form is checked.
- Secrets in any session metadata field other than `server_command`.

If you suspect an artifact may contain sensitive data, review it manually
before sharing: `gunzip < session.mtrace | python -m json.tool`.

## Supported versions

MCPTracer is pre-1.0. Only the latest release and `main` receive security fixes.

## Local inspector access

`inspect` creates a fresh random token for each launch and requires it in a
Bearer authorization header before any session API handler runs. The launch
link carries the token in a URL fragment, which is not sent in HTTP requests.
The browser removes it from the address bar and stores it only for the tab's
session. Keep terminal output and the launch link private.

Host and Origin validation protects the inspector against DNS rebinding and
cross-origin access. Loopback binding remains the default. Explicit
`--allow-non-loopback` does not bypass access checks or provide TLS; use it only
on a trusted network. Static assets expose neither the token nor recordings.
Responses use `Cache-Control: no-store`, a restrictive Content Security Policy,
and `Referrer-Policy: no-referrer`.
