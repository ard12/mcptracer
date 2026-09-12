#!/usr/bin/env bash
# Canonical MCPTracer demo: a tool-contract rug-pull that's invisible in the
# transcript but caught by `diff` and `assert --spec pin.toml`. See README.md
# for the full narrative; this script just runs it end to end.
set -euo pipefail
cd "$(dirname "$0")"

BIN="${MCPTRACER_BIN:-mcptracer}"
PY="${PYTHON:-python3}"
command -v "$PY" >/dev/null 2>&1 || PY=python
WORKDIR="$(mktemp -d)"
DB="$WORKDIR/demo.db"
trap 'rm -rf "$WORKDIR"' EXIT

extract_session_id() {
  # Pull the session id out of `[mcptracer] recording session <id> -> <db>`.
  sed -n 's/.*recording session \([^ ]*\) ->.*/\1/p' "$1"
}

echo "== Recording the trusted baseline =="
BASELINE_LOG="$WORKDIR/baseline.log"
"$BIN" --db "$DB" record --redact default --client demo -- "$PY" server.py \
  < client_script.jsonl > "$WORKDIR/baseline.out" 2>"$BASELINE_LOG"
cat "$BASELINE_LOG" >&2
BASELINE=$(extract_session_id "$BASELINE_LOG")
echo

echo "== Validating and pinning the trusted contract (should PASS) =="
"$BIN" --db "$DB" validate "$BASELINE"
MANIFEST="$WORKDIR/baseline-evidence.json"
ARTIFACT="$WORKDIR/trusted-baseline.mtrace"
"$BIN" --db "$DB" assert "$BASELINE" --spec pin.toml --manifest "$MANIFEST"

echo
echo "== Exporting redacted evidence and verifying it offline =="
"$BIN" --db "$DB" export "$BASELINE" --out "$ARTIFACT"
"$BIN" verify "$MANIFEST" --artifact "$ARTIFACT" --assert-spec pin.toml
echo

echo "== Six months later: the package silently updates =="
CANDIDATE_LOG="$WORKDIR/candidate.log"
MCPTRACER_DEMO_RUG_PULLED=1 "$BIN" --db "$DB" record --redact default --client demo -- "$PY" server.py \
  < client_script.jsonl > "$WORKDIR/candidate.out" 2>"$CANDIDATE_LOG"
cat "$CANDIDATE_LOG" >&2
CANDIDATE=$(extract_session_id "$CANDIDATE_LOG")
echo

echo "== diff catches it even though the call/response text never changed =="
set +e
"$BIN" --db "$DB" diff "$BASELINE" "$CANDIDATE" --ignore-latency
DIFF_EXIT=$?
set -e
echo "(diff exit code: $DIFF_EXIT - 1 means a meaningful difference was found)"
echo

echo "== assert --spec pin.toml catches it deterministically (for a CI gate) =="
set +e
"$BIN" --db "$DB" assert "$CANDIDATE" --spec pin.toml
ASSERT_EXIT=$?
set -e
echo "(assert exit code: $ASSERT_EXIT - 1 means the pinned contract no longer matches)"
echo

if [ "$DIFF_EXIT" -eq 1 ] && [ "$ASSERT_EXIT" -eq 1 ]; then
  echo "Demo complete: both diff and assert caught the rug-pull."
  exit 0
else
  echo "Demo did not behave as expected (diff_exit=$DIFF_EXIT assert_exit=$ASSERT_EXIT)." >&2
  exit 1
fi
