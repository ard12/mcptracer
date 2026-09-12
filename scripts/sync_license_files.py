#!/usr/bin/env python3
"""Keep distributable license documents identical to their root originals."""
from __future__ import annotations

import argparse
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DOCUMENTS = ("LICENSE", "NOTICE", "COMMERCIAL-LICENSE.md")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail without writing if copies differ")
    args = parser.parse_args()
    directories = [p.parent for p in sorted((ROOT / "crates").glob("*/Cargo.toml"))]
    directories += [ROOT / "packages/npm/mcptracer", ROOT / "packages/python/mcptracer"]
    stale = []
    for name in DOCUMENTS:
        content = (ROOT / name).read_bytes()
        for directory in directories:
            target = directory / name
            if target.is_file() and target.read_bytes() == content:
                continue
            if args.check:
                stale.append(target.relative_to(ROOT).as_posix())
            else:
                target.write_bytes(content)
    if stale:
        print("Stale or missing license documents: " + ", ".join(stale))
        print("Run python scripts/sync_license_files.py from the authoritative source.")
        return 1
    print(f"License documents {'verified' if args.check else 'synchronized'}: {len(directories)} packages")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
