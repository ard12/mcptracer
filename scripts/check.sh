#!/usr/bin/env bash
# Full verification gate. Run this before opening a PR — CI runs the same steps.
set -euo pipefail
cd "$(dirname "$0")/.."

# Gates that were not run because their tool is missing. This script claimed
# "All checks passed" even when it had skipped some, which is exactly the kind
# of gate that reports healthy while proving less than it appears to. Pass
# --strict (or set MCPTRACER_CHECK_STRICT=1) to turn a skip into a failure,
# which is what a release candidate should use.
SKIPPED=()
STRICT="${MCPTRACER_CHECK_STRICT:-0}"
for arg in "$@"; do
    case "$arg" in
        --strict) STRICT=1 ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

skip() {
    SKIPPED+=("$1")
    echo "    SKIPPED: $1 ($2)"
}

echo "==> cargo fmt --all -- --check"
cargo fmt --all -- --check

echo "==> cargo test --workspace --all-features --locked"
cargo test --workspace --all-features --locked

echo "==> cargo clippy --workspace --all-targets --all-features --locked -- -D warnings"
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

echo "==> python tests/test_proxy_integration.py"
PYTHON="$(command -v python3 || command -v python)"
"$PYTHON" tests/test_proxy_integration.py

echo "==> Generated CLI/capability references are not stale"
bash scripts/generate-cli-reference.sh
bash scripts/generate-capability-reference.sh
if ! git diff --exit-code -- docs-site/src/reference/cli.md docs-site/src/reference/capabilities.md; then
    echo "    ERROR: generated docs are stale. Commit the regenerated files:" >&2
    echo "      docs-site/src/reference/cli.md docs-site/src/reference/capabilities.md" >&2
    exit 1
fi

echo "==> mdbook build docs-site"
if command -v mdbook >/dev/null 2>&1; then
    mdbook build docs-site
else
    skip "mdbook build docs-site" "cargo install mdbook --locked --version ^0.5"
fi

echo "==> cargo audit --deny warnings"
if command -v cargo-audit >/dev/null 2>&1; then
    cargo audit --deny warnings
else
    skip "cargo audit --deny warnings" "cargo install cargo-audit --locked"
fi

if [ ${#SKIPPED[@]} -eq 0 ]; then
    echo "All checks passed."
    exit 0
fi

echo ""
echo "Checks passed, but ${#SKIPPED[@]} gate(s) did NOT run:" >&2
for gate in "${SKIPPED[@]}"; do
    echo "  - $gate" >&2
done
echo "CI runs these; a green local run here is weaker evidence than a green CI run." >&2
if [ "$STRICT" = "1" ]; then
    echo "--strict was set: treating skipped gates as failure." >&2
    exit 1
fi
