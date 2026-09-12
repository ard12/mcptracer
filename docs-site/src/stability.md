# Stability & Compatibility

MCPTracer is pre-1.0. This source export contains a `0.3.0-rc1` release
candidate, not a newly published public release. This page states
what's actually stable today versus what can still change, so you can decide
what to depend on in CI.

## Versioning

Follows [Semantic Versioning](https://semver.org/). Before `1.0.0`, a minor
version bump (`0.x` → `0.y`) may include breaking changes; patch bumps
(`0.x.y` → `0.x.z`) do not. Every change is recorded in
[`CHANGELOG.md`](https://github.com/ard12/mcptracer/blob/main/CHANGELOG.md)
under [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) conventions.

## What's stable

These are treated as public contracts — a change to any of them is a
breaking change, called out explicitly in the changelog:

- **The session model** ([Session model](spec/session-model.md)): the
  correlated request/response view `replay`, `diff`, `assert`, and
  `.mtrace` all consume.
- **The `.mtrace` format** ([The `.mtrace` format](spec/mtrace-format.md)):
  the portable session artifact. `mtrace_version` is checked on import;
  unknown versions fail closed rather than guessing.
- **The assertion spec format** ([Assertions & CI](spec/assertions.md)):
  the TOML contract used to write CI gates.
- **CLI exit codes**: `diff` and `assert` use exit codes as their primary
  CI signal (0 = pass, 1 = meaningful difference / assertion failures,
  2 = spec error for `assert`) — these are load-bearing for scripts and
  will not change silently.
- **The schema-migration mechanism**
  (`mcptracer-storage`'s `CURRENT_SCHEMA_VERSION` / transactional
  migrations): opening an older on-disk database always migrates forward
  in place; opening a database from a newer build than the one running
  fails with a clear error rather than corrupting it.

## What's still moving

- **CLI flags and subcommand names** can still change between `0.x`
  releases. Pin an exact version (via an exact Git revision or an exact published version once a release
  path exists) for CI
  reproducibility rather than tracking `latest`.
- **`mcptracer semantic`** ([Semantic search](spec/semantic-search.md)) is
  explicitly experimental: off by default, gated behind the
  `semantic-search` Cargo feature, and not covered by any stability
  guarantee.
- **The derived intelligence index** (`tool_versions`,
  `tool_version_observations`, `memory_facts`, `memory_edges` — everything
  `mcptracer index rebuild` populates) is a rebuildable cache, not
  source-of-truth data. Its internal schema can change across releases
  without a migration, because it is always safe to drop and rebuild from
  `sessions`/`messages`.
- **JSON output shapes** (`--json` on most commands) are not yet frozen.
  They won't change gratuitously, but a field can be added; do not assume
  an exhaustive field list in a script that parses them.




## Platform evidence

A configured matrix is not proof that a particular commit passed, so this
records runs rather than configuration. Tests, the Rust **1.85** MSRV check,
and the real-SDK compatibility matrix have all passed on Linux, macOS, and
Windows for this snapshot's source revision, including actual execution of the
Unix file-permission tests. The compatibility matrix's Python stdio cell is
still non-blocking because of a disclosed, SDK-side flake.

Evidence applies to a specific commit, not to the project in general: check the
Actions tab for the revision you intend to use. Building from source requires
Rust **1.85+** and a working C toolchain because `rusqlite` compiles bundled
SQLite.

## What "1.0" will mean

The `1.0.0` line is not tied to adding more commands. It requires frozen CLI
and versioned machine-readable contracts, exercised artifact/database migration
and deprecation policies, compatibility evidence across real MCP systems,
repeated production use, a supported security/release process, and no unresolved
critical evidence-integrity issue. No `1.0.0` tag has been pushed; track
[releases](https://github.com/ard12/mcptracer/releases) for actual releases.
