#!/usr/bin/env bash
# Render dist/homebrew/mcptracer.rb.tmpl into a real Homebrew formula for a
# tagged release, using that release's SHA256SUMS.txt as the source of truth
# for checksums (never hand-typed). The output is NOT itself a working tap -
# see the template's header comment for how to publish it.
#
# Usage: scripts/generate-homebrew-formula.sh v0.2.0 > mcptracer.rb
set -euo pipefail
cd "$(dirname "$0")/.."

REPO="ard12/mcptracer"
TEMPLATE="dist/homebrew/mcptracer.rb.tmpl"

[ $# -eq 1 ] || {
    echo "usage: $0 <tag, e.g. v0.2.0>" >&2
    exit 1
}
TAG="$1"
case "$TAG" in
    v*) ;;
    *) TAG="v${TAG}" ;;
esac
VERSION="${TAG#v}"

[ -f "$TEMPLATE" ] || {
    echo "error: template not found: $TEMPLATE" >&2
    exit 1
}

SUMS_URL="https://github.com/${REPO}/releases/download/${TAG}/SHA256SUMS.txt"
SUMS="$(curl -fsSL "$SUMS_URL")" || {
    echo "error: could not fetch $SUMS_URL (does this release exist?)" >&2
    exit 1
}

checksum_for() {
    local target="$1"
    local line
    line="$(echo "$SUMS" | grep "mcptracer-${TAG}-${target}.tar.gz\$" || true)"
    [ -n "$line" ] || {
        echo "error: no checksum entry for target $target in $SUMS_URL" >&2
        exit 1
    }
    echo "$line" | awk '{print $1}'
}

SHA_LINUX_X64="$(checksum_for "x86_64-unknown-linux-gnu")"
SHA_MAC_X64="$(checksum_for "x86_64-apple-darwin")"
SHA_MAC_ARM64="$(checksum_for "aarch64-apple-darwin")"

sed \
    -e "s/__VERSION__/${VERSION}/g" \
    -e "s/__SHA256_X86_64_UNKNOWN_LINUX_GNU__/${SHA_LINUX_X64}/" \
    -e "s/__SHA256_X86_64_APPLE_DARWIN__/${SHA_MAC_X64}/" \
    -e "s/__SHA256_AARCH64_APPLE_DARWIN__/${SHA_MAC_ARM64}/" \
    "$TEMPLATE"
