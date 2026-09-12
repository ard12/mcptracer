# Spec: Assertions + CI

Status: **shipped.** Depends on the session model and diff
([`session-model.md`](session-model.md), [`diff.md`](diff.md)). This is the
core CI-integration surface — treat the assertion spec format as a stable,
public contract. See [`artifact-verification.md`](artifact-verification.md)
for `--manifest` and offline `verify`.

## Goal

Let MCP server authors write declarative checks over a recorded (or replayed)
session and run them in CI with a meaningful exit code.

```bash
mcptracer assert <session-id> --spec checks.toml
mcptracer assert <session-id> --golden golden-session-id     # snapshot mode
```

Two complementary modes:

1. **Rule mode** — evaluate a declarative spec of assertions against one session.
2. **Snapshot mode** — `assert` is sugar over `diff <golden> <session>` with exit
   code semantics (regression gate). Reuse the diff engine; do not reimplement.

## Spec format (rule mode)

TOML (matches the ecosystem; easy to hand-write and review). Start with a small,
composable set of assertion kinds. `[[assert]]` is canonical; `[[assertions]]`
is accepted as an alias. Every entry may carry an optional
`description = "..."` label, which replaces the auto-generated text in
PASS/FAIL output. Example:

```toml
# checks.toml
[[assert]]
kind = "no_errors"                 # no exchange has status Error/Unanswered

[[assert]]
kind = "tool_called"
tool = "read_file"                 # tools/call with this name occurred
min = 1                            # optional count bounds
max = 3

[[assert]]
kind = "call_order"
before = "initialize"
after = "tools/call"               # every tools/call happens after initialize

[[assert]]
kind = "latency"
tool = "search"
p95_under_ms = 500

[[assert]]
kind = "response_matches"
tool = "read_file"
pointer = "/result/content/0/type"
equals = "text"
```

Assertion kinds for v1 (each maps to a check over `SessionModel`):

| kind | meaning |
| --- | --- |
| `no_errors` | zero `Error`/`Unanswered` exchanges (or scope to a tool). |
| `tool_called` | a `tools/call` for `tool` occurred; optional `min`/`max`. |
| `method_called` | a given method occurred; optional count bounds. |
| `call_order` | all `after` occurrences follow at least one `before`. |
| `latency` | `p95`/`max`/`p50` for a tool/method under a bound (uses proxy latency; document the caveat). |
| `response_matches` | JSON-pointer value in a response equals/contains an expected value; honors redaction (a redacted value neither passes nor fails an equality — it errors as "cannot assert on redacted field" unless `--allow-redacted-skip`). |

Keep the set small and orthogonal. Adding kinds later is cheap; removing them
breaks users, so do not over-ship.

## Output and exit codes

- Print one line per assertion: `PASS`/`FAIL` + a human reason.
- Summary line: `N passed, M failed`.
- **Exit `0`** iff all pass; **exit `1`** on any failure; **exit `2`** on spec
  errors (malformed TOML, unknown kind) so CI can distinguish "test failed" from
  "test is broken."
- `--json` emits a machine-readable result array for dashboards.

## GitHub Action

Ship `.github/actions/mcptracer` (composite action) and document a usage snippet:

```yaml
- uses: ard12/mcptracer/.github/actions/mcptracer@v1
  with:
    record: "python my_server.py"      # command to record against
    spec: ".mcptracer/checks.toml"
```

The action: installs the released binary (from the release workflow artifacts),
records or replays a session, runs `assert`, and fails the job on non-zero exit.
Keep the action thin — all logic lives in the binary so it is testable locally.

## Where it lives

- Assertion evaluation (pure, over `SessionModel`): `mcptracer-model` or a
  `mcptracer-assert` module. Lean toward a module in `model`.
- Spec parsing (TOML → typed rules): its own small module with thorough parse
  tests, including bad-spec cases that must exit `2`.
- CLI wiring + output: `mcptracer-proxy::commands::assert`.

## Tests

- Each assertion kind: one passing and one failing fixture over a hand-built
  `SessionModel`.
- Spec parsing: valid specs round-trip; malformed specs produce exit-`2`-class
  errors with clear messages.
- Snapshot mode: golden vs. changed session → fail; golden vs. identical → pass
  (delegates to diff, so mostly an integration-level check).
