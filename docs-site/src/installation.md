# Installation

This is a pre-1.0 public source preview. No package, binary archive, Homebrew
tap, npm package, PyPI package, or released GitHub Action is claimed.

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

The workflow is configured for Linux, macOS, and Windows, but configuration is
not execution evidence. The compatibility results currently documented in this
snapshot were obtained on Windows; Linux and macOS remain pending successful
runs of the exact public commit. Building requires Rust 1.85+ and a working C
toolchain because `rusqlite` compiles bundled SQLite.
