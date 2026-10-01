#!/usr/bin/env python3
"""Fail closed when an ELF cannot be inspected or exceeds the glibc ceiling."""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path


class CompatibilityError(RuntimeError):
    """The binary is incompatible or its compatibility cannot be established."""


def check_binary(binary: Path, maximum: str = "2.35") -> str | None:
    if not re.fullmatch(r"[0-9]+(?:\.[0-9]+)+", maximum):
        raise CompatibilityError("invalid maximum glibc version")
    try:
        with binary.open("rb") as stream:
            if stream.read(4) != b"\x7fELF":
                raise CompatibilityError("input is not an ELF binary")
    except OSError as error:
        raise CompatibilityError("cannot read ELF binary") from error
    environment = os.environ.copy()
    environment.update(LC_ALL="C", LANG="C")
    try:
        inspection = subprocess.run(
            ["readelf", "--version-info", "--wide", str(binary)],
            capture_output=True, text=True, env=environment,
            timeout=30, check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise CompatibilityError("ELF inspection could not complete") from error
    if inspection.returncode != 0 or inspection.stderr.strip():
        raise CompatibilityError("readelf rejected the ELF binary")
    report = inspection.stdout
    headings = ("Version symbols section ", "Version definition section ",
                "Version needs section ")
    if not any(heading in report for heading in headings):
        if report.strip() == "No version information found in this file.":
            return None
        raise CompatibilityError("ELF inspection returned no recognized version report")
    # Imported requirements, not versions defined by the binary itself.
    if "Version needs section " not in report:
        return None
    requirements = report.split("Version needs section ", 1)[1]
    if not re.search(r"\bName:\s+\S+", requirements):
        raise CompatibilityError("ELF inspection returned an empty requirements section")
    names = re.findall(r"\bName:\s+(GLIBC_[^\s]+)", requirements)
    versions: list[tuple[int, ...]] = []
    for name in names:
        if not re.fullmatch(r"GLIBC_[0-9]+(?:\.[0-9]+)+", name):
            raise CompatibilityError("unrecognized GLIBC version requirement")
        versions.append(tuple(map(int, name.removeprefix("GLIBC_").split("."))))
    if not versions:
        return None
    required = max(versions)
    if required > tuple(map(int, maximum.split("."))):
        version = ".".join(map(str, required))
        raise CompatibilityError(
            f"ELF requires GLIBC_{version}; maximum supported is GLIBC_{maximum}"
        )
    return ".".join(map(str, required))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--max-glibc", default="2.35")
    args = parser.parse_args()
    try:
        required = check_binary(args.binary, args.max_glibc)
    except CompatibilityError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1
    if required is None:
        print("ELF inspection succeeded; no imported GLIBC version requirements.")
    else:
        print(f"ELF inspection succeeded; maximum requirement GLIBC_{required}.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
