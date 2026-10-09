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
                aarch64|arm64) echo "aarch64-unknown-linux-gnu" ;;
                *) die "unsupported Linux architecture: $arch (release binaries are published for x86_64 and aarch64)" ;;
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

NO_STABLE_RELEASE_MSG="no stable version tag is available for automatic installation. This installer requires an explicit version for preview tags -- install it explicitly: MCPTRACER_VERSION=v0.3.0-rc3 sh install.sh (or: curl -fsSL <install.sh-url> | MCPTRACER_VERSION=v0.3.0-rc3 sh)"

VERSION="${MCPTRACER_VERSION:-}"
if [ -z "$VERSION" ]; then
    log "resolving latest release..."
    # /releases/latest follows GitHub's mutable prerelease flag. We do not
    # fall back to "newest release including prereleases" when it comes up
    # empty, and the tag check below also rejects preview-style tags if that
    # flag was cleared. Preview installation must be explicit via
    # MCPTRACER_VERSION.
    set +e
    LATEST_JSON="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null)"
    CURL_STATUS=$?
    set -e
    if [ "$CURL_STATUS" -eq 0 ]; then
        VERSION="$(printf '%s' "$LATEST_JSON" | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
        # GitHub's /latest endpoint is controlled by the mutable prerelease
        # flag. Do not let a preview tag such as v0.3.0-rc1 become an implicit
        # install if that flag is accidentally cleared.
        case "$VERSION" in *-*) VERSION="" ;; esac
    fi
    [ -n "$VERSION" ] || die "$NO_STABLE_RELEASE_MSG"
fi
case "$VERSION" in
    v*) TAG="$VERSION" ;;
    *) TAG="v${VERSION}" ;;
esac
EXPECTED_VERSION="${VERSION#v}"

ASSET="${BIN_NAME}-${TAG}-${TARGET}.tar.gz"
BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"

WORKDIR="$(mktemp -d)"
TEMP_BINARY=""
cleanup_install() {
    if [ -n "${TEMP_BINARY:-}" ]; then rm -f "$TEMP_BINARY"; fi
    if [ -n "${WORKDIR:-}" ]; then rm -rf "$WORKDIR"; fi
}
trap cleanup_install EXIT

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
TEMP_BINARY="$(mktemp "$INSTALL_DIR/.${BIN_NAME}.XXXXXX")" \
    || die "cannot create a staging binary in ${INSTALL_DIR}"
install -m 755 "$STAGING/$BIN_NAME" "$TEMP_BINARY"

VERSION_OUTPUT="$("$TEMP_BINARY" --version 2>&1)" \
    || die "downloaded binary does not run: ${TEMP_BINARY} (likely causes: wrong CPU architecture, a missing shared library, or a corrupted extraction)"
if [ "$VERSION_OUTPUT" != "$BIN_NAME $EXPECTED_VERSION" ]; then
    die "downloaded binary version mismatch: expected ${BIN_NAME} ${EXPECTED_VERSION}, got ${VERSION_OUTPUT}"
fi
printf '%s\n' "$VERSION_OUTPUT" >&2
# The temporary file is in INSTALL_DIR, so rename is an atomic same-volume
# replacement. The previous working binary is untouched until all checks pass.
mv -f "$TEMP_BINARY" "$INSTALL_DIR/$BIN_NAME" \
    || die "could not replace ${INSTALL_DIR}/${BIN_NAME}"
TEMP_BINARY=""

log "installed ${TAG} to ${INSTALL_DIR}/${BIN_NAME}"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) log "note: ${INSTALL_DIR} is not on PATH. Add it, e.g.: export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac
