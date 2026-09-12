#!/usr/bin/env bash
# Canonical MCPTracer demo: a latency regression caught by an explicit,
# numeric `assert kind = "latency"` release gate. See README.md for the full
# narrative; this script just runs it end to end.
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

echo "== Recording the current release (fast) =="
BASELINE_LOG="$WORKDIR/baseline.log"
"$BIN" --db "$DB" record --client demo -- "$PY" server.py \
  < client_script.jsonl > "$WORKDIR/baseline.out" 2>"$BASELINE_LOG"
cat "$BASELINE_LOG" >&2
BASELINE=$(extract_session_id "$BASELINE_LOG")
echo

echo "== Gating the release: search must stay under 100ms (should PASS) =="
"$BIN" --db "$DB" assert "$BASELINE" --spec checks.toml
echo

echo "== A routine-looking change adds a slow synchronous lookup =="
CANDIDATE_LOG="$WORKDIR/candidate.log"
MCPTRACER_DEMO_SLOW_MS=250 "$BIN" --db "$DB" record --client demo -- "$PY" server.py \
  < client_script.jsonl > "$WORKDIR/candidate.out" 2>"$CANDIDATE_LOG"
cat "$CANDIDATE_LOG" >&2
CANDIDATE=$(extract_session_id "$CANDIDATE_LOG")
echo

echo "== Gating the next release: same check catches the regression =="
set +e
"$BIN" --db "$DB" assert "$CANDIDATE" --spec checks.toml
ASSERT_EXIT=$?
set -e
echo "(assert exit code: $ASSERT_EXIT - 1 means the latency gate caught a regression)"
echo

echo "== diff shows the same story with a latency delta, not just pass/fail =="
set +e
"$BIN" --db "$DB" diff "$BASELINE" "$CANDIDATE"
DIFF_EXIT=$?
set -e
echo "(diff exit code: $DIFF_EXIT)"
echo

if [ "$ASSERT_EXIT" -eq 1 ]; then
  echo "Demo complete: the latency gate caught the regression before release."
  exit 0
else
  echo "Demo did not behave as expected (assert_exit=$ASSERT_EXIT)." >&2
  exit 1
fi
