# Spec: Opt-in tool schema change explanations (T-105)

Status: **initial contract reviewed; private local acceptance passed; current-candidate and hosted verification open**. This is a
presentation layer over the existing tool-schema security finding. It does not
change tool hashes, `DiffReport::is_empty()`, failure policy, or baseline
approval.

## Contract boundary

- Explanations do not change default diff fields or exit status. The separate
  stdio cancellation addition moves JSON output to `diff-report.v4`;
  `assert --golden --json` uses the same version. Published v3 is retained.
- `diff --explain-schema` is opt-in. Human output adds an explanation section.
  With `--json`, output is a separate versioned envelope
  `schema-explanation-report.v1` (validated by
  `schemas/schema-explanation-report.v1.schema.json`) containing the existing
  version-4 diff report plus `schema_explanations`. Validate the nested report
  separately against `diff-report.v4.schema.json`.
- The authenticated inspector diff route accepts `?explain_schema=true`. Its
  default response remains unchanged; the opt-in response adds the same
  explanations and a schema-version marker. Keep the browser surface opt-in.
- CI can consume the opt-in JSON report as an artifact. Existing SARIF and
  GitHub check-run payload shapes and redaction rules remain unchanged.
- Explanations are advisory. They do not alter the existing security finding,
  the diff exit code, `assert`, or `tools_pinned` behavior. An unclassified
  explanation never makes a report empty or approves a change.

## Version-1 explanation shape

Each explanation has:

- `tool`: exact tool name;
- `schema`: `input` or `output`;
- `pointer`: RFC 6901 pointer relative to the corresponding schema root;
- `rule_id`: stable lower-case identifier;
- `classification`: `potentially_breaking`, `potentially_non_breaking`,
  `ambiguous`, or `unclassified`;
- `before_summary`, `after_summary`: deterministic, redacted constraint
  summaries;
- `reason`: a short deterministic explanation;
- optional `unsupported_keywords`: keyword names only, never their values.

The envelope carries `schema_version: 1`, `diff_schema_version: 4`, catalog
completeness (`baseline` and `candidate`), an analysis status, the current v4
`DiffReport`, and the ordered explanations. Do not add fields to
`diff-report.v4.schema.json` merely to add explanations. Do not mutate published
v3; the new v1 explanation envelope has not yet been published.

### Initial supported subset

Traverse object schemas through `properties` to a maximum depth of 16 and 2,048
visited nodes per tool/schema. Emit at most 100 explanations per report; if a
limit is reached, set `analysis_status: "truncated"` and include an explicit
unclassified limit finding. Sort properties, findings and rule IDs
lexicographically for deterministic output.

Support only:

| Keyword | Input-schema interpretation | Output-schema interpretation |
| --- | --- | --- |
| `required` membership | Additions are potentially breaking; removals potentially non-breaking | Ambiguous; consumer expectations vary |
| `type` | Narrowing is potentially breaking; widening potentially non-breaking; incomparable changes ambiguous | Ambiguous |
| `enum` | Narrowing is potentially breaking; widening potentially non-breaking; overlapping changes ambiguous | Ambiguous |
| boolean `additionalProperties` | `true` to `false` is potentially breaking; `false` to `true` potentially non-breaking | Ambiguous |

Example rule IDs include `input.required.added`, `input.type.narrowed`,
`input.enum.widened`, `input.additional_properties.closed`, and their `output.`
variants. A claim of compatibility is never unconditional: the classification
means only the likely direction of contract pressure, not that every client or
server remains compatible.

Enum summaries include cardinality and state that values are withheld; raw enum
members are never serialized. Type names and boolean additional-property state
may be shown. Never include examples, defaults, descriptions, annotation
contents, enum members, message payloads, or secret values in this explanation
object. Do not hash low-entropy values as a substitute for redaction.

## Unsupported and incomplete input

`$ref` (including local/recursive references), schema-valued
`additionalProperties`, combinators (`allOf`, `anyOf`, `oneOf`, `not`),
conditionals, `const`, `pattern`, `format`, numeric/length bounds, custom
vocabularies, invalid keyword shapes, and unknown changed keywords are not
classified in version 1. Emit `unclassified` with the pointer and keyword names;
never call the change benign. Description-only and annotation-only changes stay
in the existing security list but receive no semantic compatibility label.

Analyze schemas only when both reconstructed `tools/list` catalogs are
complete. Otherwise return `analysis_status: "incomplete_catalog"` and no
schema-level compatibility claims. Added/removed tools remain handled by the
existing findings. If any traversal budget is exhausted, retain existing
findings and exit status while marking the analysis truncated.

The existing `tools/list` security finding remains authoritative for gating.
This analyzer must not infer missing schemas, resolve references over the
network, execute code, use embeddings/LLMs, guess tool renames, or convert an
unsupported change into a pass.

## Labeled corpus and reporting

Keep synthetic before/after fixtures for each supported rule, both directions,
and ambiguous/unclassified cases. Include nested and escaped property names,
benign description rewording, changed negation/numbers in descriptions, and a
risky annotation change. Description/annotation fixtures prove that existing
findings remain visible and are not silently labeled compatible; they are
outside the version-1 schema classifier. Report expected/observed outcomes,
false-positive/false-negative counts by implemented rule, and the fixture
count. These counts describe only this synthetic corpus, never real-world
accuracy.

Required focused evidence:

1. A required input property added between complete catalogs produces the exact
tool, `/properties/<name>` path, old/new requirement summaries, a stable rule
ID, and `potentially_breaking`.
2. Type, enum and boolean `additionalProperties` narrowing/widening cases match
   the table; input and output direction differ as specified.
3. Unsupported constructs, incomplete catalogs and traversal limits are
   explicitly unclassified/incomplete/truncated.
4. A benign rewording and a changed description number/negation remain visible
   as existing tool-description changes, without an invented semantic verdict.
   Risky annotation changes remain visible as existing annotation findings.
5. Default CLI/inspector schemas and exit codes are unchanged; opt-in CLI JSON
   validates against its new v1 schema; inspector default/opt-in HTTP responses
   follow the documented contract; SARIF/check-run output is unchanged.

## Deferred work

Version 1 does not attempt natural-language intent classification, description
synonym/rephrasing detection, annotation risk grading, schema-reference
resolution, or a general JSON Schema compatibility engine. Add such rules only
with separately labeled synthetic corpora, bounded false-positive review, and
an explicit report/schema version decision.
