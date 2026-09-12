# Spec: `mcptracer semantic` — Local Lexical Search

Status: **shipped, experimental, off by default.** Depends on the derived
index ([the derived-index architecture](../architecture/overview.md#derived-local-analysis),
T-50). Build-gated behind the `semantic-search` Cargo feature on both
`mcptracer-intel` and `mcptracer-proxy`; the default build does not include
this code at all.

## What this is, and what it is not

**This is not a neural embedding model.** There are no model weights bundled
or downloaded, and nothing here makes a network call. The project's rule is
that no intelligence feature may sit in the protocol hot path or depend on a
live model; a "local embedding provider" for a project with that constraint
has to actually be local — no ONNX runtime, no vendored weights, no
first-run download.

What it is: a normalized term-frequency vector (`embed_text`) over indexed
document text, compared with cosine similarity — classic pre-neural
"vector search." The "semantic-ish" part — a query like **"rug pull"**
finding a tool that has drifted, despite sharing no literal words with the
indexed text — comes entirely from [`DOMAIN_GLOSSARY`] in
`crates/mcptracer-intel/src/semantic.rs`: a small, explicit, auditable table
mapping this project's own security vocabulary onto the descriptive keywords
`build_corpus` writes into indexed documents (`"description changed schema
drift version supersedes security finding"` for a tool with a
`version_supersedes` edge). It is a curated glossary, not a learned
representation. Anyone reading the source sees exactly why a query matched.

## Indexed corpus

Two document kinds, built from tables the derived index (T-50) already
maintains — no raw payload text is available to index in the first place;
`tool_versions.description_hash`/`schema_hash` are hashes, not text:

- **session** — `"session <id> client <client> transport <transport>"`.
- **tool_version** — `"tool <name> version"`, plus drift keywords when the
  version participates in a `version_supersedes` edge on either side (it was
  superseded, or it superseded something).

## Redaction gate

Before indexing, every session's `redaction_policy` is checked. A session
recorded with policy `none` causes the whole command to refuse with a clear
error, unless `--allow-unredacted` is passed — the same gate `.mtrace`
export uses. This is defense-in-depth rather than a response to anything
actually unsafe in the current corpus (derived facts and tool versions never
carry raw payload text, per `mcptracer-intel`'s crate-level safety
properties) — it keeps the policy story consistent with the rest of the
codebase and leaves room for a future corpus that does need it.

## Provenance

Every search hit carries `provider: "local-lexical-v1"`. A future remote
provider (not implemented — the accept criteria for this task only cover the
local provider) must use its own distinct provider string and its own
explicit unsafe opt-in flag; this field is the seam that makes results from
different providers distinguishable rather than silently mixed.

## CLI

```bash
mcptracer semantic "rug pull"
mcptracer semantic "rug pull" --json
mcptracer semantic "rug pull" --allow-unredacted --limit 20
```

Requires building with `--features semantic-search`
(`cargo build -p mcptracer-proxy --features semantic-search`); the command
does not exist in a default build.
