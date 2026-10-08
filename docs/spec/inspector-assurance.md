# Inspector export and diff safety

The authenticated inspector exposes read-only endpoints for session artifacts
and comparisons. These routes use the same safety preconditions as their CLI
counterparts.

## Export

`GET /api/sessions/{id}/export` uses the shared validated artifact builder,
including source-policy redaction and sensitive-content lint. If the recording
has no redaction policy, the request fails with HTTP 400 unless the caller sends
`allow_unredacted=true`. Sensitive-content findings fail with HTTP 400 unless
the caller separately sends `allow_sensitive_content=true`. The response lists
JSON pointers and finding categories, never sensitive values.

The inspector asks for each override in a separate browser confirmation and
retries with the corresponding query parameter only after the user accepts.
Other export failures are shown without retry. The authentication token remains
in the Authorization header and is never placed in the URL.

## Diff

`GET /api/diff/{id1}/{id2}` requires both sessions to pass the shared
`require_healthy_session` gate before comparison. If either is incomplete,
dropped, or otherwise invalid, the endpoint returns HTTP 400 with the sanitized
health summary; it does not return an ordinary diff that can be read as
functional equivalence.

Healthy sessions retain the existing diff response. The inspector’s host,
origin, authentication, cache and content-security protections apply to both
routes.

## Verification

Unreleased hardening: detail includes `capture_health.healthy` and
`capture_health.issues`, computed by the same assessment as `validate` over
the returned summary/messages. The UI displays an incomplete/invalid banner
and dropped count even when individual exchanges have OK statuses. Missing
assessment is unknown, never healthy. Issue details are not copied into it.

Exchange lookup is indexed once per detail response. Request/response maps
preserve first-match behavior for duplicate sequence references. Filtering
reuses the maps, and payload formatting occurs only on first expansion.
Small growth fixtures check visits and rendered status/latency, without timing
thresholds or a dashboard rewrite.

`commands::inspect` tests exercise direct handlers and authenticated HTTP
requests for default refusal, explicit overrides, sensitive-content diagnostics
and an unhealthy diff input. Payload secret values must not appear in refusal
responses.

Real-process local fixtures compare capture from healthy and malformed JSON,
truncated HTTP, and finite SSE with/without the required event delimiter.
Clean HTTP EOF alone does not turn an unfinished SSE event into complete
evidence. Forwarded status/headers/received bytes stay unchanged, failed prefixes
do not become complete message rows, CLI/detail health agree, and unhealthy
self-diffs return HTTP 400.
