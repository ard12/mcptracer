#!/usr/bin/env bash
# Install the mcptracer CLI from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/ard12/mcptracer/main/scripts/install.sh | sh
#
# Env vars:
#   MCPTRACER_VERSION      release tag to install, e.g. "v0.2.0" or "0.2.0"
#                           (default: latest release)
#   MCPTRACER_INSTALL_DIR  directory to install the binary into
#                           (default: "$HOME/.local/bin")
# POSIX-only options: this script is documented as `curl ... | bash`, but
# piping into `sh` ignores the shebang, and dash (Debian/Ubuntu /bin/sh)
# has no `set -o pipefail`. Every pipeline below has its result checked
# explicitly, so `set -eu` is sufficient and works under either shell.
set -eu

REPO="ard12/mcptracer"
BIN_NAME="mcptracer"
INSTALL_DIR="${MCPTRACER_INSTALL_DIR:-$HOME/.local/bin}"

log() { printf '[mcptracer-install] %s\n' "$1" >&2; }
die() {
    printf '[mcptracer-install] error: %s\n' "$1" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}
need curl
need tar

detect_target() {
    # No `local`: not in POSIX, and absent from some /bin/sh implementations.
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Linux)
            case "$arch" in
                x86_64) echo "x86_64-unknown-linux-gnu" ;;
                *) die "unsupported Linux architecture: $arch (only x86_64 release binaries are published)" ;;
            esac
            ;;
        Darwin)
            case "$arch" in
                x86_64) echo "x86_64-apple-darwin" ;;
                arm64) echo "aarch64-apple-darwin" ;;
                *) die "unsupported macOS architecture: $arch" ;;
            esac
            ;;
        *)
            die "unsupported OS: $os (Windows: use scripts/install.ps1 instead)"
            ;;
    esac
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "no sha256sum or shasum available to verify the download"
    fi
}

TARGET="$(detect_target)"

VERSION="${MCPTRACER_VERSION:-}"
if [ -z "$VERSION" ]; then
    log "resolving latest release..."
    VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
    [ -n "$VERSION" ] || die "could not resolve the latest release tag"
fi
case "$VERSION" in
    v*) TAG="$VERSION" ;;
    *) TAG="v${VERSION}" ;;
esac

ASSET="${BIN_NAME}-${TAG}-${TARGET}.tar.gz"
BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

log "downloading ${ASSET} (${TAG})..."
curl -fsSL -o "$WORKDIR/$ASSET" "${BASE_URL}/${ASSET}" \
    || die "download failed: ${BASE_URL}/${ASSET} (does this release/target combination exist?)"
curl -fsSL -o "$WORKDIR/$ASSET.sha256" "${BASE_URL}/${ASSET}.sha256" \
    || die "checksum download failed: ${BASE_URL}/${ASSET}.sha256"

log "verifying checksum..."
expected="$(awk '{print $1}' "$WORKDIR/$ASSET.sha256")"
actual="$(sha256_of "$WORKDIR/$ASSET")"
[ "$expected" = "$actual" ] || die "checksum mismatch for ${ASSET}: expected ${expected}, got ${actual}"

log "extracting..."
tar xzf "$WORKDIR/$ASSET" -C "$WORKDIR"
STAGING="$WORKDIR/${BIN_NAME}-${TAG}-${TARGET}"
[ -x "$STAGING/$BIN_NAME" ] || die "extracted archive did not contain $BIN_NAME"

mkdir -p "$INSTALL_DIR"
install -m 755 "$STAGING/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"

log "installed ${TAG} to ${INSTALL_DIR}/${BIN_NAME}"
"$INSTALL_DIR/$BIN_NAME" --version >&2 || true

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) log "note: ${INSTALL_DIR} is not on PATH. Add it, e.g.: export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac
