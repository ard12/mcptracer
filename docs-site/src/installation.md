# Installation

This is a pre-1.0 preview. Prebuilt binaries are published as a **prerelease**
(`v0.3.0-rc1`) for Linux (x86_64, ARM64), macOS (Intel, Apple Silicon), and Windows
(x86_64). No package-manager install exists yet: npm, PyPI, crates.io, Homebrew,
and a released GitHub Action are all unpublished.

## Install the prebuilt preview

GitHub's latest-release endpoint deliberately excludes prereleases, so pin the
version explicitly. On macOS and Linux the variable must sit to the **right** of
the pipe, attached to `bash`; on the left it would be set for `curl` instead and
the installer would never see it.

```bash
curl -fsSL https://raw.githubusercontent.com/ard12/mcptracer/main/scripts/install.sh | MCPTRACER_VERSION=v0.3.0-rc1 bash
```

```powershell
$env:MCPTRACER_VERSION = "v0.3.0-rc1"; irm https://raw.githubusercontent.com/ard12/mcptracer/main/scripts/install.ps1 | iex
```

Both installers verify the archive's SHA-256 before extracting and confirm the
installed binary actually runs. To check build provenance yourself:

```bash
gh attestation verify mcptracer-v0.3.0-rc1-<target>.tar.gz --owner ard12
```

## Build from source

Build the checked-out revision from source:

```bash
cargo build --workspace --all-features --locked
cargo install --path crates/mcptracer-proxy --locked
mcptracer --version
```

To pin a remote source build, replace `<public-commit>` with a reviewed commit
from this repository:

```bash
cargo install --git https://github.com/ard12/mcptracer   --rev <public-commit> mcptracer-proxy --locked
```

## Verifying the source tree

Verify the generated manifest and complete file coverage before building:

```bash
python scripts/verify_oss_export.py .
```

Then run the source tests or the isolated first-use smoke:

```bash
cargo test --workspace --all-features --locked
python tests/test_proxy_integration.py
python scripts/source_preview_smoke.py .
```

The smoke runner installs this checkout into a temporary prefix and uses that
installed binary for version/help checks and the complete rug-pull tutorial.

## Platform evidence

A configured matrix is not proof that a commit passed, so this section records
runs rather than configuration. Tests, the Rust 1.85 MSRV check, and the
real-SDK compatibility matrix have all passed on Linux, macOS, and Windows for
this snapshot's source revision; the Unix file-permission tests are included in
that and were confirmed to execute rather than being filtered out. The
compatibility matrix's Python stdio cell remains non-blocking with a disclosed,
SDK-side flake — see `docs/spec/compatibility-matrix.md`.

Check the Actions tab for the exact commit you intend to use; that is the only
evidence that applies to it. Building requires Rust 1.85+ and a working C
toolchain because `rusqlite` compiles bundled SQLite.
