# Redaction

Recorded payloads can contain secrets. Redaction rewrites the **stored**
copy of each message; the bytes forwarded between client and server are
never modified.

```bash
mcptracer record --client codex --redact default -- your-mcp-server
```

`--redact default` masks values under common secret-bearing keys
(passwords, tokens, API keys, credentials, cookies). It does **not** detect
secrets embedded in free text, URLs, arrays, or command-line arguments, so
it does not make a recording safe to publish on its own. The default
policy is `none`; treat the local SQLite database as private unless you
know exactly what a given session's redaction policy did and did not
cover.

Add organization-specific field names with `--redact-keys`, which requires
the `default` policy and records the session's policy as `custom`:

```bash
mcptracer record --redact default --redact-keys tenant_id,customer-code -- your-mcp-server
```

Custom names use the same case- and separator-insensitive matching as
built-in keys, so `tenant_id` also masks `tenantId`. The normalized custom
key list is stored with the session, so `mcptracer validate` can verify the
recorded payloads actually obey the policy that was active during capture.

## Where the gate applies

Every export or sharing path refuses to act on an unredacted (`policy =
none`) session unless you explicitly pass `--allow-unredacted`:

- `.mtrace` export ([The `.mtrace` format](../spec/mtrace-format.md))
- OpenTelemetry export ([OpenTelemetry export](../spec/otel-export.md))
- semantic-search indexing ([Semantic search](../spec/semantic-search.md))

`replay` and `bench` are the one place redaction does **not** stay silent
in your favor: if the source session was recorded with redaction, they send
the stored payload as-is to a live target, so a redacted argument is sent
as the literal `***REDACTED***` placeholder rather than the original value.
Both commands scan for this and print an unmissable warning before running
— see [Replay](../spec/replay.md#redaction-interaction).
