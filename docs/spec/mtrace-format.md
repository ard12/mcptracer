# Spec: `.mtrace` Portable Session Format

Status: **shipped.** The portable interchange format. Depends on redaction
(shipped) and the stable session model ([`session-model.md`](session-model.md)).
**Do not add export before redaction is enforced on the export path** — this is
a hard rule from `CONTRIBUTING.md`.

## Goal

A single, portable, self-describing file that captures one MCP session so it can
be shared in a bug report, checked into a repo as a CI fixture, or replayed on
another machine. It must be **redaction-gated and reviewable before sharing** and
**stable** (versioned) so the format can become an ecosystem standard.

```bash
mcptracer export <session-id> --out session.mtrace
mcptracer export <session-id> --out session.mtrace --redact default   # force redaction
mcptracer import session.mtrace                                        # into local db
mcptracer replay-file session.mtrace -- <server>                      # (later) replay directly
```

## Container

- A `.mtrace` file is a **gzip-compressed JSON document** (single file, not an
  archive, for v1 simplicity). Magic: first bytes are the gzip header; the
  inner document has an explicit `"format"` and `"version"`.
- Rationale for JSON-in-gzip over a binary format: human-inspectable when
  ungzipped, trivially cross-language, good enough compression for JSON-RPC text.
  Revisit a framed binary format only if size becomes a real problem.

## Document schema (v1)

```jsonc
{
  "format": "mtrace",
  "version": 1,
  "exported_at_ns": 1730000000000000000,
  "exporter": "mcptracer/0.4.0",
  "session": {
    "client": "codex",
    "server_command": "python my_server.py",
    "transport": "stdio",
    "started_at_ns": 0,
    "ended_at_ns": 0,
    "redaction_policy": "default",     // MUST reflect what was applied below
    "redaction_keys": [],               // normalized custom keys, if policy is custom
    "dropped_messages": 0,              // retained for integrity checks after import
    "tags": []
  },
  "messages": [
    {
      "seq": 0,
      "ts_ns": 0,
      "direction": "c2s",              // matches storage encoding
      "message_kind": "request",
      "rpc_id": "1",                   // JSON-encoded id, as stored
      "method": "tools/call",
      "tool_name": "read_file",
      "payload": { /* redacted JSON object */ },
      "payload_bytes": 123,
      "is_error": false,
      "error_code": null
    }
  ]
}
```

Notes:

- `payload` is embedded as a **JSON object** (not a re-encoded string) so the file
  is readable and diffable. On import, re-serialize to the storage `payload`
  string form.
- `redaction_policy` in the file MUST match what is actually in the payloads. The
  export path **must not** be able to emit `none` while the source session was
  recorded with a policy, and vice versa — carry the source policy, and if
  `--redact` upgrades it, apply redaction to payloads during export and set the
  field accordingly.
- `redaction_keys` retains normalized organization-specific keys when the policy
  is `custom`; import rejects a custom policy with missing or non-normalized
  keys. This is required to validate the same redaction contract later.
- `dropped_messages` is retained so importing a partial recording cannot turn it
  into a healthy session accidentally.

## Safety rules (hard)

1. **Export redacts.** If the source session's `redaction_policy == none`, export
   **requires** an explicit policy: either `--redact <policy>` on the export
   command, or `--allow-unredacted` to opt out with a loud stderr warning. Never
   silently export raw payloads. This preserves the `CONTRIBUTING.md` invariant
   "redaction before sharing."
2. **Import is local-only.** Import writes into the local SQLite db via the normal
   `Store` API; it never executes anything. Treat imported files as untrusted
   input: validate `format`/`version`, bound sizes, and reject unknown top-level
   fields under a `--strict` flag.
3. **Round-trip fidelity.** `export` then `import` then `export` must be
   byte-stable for the inner JSON (modulo `exported_at_ns`). Add a round-trip test.
4. **Derived fields are re-derived, not trusted (T-72).** `message_kind`,
   `rpc_id`, `method`, `tool_name`, `is_error`, and `error_code` are all
   computable purely from `payload` — the same derivation `record`/`replay`
   use when they first populate these columns. An artifact still carries them
   as separate fields (so a reader never needs a JSON parse to answer "was
   this an error?"), but nothing may treat that as license to trust a claim
   the artifact's own `payload` contradicts. `validate()` re-derives every one
   of these fields from `payload` and rejects the whole document if any
   disagree — atomically, before any session is written — rather than
   silently trusting or silently overwriting the claim. `direction` is the one
   exception: it is not derivable from `payload` alone (MCP is bidirectional),
   so the artifact remains its only source.
5. **Pre-export sensitive-content lint (T-63).** Key-name redaction alone
   cannot justify an absolute "safe to share" claim — see "Share-safety
   guarantee" below for the second layer this hard rule relies on.

## Share-safety guarantee (T-63)

Key-name redaction masks values under sensitive key names, but it cannot
catch the same secrets embedded in free-text strings, URL query parameters,
or PEM blocks — fields the server may return under innocuous key names like
`content`, `text`, or `url`. `export` closes that gap with a second,
always-on layer:

1. **Key-name redaction**: values under sensitive key names (`password`,
   `api_key`, `authorization`, `token`, …) are replaced with
   `"***REDACTED***"`. Tool-schema definitions (`inputSchema`/`outputSchema`)
   are exempt because they describe contracts, not runtime values.
2. **Pre-export content lint**: before writing any artifact, `export` runs
   `mcptracer_redact::sensitive_content_lint` over every message payload and
   the session's `server_command` metadata, scanning for `bearer_token`,
   `pem_block`, `url_query_secret`, and `command_secret` patterns. If any
   finding is detected, the export aborts with an error listing each
   finding's **JSON pointer and category — never the value**.
   `--allow-sensitive-content` overrides the abort with a loud stderr
   warning.

**What this does NOT guarantee:** the content lint is deterministic but
cannot detect all possible sensitive data. It will not flag:
- Secrets under non-standard key names that don't match the default list and
  weren't added via `--redact-keys`.
- Secrets embedded in binary blobs, base64 content, or custom encodings.
- Secrets embedded in structured content that doesn't match the
  bearer/PEM/URL-query patterns above.
- A `command_secret` passed as a separate argv token (`--api-key sk-123`)
  rather than `--api-key=sk-123` — only the `=`-joined form is checked, to
  avoid guessing which bare token following a flag is the flag's value.
- Secrets in session metadata fields other than `server_command`.

If you suspect an artifact may contain sensitive data, review it manually
before sharing: `gunzip < session.mtrace | python -m json.tool`.

## Versioning

- `version` is an integer. Bump on any breaking schema change. Importers must
  reject versions they do not understand with a clear error, and should support
  reading `version - 1` where feasible.
- Keep a `docs/spec/mtrace-changelog.md` once v2 is contemplated.

## Where it lives

- Serialization/deserialization types + gzip: new module `mcptracer-storage`
  (it already owns the session/message shapes) **or** a small `mcptracer-mtrace`
  crate if it grows codecs. Start in `storage` under an `mtrace` module.
- CLI: `mcptracer-proxy::commands::{export, import}`.

## Tests

- Schema round-trip: build a `SessionModel`/message set, export, import into a
  fresh in-memory `Store`, assert equality of sessions + messages.
- Safety: exporting an unredacted session without `--redact`/`--allow-unredacted`
  fails with exit non-zero and a clear message.
- Version guard: importing a file with `version: 999` fails cleanly.
- Redaction carry: exporting a `default`-redacted session yields payloads with
  `***REDACTED***` and `redaction_policy: "default"` in the file.
- Derived-field integrity: a crafted artifact whose `message_kind`, `rpc_id`,
  `method`, `tool_name`, `is_error`, or `error_code` disagrees with its own
  `payload` is rejected — one Rust test per field in `mtrace.rs`, plus a CLI
  `import` integration test proving the rejection is atomic (no partial
  session row survives).
