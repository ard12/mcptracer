# Spec: Named Local Baselines

Status: **shipped** (backlog T-74). This is the baseline-registry half of
T-74; the versioned-schema and JUnit/SARIF/GitHub-annotations half — deferred
when this was first written — has since shipped too, in
[`ci-output-contracts.md`](ci-output-contracts.md). Depends on the canonical
digest ([`artifact-verification.md`](artifact-verification.md)).

## Goal

Let a CI workflow resolve "the approved baseline for this project, scenario,
and environment" without ever copying a transient session id into a config
file or a workflow YAML. A session id is an implementation detail of one
recording; a baseline is a durable, addressable pointer that moves forward
deliberately as new baselines are reviewed and approved.

```bash
mcptracer baseline candidate my-app smoke-test ci <session-id>
mcptracer baseline promote my-app smoke-test ci <session-id> --by alice --reason "initial baseline" --redact default
mcptracer baseline resolve my-app smoke-test ci
mcptracer assert "$SESSION" --golden "$(mcptracer baseline resolve my-app smoke-test ci)"
```

## Addressing and lifecycle

A baseline is addressed by the triple `(project, scenario, environment)` —
three free-form strings the caller defines (e.g. `my-app`, `smoke-test`,
`ci`). Nothing about MCPTracer constrains their meaning beyond "the same
triple should mean the same thing across calls."

Each row in the local `baselines` table moves through a fixed lifecycle:

```text
candidate  --promote-->  approved  --revoke-->  revoked
                             |
                             | (a different candidate is promoted
                             |  for the same triple)
                             v
                         superseded
```

- **`candidate`**: `mcptracer baseline candidate <project> <scenario> <environment> <session-id>` registers a session as a candidate. Multiple simultaneous candidates for the same triple are allowed — nothing about registering a candidate affects which baseline, if any, is currently approved.
- **`approved`**: `mcptracer baseline promote <project> <scenario> <environment> <session-id> --by <actor> --reason <text>` promotes a *registered* candidate matching that exact `(project, scenario, environment, session_id)`. Promoting fails with a clear error if no such candidate was registered first — there is no way to approve a session that wasn't explicitly staged as a candidate. Promotion is the only point at which a canonical content digest (`mtrace::canonical_digest`, computed via the same `export_mtrace_document` path `export` uses, so `--redact`/`--allow-unredacted` apply the identical safety rule) is captured and recorded, alongside the actor label and reason — matching the accept criterion "promotion records actor label, artifact digest, and reason." Promoting a new candidate atomically supersedes whatever was previously `approved` for the same triple: at most one baseline is ever `approved` for a given `(project, scenario, environment)` at a time.
- **`superseded`**: set automatically, only by a new promotion for the same triple. Not reachable any other way.
- **`revoked`**: `mcptracer baseline revoke <project> <scenario> <environment> --reason <text>` revokes the currently `approved` baseline for that triple, if any. After this, `resolve` finds nothing for that triple until a new baseline is promoted — there is no implicit fallback to an older superseded or candidate row.

## Resolution (`mcptracer baseline resolve`)

```bash
mcptracer baseline resolve <project> <scenario> <environment> [--json]
```

Finds the row in state `approved` for the triple and prints its `session_id`
— and nothing else — to stdout in plain-text mode, so it composes directly
into command substitution:

```bash
mcptracer assert "$SESSION" --golden "$(mcptracer baseline resolve my-app smoke-test ci)"
```

This is the one lookup a CI workflow should use instead of hardcoding a
transient session id, directly satisfying the accept criterion "a CI
workflow resolves an approved baseline without copying a transient session
id." `--json` prints the full `Baseline` record (state, digest, promoted_by,
promoted_at, promotion_reason, created_at) for anything that wants more than
just the id. Resolution fails with a clear error — not an empty/`null`
result — if nothing is currently approved for that triple, whether because
nothing was ever promoted or because the last approved baseline was revoked.

## Storage

New `baselines` table (schema migration v5), one row per
candidate/promotion/lifecycle transition:

```sql
CREATE TABLE baselines (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project TEXT NOT NULL,
    scenario TEXT NOT NULL,
    environment TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    state TEXT NOT NULL,              -- candidate|approved|superseded|revoked
    digest TEXT,                      -- set only at promotion
    promoted_by TEXT,
    promoted_at INTEGER,
    promotion_reason TEXT,
    revoked_at INTEGER,
    revoked_reason TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_baselines_lookup ON baselines(project, scenario, environment, state);
```

`session_id` cascades on session deletion, matching `messages`' existing
foreign key — a baseline pointing at a session that no longer exists is not
a state this schema tries to represent.

## The other half of T-74

T-74's backlog entry also calls for "publish versioned JSON Schemas for
validate/diff/assert/eval/bench evidence results" plus "add JUnit/SARIF/
GitHub annotations as adapters." That work shares essentially no
implementation surface with the baseline registry above and was deliberately
done as its own focused pass rather than compressed into this one — see
[`ci-output-contracts.md`](ci-output-contracts.md) for the schemas, the
golden conformance test, and the three CI-format adapters.

## Tests

- `mcptracer-storage` unit tests: fresh database has the `baselines` table;
  candidate-then-promote records digest/actor/reason; promoting without a
  registered candidate fails clearly; promoting a new baseline supersedes
  the previous approved one; resolving with nothing approved fails clearly;
  revoke clears the approved state and records reason/timestamp; revoking
  with nothing approved fails clearly; listing filters by project.
- `tests/test_proxy_integration.py::test_baseline_lifecycle_and_ci_resolve_composition`:
  end-to-end through the real CLI — resolving before promotion fails,
  candidate registration, promotion (with digest/actor/reason recorded and
  visible via `--json`), the exact `assert --golden "$(baseline resolve
  ...)"` composition pattern this feature exists for (proving both a
  matching and a differing session behave correctly through it), promoting
  a second candidate supersedes the first, and revocation clears
  resolution.
