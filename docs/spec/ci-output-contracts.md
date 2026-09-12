# Spec: Versioned Output Schemas and CI-Format Adapters

Status: **shipped** (backlog T-74, second half; the GitHub check-run adapter
added in T-83). Depends on [`baselines.md`](baselines.md), `diff`, `assert`,
`eval`, `bench`, `validate`.

## Goal

`--json` output from `validate`/`diff`/`assert`/`eval`/`bench` is already a
documented interface — the stability page calls out that it "won't change
gratuitously, but a field can be added." This spec makes that contract
concrete and machine-checkable: a published JSON Schema per command, and a
test that fails the moment real CLI output stops matching what's published,
so a breaking change is caught before it ships rather than discovered by
whoever's script broke. It also adds JUnit XML, SARIF, and GitHub Actions
annotation adapters so a CI system doesn't have to hand-roll a JSON
consumer just to get a green/red check.

## Published schemas (`schemas/*.vN.schema.json`)

Six JSON Schema (draft 2020-12) documents, one per distinct output shape:

| File | Emitted by |
|---|---|
| `session-integrity-report.v2.schema.json` | `validate --json` |
| `diff-report.v3.schema.json` | `diff --json`, and `assert --golden --json` (same `DiffReport` type) |
| `assert-results.v2.schema.json` | `assert --spec --json` (an object, `{"schema_version": 2, "results": [...]}`; v1 was a bare JSON array) |
| `eval-report.v2.schema.json` | `eval --json` |
| `bench-report.v2.schema.json` | `bench --json` |
| `quota-report.v2.schema.json` | `quota --json` (single-session and fleet variants) |

Every schema sets `additionalProperties: false` at every object level. This
is intentionally stricter than "ignore fields you don't recognize" — the
point of the golden test below is to catch *any* shape drift, including
adding a field, so it can be reviewed and the schema deliberately
version-bumped, rather than silently landing. **Versioning:** a breaking
change adds a new `<name>.vN.schema.json` file; an already-published file is
never edited in place once published, so an archived document still validates
against the version it was produced under.

Be precise about what that does and does not buy you. A retained `.vN` file
describes the shape a *past release* emitted — it does not keep the current
CLI emitting that shape. Each command emits exactly one version: the newest.
So a CI job pinned to `diff-report.v2.schema.json` does not keep working after
upgrading to a build that emits v3; its validation starts failing, which is
the intended loud signal rather than a silent shape change. To stay on an
older contract, pin the **binary** version, not just the schema file. See the
migration note in `CHANGELOG.md` for what changed between versions.

**`schema_version`:** every one of these documents carries a required
`schema_version` integer field whose value is the major version of the
schema file it conforms to (`quota-report` had this from the start; the
other five gained it in the same breaking change that fixed the two enum
encodings described below). A consumer should read this field and branch
— or refuse to proceed on a version it does not understand — rather than
infer the document's shape from its content: because `additionalProperties:
false` already makes an unrecognized field a hard schema failure, an
unrecognized `schema_version` is the reliable signal that the rest of the
document may be shaped differently too. `assert --spec --json` v1 had
nowhere to put this field (a bare JSON array has no property to hold it),
which is why v2 wraps the same array in an object,
`{"schema_version": 2, "results": [...]}`, with the array's element shape
unchanged.

`ExchangeStatus` (used inside `diff-report`'s `status_changed` delta, and as
`Exchange.status` in `sessions show --json`, which is not one of the six
schema-covered outputs above) used to have no `#[serde(rename_all)]`
attribute in the Rust type, so it serialized in `PascalCase` (`"Ok"`,
`"Error"`, `"ToolError"`, `"Subscribed"`, `"Unanswered"`,
`"OrphanResponse"`) instead of the `snake_case` convention every other enum
in these schemas uses; `Direction` (`Exchange.origin` and
`NotificationEvent.direction`, likewise only reachable via `sessions show
--json`) had the same problem one level worse, serializing as
`"ClientToServer"`/`"ServerToClient"` — matching neither that convention
nor this project's own `"c2s"`/`"s2c"` wire encoding used everywhere else
(the `.mtrace` format, the SQLite `direction` column,
`Direction::as_db_str()`). Both were real, slightly surprising
inconsistencies, previously documented here rather than silently
"corrected" in the schema (which would have made the schema wrong). Both
are now fixed: `ExchangeStatus` carries `#[serde(rename_all = "snake_case")]`
like every other enum here (`"ok"`, `"error"`, `"tool_error"`,
`"subscribed"`, `"unanswered"`, `"orphan_response"`), and `Direction`
serializes as `"c2s"`/`"s2c"` — the encoding it already used everywhere
else, not `"client_to_server"`. `diff-report`'s schema bumped to v3 to
carry both the `ExchangeStatus` fix and `schema_version` in one breaking
change rather than two.

## Golden conformance test

`tests/test_proxy_integration.py::test_json_output_conforms_to_published_schemas`
runs every one of the six commands for real through the compiled CLI
binary and validates the actual stdout against the matching schema file.
There is no mocking of CLI output and no hand-maintained fixture standing
in for it — if the Rust side's serialization drifts from what's published,
this test fails.

Validation itself uses `tests/schema_validator.py`, a minimal, dependency-free
JSON Schema validator written for this test suite specifically (this
project's Python tests have zero external dependencies, and adding
`jsonschema` as one just for this would be the only thing needing it). It
implements exactly the subset of JSON Schema draft 2020-12 the schemas above
actually use — `type`, `properties`, `required`, `additionalProperties`,
`enum`, `const`, `items`, `oneOf`, local `$ref`s into `$defs`, `minimum`/
`maximum` — and is not intended as a general-purpose validator outside this
suite.

## CI-format adapters (`mcptracer-proxy::ci_formats`)

No new crate dependency: JUnit XML and a minimal SARIF 2.1.0 log are simple
enough to render directly against the existing `AssertionResult`/`DiffReport`
types with a few dozen lines of code, matching how this codebase already
prefers direct code over a dependency for something this bounded.

### `assert --junit <path>`

Writes a JUnit XML `<testsuite>` — one `<testcase>` per assertion in
`--spec` (rule) mode, with a `<failure>` child element for each failing one;
a single one-test `<testsuite>` in `--golden` (snapshot) mode, since golden
mode has no per-rule breakdown to report individually. Never overwrites an
existing file (reuses `mtrace::write_file`, so it also gets the same `0600`
hardening on Unix as every other file this project writes). Consumed by CI
systems that ingest test results as a file, e.g. GitHub Actions'
test-reporting actions or GitLab's `artifacts: reports: junit`.

### `assert --github`

Prints GitHub Actions workflow-command annotations to stdout — `::notice::
PASS <description>` or `::error::FAIL <description> - <reason>` per
assertion (one summary line for golden mode) — so results surface directly
in the GitHub Actions log UI without a separate reporting step. Message text
is escaped per GitHub's workflow-command rules (`%25`/`%0D`/`%0A`) so a
reason or description containing a newline can't be misread as the start of
a new command.

### `diff --sarif <path>`

Writes a minimal SARIF 2.1.0 log built **only** from `DiffReport.security` —
the tool-drift findings (rug-pull-style description/schema/annotation
changes). Ordinary behavioral differences (response/latency/status changes,
added/removed exchanges) are not SARIF "results": SARIF is a findings
format, and tool drift is the one part of a diff that genuinely is a
finding in that sense, not a behavioral fact to be read as prose the way
the rest of `diff`'s output is. A diff with no security drift still writes
a valid SARIF log with an empty `results` array, not an absent file — a CI
step that always runs `diff --sarif` doesn't need to special-case "nothing
to report." `ruleId` uses the same `snake_case` identifiers as `diff`'s own
JSON `security[].kind` field.

### `diff --github-check-json <path> --github-check-sha <sha> [--github-check-details-url <url>]`

Writes a GitHub "Create a check run" API payload (T-83) — JSON a CI workflow
posts itself, e.g. `gh api repos/{owner}/{repo}/check-runs --input
payload.json`. **MCPTracer never calls GitHub's API** — that needs a GitHub
App or installation token with `checks:write` this environment has no way to
exercise live, so the boundary is the same one the composite Action already
uses: MCPTracer computes evidence, the surrounding workflow talks to GitHub.
`--github-check-sha` is required alongside `--github-check-json` (clap
enforces this; GitHub's API has no meaningful default for which commit a
check applies to).

**Link evidence without exposing payloads** (T-83's own accept-criterion
wording) is enforced in what the summary includes, not left to the caller's
discipline: only *counts* of changed/added/removed exchanges ever appear —
never their before/after content. `security` findings are the one exception,
included in full, because they describe tool-*contract* metadata
(name/description/schema changes — the same T-73 rug-pull signal
`--sarif` reports) rather than message traffic. `--github-check-details-url`
lets the check link to an authenticated evidence URL instead of the check
embedding content where any PR viewer or CI log reader would see it.

## Tests

- `mcptracer-proxy::ci_formats` unit tests: JUnit test/failure counts and
  XML escaping; golden-mode single-test framing; GitHub annotation
  notice/error selection and message escaping; SARIF result count, `ruleId`,
  and the empty-findings case; GitHub check-run `conclusion`/`head_sha`/
  `details_url`, security-finding detail included in the summary, and —
  the load-bearing test — a changed exchange's actual response content
  proven absent from the summary even though its presence-as-a-count is.
- `tests/test_proxy_integration.py::test_json_output_conforms_to_published_schemas`:
  every schema-covered `--json` command's real output validated against its
  published schema.
- `tests/test_proxy_integration.py::test_junit_sarif_and_github_annotation_adapters`:
  real JUnit XML parsed and checked (test/failure counts, per-testcase
  `<failure>` presence) for both `--spec` and `--golden` modes; real stdout
  checked for `::notice::`/`::error::` lines; real SARIF JSON checked for
  shape and a genuine `tool_description_changed` finding from the fake
  server's "changed" variant, plus the empty-results case for an identical-
  session diff; a real `--github-check-json` payload checked for
  `head_sha`/`conclusion`/`details_url` and that the fake server's actual
  changed response text (`"Echo2:"`) never appears in the summary; and that
  omitting `--github-check-sha` is a clap usage error (exit 2), not a
  silent no-op.
