# Spec: Evidence Manifest and Offline Verification

Status: **shipped** (backlog T-73, hardened in T-73A). Depends on the
`.mtrace` format ([`mtrace-format.md`](mtrace-format.md)) and `assert`.

## Goal

Let a CI pipeline (or a human reviewing a bug report later) confirm, entirely
offline — no database, no network, no re-running anything — that a specific
`.mtrace` artifact is the exact evidence that produced a specific `assert`
outcome, and detect if either the artifact or the assertion spec it was
checked against has changed since.

```bash
mcptracer assert <session-id> --spec checks.toml --manifest evidence.json --allow-unredacted
mcptracer verify evidence.json --artifact session.mtrace --assert-spec checks.toml
```

## Canonical content digest

`mcptracer_storage::mtrace::canonical_digest` computes a SHA-256 over a JSON
value containing exactly `{format, version, session, messages}` — deliberately
excluding `exported_at_ns` and `exporter`. Those two fields describe the
export transaction (when, by which build), not the underlying recorded
evidence; including them would make the digest change every time someone
re-exports an unchanged session, which defeats the purpose of a *content*
identity digest.

JSON object key order never affects this digest. The workspace's `serde_json`
dependency is built without the `preserve_order` feature, so `serde_json::Map`
is backed by a `BTreeMap` and `to_string` always emits sorted keys regardless
of parse-time order — the same property `mcptracer-intel`'s tool-version
hashing and `mcptracer-model::diff::tool_hash` already rely on, applied here
to the whole document rather than one tool definition. Array order (message
sequence, and any array inside a payload) is preserved exactly and does
affect the digest, so reordering or truncating `messages` is detected, per
the accept criteria.

The same function computes the digest whether the source is a freshly-built
in-memory document (a live session, via `Store::export_mtrace_document`) or
one just decoded from an on-disk `.mtrace` file — they agree exactly for the
same content, which is what makes cross-machine, cross-time verification
possible at all.

## Evidence manifest (`mcptracer_model::manifest::EvidenceManifest`)

```jsonc
{
  "schema_version": 1,
  "mcptracer_version": "0.2.0",
  "generated_at_ns": 1730000000000000000,
  "artifact": { "sha256": "…64 hex chars…" },
  "baseline": { "sha256": "…" },        // present only for `assert --golden`
  "assertion_spec": { "sha256": "…" },  // present only for `assert --spec`
  "outcome": "pass",                    // or "fail"
  "signed": false
}
```

- `artifact` is the canonical content digest of the session `assert` checked.
- `baseline` (golden-snapshot mode only) is the same digest computed over the
  golden session.
- `assertion_spec` (rule mode only) is a plain SHA-256 over the assertion
  TOML file's raw bytes — not the canonical-document digest, since a spec
  file isn't a `.mtrace` document.
- Exactly one of `baseline`/`assertion_spec` is present, matching `assert`'s
  own `--golden`/`--spec` being mutually exclusive.
- No field records a filesystem path. The machine that verifies a manifest
  is very often not the machine that generated it, so a recorded path would
  be meaningless or actively misleading; the verifier supplies its own local
  path to whatever file it wants to check.
- `signed` is always `false` in schema version 1 — see below.

## Manifest validation (T-73A)

`EvidenceManifest::from_json` is the only supported way to load a manifest
from untrusted input, and it always validates before returning — there is no
"parse without validating" entry point a caller could accidentally use
instead. It rejects:

- **`schema_version` other than the one this build understands.** No
  best-effort interpretation of an unrecognized version.
- **`signed: true`.** This build has no signature-checking code at all;
  accepting that claim at face value would let a manifest assert a guarantee
  nothing here actually checks. A manifest making this claim is refused
  outright, not silently downgraded to "treat as unsigned."
- **Both `baseline` and `assertion_spec` present, or neither.** Exactly one
  must be present, matching `assert`'s own `--golden`/`--spec` mutual
  exclusivity — a manifest with both or neither does not correspond to any
  real `assert` invocation and is therefore malformed by construction, not
  merely unusual.
- **Any digest that isn't exactly 64 lowercase hex characters** — the exact
  shape `ManifestDigest::of` always produces. A digest of any other shape
  cannot possibly equal a real one, so treating it as "just a mismatch" would
  bury a malformed-file signal inside an ordinary-looking `MISMATCH` line.

`assert --manifest` calls `validate()` on its own output before writing it,
the same way `mtrace::encode` validates before compressing — defense in
depth against a bug in construction, not just against hostile files read
back in later.

`mcptracer assert --manifest <path>` writes this file (never overwriting an
existing one, and hardened to `0600` on Unix via the same `mtrace::write_file`
export uses — see [T-61 in `SECURITY.md`](../../SECURITY.md)) after computing
the pass/fail outcome. `--redact`/`--allow-unredacted` on `assert` apply the
exact same rule `export` already enforces for computing a digest over a
session: a session recorded with `redaction_policy: none` needs an explicit
decision before its content is hashed for a manifest, same as before it's
exported to a file. This is deliberate — it means a manifest's `artifact`
digest always matches what a real `export` of that same session would
produce, so `verify` can check a manifest against either an artifact that
was exported before or one exported after the fact.

## Offline verification (`mcptracer verify`)

```
mcptracer verify <manifest> --artifact <path> [--baseline <path>] [--assert-spec <path>] [--allow-partial]
```

Takes no `--db` — it reads the manifest and the local files given on the
command line only. The manifest is loaded via `EvidenceManifest::from_json`,
so every invariant above is enforced before any file comparison happens —
a manifest claiming `signed: true`, or otherwise malformed, is refused
outright rather than partially processed.

**Verification is complete by default (T-73A).** If the manifest records a
`baseline` or `assertion_spec` digest and the corresponding `--baseline`/
`--assert-spec` file wasn't given, `verify` fails immediately with a clear
error naming the missing flag — it does not silently check only the
`artifact` digest and report success. A CI script that only checks the exit
code must not be able to be fooled into believing everything was verified
when part of the manifest was never actually checked. Pass `--allow-partial`
to explicitly opt into skipping a recorded digest; in that mode the skipped
digest is reported `SKIPPED` and prefixed with an unmissable "partial
verification" warning, but the run can still exit `0` if everything it did
check matched. Passing a file for a digest the manifest never recorded is
always an error (there is nothing to check it against), regardless of
`--allow-partial`.

For each digest actually checked, `verify` recomputes that file's digest
(via `canonical_digest` for `.mtrace` files, plain SHA-256 for the assertion
spec) and reports `MATCH` or `MISMATCH`. Exit code `0` only if every checked
digest matched (and, without `--allow-partial`, every manifest-recorded
digest was checked); `1` on any mismatch or unacknowledged partial
verification — the CLI exit code remains this project's primary CI signal,
matching `diff`/`assert`.

`manifest.signed` is guaranteed `false` by the time `verify`'s reporting code
runs — `from_json` already rejected anything else — so the "signature:
verified" message is structurally unreachable in this schema version. The
code path that would print it is guarded by an `unreachable!()` rather than
simply omitted, so a future change that tried to loosen the `signed: true`
rejection without also implementing real signature checking would panic
immediately instead of silently starting to print a false claim.

## Why signing is out of scope for v1

`signed` is hardcoded to `false` everywhere in this schema version; nothing
in `mcptracer-model::manifest` can set it to `true`. This was a deliberate
scoping decision, not an oversight:

- A digest match on its own is **not** tamper-evidence. It proves the
  checked file's content matches what the manifest recorded — nothing more.
  Anyone who can edit the manifest can also recompute a matching digest for
  altered content and write both together. Real tamper-evidence needs a
  signature over the digest that ties it to a key the verifier already
  trusts *independently* of the manifest itself.
- Local key management (generation, storage, rotation, loss-of-key recovery)
  is a substantial design surface with real security tradeoffs of its own,
  and the backlog's own framing of this task — "support offline verification
  and detached local signatures **before** optional hosted signing" — treats
  local signing as a stepping stone toward a future hosted service, not a
  final design. Committing to a local-key scheme now risks having to change
  it once that hosted story exists.
- The accept criteria for this task is satisfiable, and is satisfied, without
  signing: "unsigned v1 artifacts remain readable but are explicitly reported
  unsigned; no 'tamper-evident' claim appears without cryptographic
  verification." `verify`'s output makes the unsigned status explicit and
  spells out exactly what a digest match does and doesn't prove, rather than
  implying a stronger guarantee than what's actually implemented.

Signing detached manifests (and eventually hosted signing) remains an open
follow-up, not a silently dropped requirement.

## Tests

- `mcptracer-storage::mtrace` unit tests: digest stability across
  re-export metadata and JSON key order; digest changes on payload mutation,
  session-metadata mutation, message reordering, and message truncation.
- `mcptracer-model::manifest` unit tests: digest-matches-exact-bytes,
  JSON round-trip preserves every field, omitted optional digests are
  absent from the JSON (not `null`), `signed` is always `false`, plus
  (T-73A) `from_json`/`validate` reject `signed: true`, an unsupported
  schema version, both/neither of `baseline`/`assertion_spec`, a too-short
  digest, an uppercase-hex digest, and non-hex characters in a digest — with
  a positive control confirming a well-formed manifest is still accepted.
- `tests/test_proxy_integration.py::test_assert_manifest_and_offline_verify`:
  end-to-end through the real CLI — `assert --manifest` in both spec and
  golden mode, `verify` against a matching real export (`MATCH`, exit 0) and
  against a different session's export (`MISMATCH`, exit 1), with `verify`
  invoked without `--db` to prove the offline claim; (T-73A) omitting
  `--assert-spec` for a manifest that records one is a hard error naming the
  missing flag, and `--allow-partial` explicitly opts into skipping it with
  a `SKIPPED` line and a partial-verification warning.
- `tests/test_proxy_integration.py::test_verify_rejects_hostile_manifests`
  (T-73A): hand-crafted manifest JSON files (never routed through `assert`)
  proving the real CLI binary rejects `signed: true` and never prints
  "signature: verified" for it, plus each structural-validation failure
  above — all before `verify` ever reads the `--artifact` file, which in
  this test does not even exist.
