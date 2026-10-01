# Installation

This is a pre-1.0 source candidate. Its changes are not included in the existing
`v0.3.0-rc1` binary archives. Build this revision from source to use these fixes.
The older RC1 Linux x86_64 archive requires GLIBC_2.39 and cannot run on Ubuntu
22.04 / glibc 2.35. A corrected binary prerelease remains pending.
No package-manager install exists yet: npm, PyPI, crates.io and Homebrew remain
unpublished. The existing RC1 tag contains an install action; no new action tag
has been released for this source candidate.

## Install the older prebuilt preview

Pin the version explicitly; a mutable Latest badge does not identify source.
These commands install the older RC1 and do not provide this candidate's fixes.
On macOS and Linux the variable must sit to the **right** of
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

## Labs commands (opt-in build)

The default build has the core workflow. The derived, advisory `index`,
`route`, `optimize` and `graph` commands are compiled in only with
`--features labs`:

```bash
cargo install --path crates/mcptracer-proxy --features labs --locked
```

`--features semantic-search` adds `semantic` and includes `labs`.

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

Publicly accessible hosted test, MSRV, and real-SDK results are pending/unverified for source revision `63c82701aaedb5907885580c5f5a819116846176`. Windows-local checks do not establish Linux or macOS success.

Check the Actions tab for the exact commit you intend to use; that is the only
evidence that applies to it. Building requires Rust 1.85+ and a working C
toolchain because `rusqlite` compiles bundled SQLite.
