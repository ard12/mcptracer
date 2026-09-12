#!/usr/bin/env python3
"""Read-only verification for an MCPTracer public-source export.

Manifest paths are untrusted. The verifier rejects unsafe paths and symlinks,
checks every declared hash, and detects missing or unexpected source files.
A pass establishes integrity against the manifest, not authorship or trust.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
from dataclasses import asdict, dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Iterable

MANIFEST_NAME = "OSS_EXPORT_MANIFEST.json"
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")


@dataclass(frozen=True)
class Finding:
    code: str
    message: str


@dataclass(frozen=True)
class VerificationResult:
    candidate: str
    source_commit: str | None
    policy_sha256: str | None
    hashed_files: int
    reserved_paths: int
    findings: tuple[Finding, ...]

    @property
    def ok(self) -> bool:
        return not self.findings

    def as_dict(self) -> dict[str, Any]:
        return {
            "format_version": 1,
            "candidate": self.candidate,
            "source_commit": self.source_commit,
            "source_policy_sha256": self.policy_sha256,
            "hashed_files": self.hashed_files,
            "reserved_paths": self.reserved_paths,
            "status": "pass" if self.ok else "fail",
            "findings": [asdict(finding) for finding in self.findings],
            "claim": "byte integrity against the export manifest; not authorship",
        }


def _reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def _load_json(path: Path) -> Any:
    return json.loads(
        path.read_text(encoding="utf-8"),
        object_pairs_hook=_reject_duplicate_keys,
    )


def _safe_relative(value: Any) -> PurePosixPath:
    if not isinstance(value, str) or not value or "\x00" in value:
        raise ValueError("path must be a non-empty string without NUL bytes")
    if "\\" in value:
        raise ValueError("path must use forward slashes")
    raw_parts = value.split("/")
    if any(part in {"", ".", ".."} for part in raw_parts):
        raise ValueError("empty, current-directory, and parent segments are not allowed")
    path = PurePosixPath(value)
    if path.is_absolute() or value.startswith("/"):
        raise ValueError("absolute paths are not allowed")
    if not path.parts:
        raise ValueError("path must contain at least one segment")
    if ":" in path.parts[0]:
        raise ValueError("drive-qualified paths are not allowed")
    return path


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _ignored_output(path: PurePosixPath, *, is_dir: bool) -> bool:
    parts = path.parts
    if parts and parts[0] == ".git":
        return True
    if is_dir and parts == ("target",):
        return True
    if is_dir and parts and parts[-1] in {"node_modules", "__pycache__"}:
        return True
    if is_dir and parts == ("docs-site", "book"):
        return True
    return not is_dir and path.suffix == ".pyc"


def _inventory(root: Path, findings: list[Finding]) -> set[str]:
    files: set[str] = set()

    def visit(directory: Path, prefix: tuple[str, ...]) -> None:
        try:
            entries = sorted(os.scandir(directory), key=lambda entry: entry.name)
        except OSError as error:
            findings.append(Finding("filesystem-read", f"cannot scan {directory}: {error}"))
            return
        for entry in entries:
            relative = PurePosixPath(*prefix, entry.name)
            is_dir = entry.is_dir(follow_symlinks=False)
            if _ignored_output(relative, is_dir=is_dir):
                continue
            if entry.is_symlink():
                findings.append(Finding("unsafe-symlink", f"symlink is not allowed: {relative}"))
            elif is_dir:
                visit(Path(entry.path), (*prefix, entry.name))
            elif entry.is_file(follow_symlinks=False):
                files.add(relative.as_posix())
            else:
                findings.append(
                    Finding("unsupported-entry", f"unsupported filesystem entry: {relative}")
                )

    visit(root, ())
    return files


def _valid_hash(value: Any, label: str, findings: list[Finding]) -> str | None:
    if not isinstance(value, str) or not SHA256_RE.fullmatch(value):
        findings.append(
            Finding("manifest-format", f"{label} must be a lowercase SHA-256 value")
        )
        return None
    return value


def _path_list(
    value: Any, label: str, findings: list[Finding]
) -> list[tuple[str, PurePosixPath]]:
    if not isinstance(value, list):
        findings.append(Finding("manifest-format", f"{label} must be an array"))
        return []
    validated: list[tuple[str, PurePosixPath]] = []
    seen: set[str] = set()
    for index, item in enumerate(value):
        try:
            path = _safe_relative(item)
        except ValueError as error:
            findings.append(
                Finding("unsafe-manifest-path", f"{label}[{index}]: {error}")
            )
            continue
        normalized = path.as_posix()
        if normalized in seen:
            findings.append(
                Finding("manifest-format", f"duplicate path in {label}: {normalized}")
            )
            continue
        seen.add(normalized)
        validated.append((normalized, path))
    return validated


def _exists_without_following(root: Path, relative: PurePosixPath) -> bool:
    current = root
    for part in relative.parts:
        current /= part
        if not os.path.lexists(current):
            return False
        if current.is_symlink():
            return True
    return True


def _regular_without_symlink(
    root: Path, relative: PurePosixPath
) -> tuple[bool, str | None]:
    current = root
    for part in relative.parts:
        current /= part
        try:
            if current.is_symlink():
                return False, f"path traverses symlink: {relative}"
            current.lstat()
        except FileNotFoundError:
            return False, None
        except OSError as error:
            return False, f"cannot inspect {relative}: {error}"
    return current.is_file(), None


def verify_export(
    candidate: Path,
    *,
    expected_source_sha: str | None = None,
    expected_policy_sha: str | None = None,
    policy_path: Path | None = None,
) -> VerificationResult:
    candidate = candidate.resolve()
    findings: list[Finding] = []
    source_commit: str | None = None
    policy_sha: str | None = None
    hashed_count = 0
    reserved_count = 0

    def result() -> VerificationResult:
        return VerificationResult(
            str(candidate),
            source_commit,
            policy_sha,
            hashed_count,
            reserved_count,
            tuple(findings),
        )

    if not candidate.is_dir():
        findings.append(
            Finding("candidate", f"candidate directory does not exist: {candidate}")
        )
        return result()

    manifest_path = candidate / MANIFEST_NAME
    if manifest_path.is_symlink() or not manifest_path.is_file():
        findings.append(
            Finding("manifest", f"missing regular manifest: {MANIFEST_NAME}")
        )
        return result()

    try:
        manifest = _load_json(manifest_path)
    except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as error:
        findings.append(Finding("manifest-format", f"cannot parse manifest: {error}"))
        return result()

    if not isinstance(manifest, dict):
        findings.append(Finding("manifest-format", "manifest root must be an object"))
        return result()
    if manifest.get("format_version") != 1:
        findings.append(Finding("manifest-format", "format_version must be 1"))

    source_value = manifest.get("source_commit")
    if isinstance(source_value, str) and COMMIT_RE.fullmatch(source_value):
        source_commit = source_value
    else:
        findings.append(
            Finding(
                "manifest-format",
                "source_commit must be a lowercase 40-character Git SHA",
            )
        )
    policy_sha = _valid_hash(
        manifest.get("source_policy_sha256"), "source_policy_sha256", findings
    )
    if not isinstance(manifest.get("license"), str) or not manifest["license"].strip():
        findings.append(
            Finding("manifest-format", "license must be a non-empty string")
        )

    if expected_source_sha is not None:
        if not COMMIT_RE.fullmatch(expected_source_sha):
            findings.append(
                Finding(
                    "expected-value",
                    "expected source SHA must be 40 lowercase hex characters",
                )
            )
        elif source_commit != expected_source_sha:
            findings.append(
                Finding(
                    "source-sha-mismatch",
                    f"expected source {expected_source_sha}, found {source_commit}",
                )
            )
    if expected_policy_sha is not None:
        if not SHA256_RE.fullmatch(expected_policy_sha):
            findings.append(
                Finding(
                    "expected-value",
                    "expected policy SHA must be 64 lowercase hex characters",
                )
            )
        elif policy_sha != expected_policy_sha:
            findings.append(
                Finding(
                    "policy-sha-mismatch",
                    f"expected policy {expected_policy_sha}, found {policy_sha}",
                )
            )

    reserved = _path_list(
        manifest.get("reserved_paths_verified_absent"),
        "reserved_paths_verified_absent",
        findings,
    )
    reserved_count = len(reserved)
    for normalized, path in reserved:
        if _exists_without_following(candidate, path):
            findings.append(
                Finding("reserved-path-present", f"reserved path is present: {normalized}")
            )

    unhashed = _path_list(
        manifest.get("unhashed_generated_files"),
        "unhashed_generated_files",
        findings,
    )
    unhashed_names = {normalized for normalized, _ in unhashed}
    if unhashed_names != {MANIFEST_NAME}:
        findings.append(
            Finding(
                "manifest-format",
                f"unhashed_generated_files must contain only {MANIFEST_NAME}",
            )
        )

    files_value = manifest.get("files")
    validated_files: dict[str, tuple[PurePosixPath, str]] = {}
    if not isinstance(files_value, dict) or not files_value:
        findings.append(
            Finding("manifest-format", "files must be a non-empty object")
        )
    else:
        for raw_path, raw_hash in files_value.items():
            try:
                path = _safe_relative(raw_path)
            except ValueError as error:
                findings.append(
                    Finding(
                        "unsafe-manifest-path",
                        f"files key {raw_path!r}: {error}",
                    )
                )
                continue
            normalized = path.as_posix()
            expected_hash = _valid_hash(
                raw_hash, f"files[{normalized!r}]", findings
            )
            if expected_hash is not None:
                validated_files[normalized] = (path, expected_hash)
    hashed_count = len(validated_files)
    if MANIFEST_NAME in validated_files:
        findings.append(
            Finding("manifest-format", f"{MANIFEST_NAME} cannot hash itself")
        )

    for normalized, (path, expected_hash) in validated_files.items():
        regular, error = _regular_without_symlink(candidate, path)
        if error:
            findings.append(Finding("unsafe-manifest-path", error))
            continue
        if not regular:
            findings.append(
                Finding(
                    "missing-file",
                    f"manifest file is missing or not regular: {normalized}",
                )
            )
            continue
        try:
            actual_hash = _sha256(candidate / Path(*path.parts))
        except OSError as read_error:
            findings.append(
                Finding("filesystem-read", f"cannot hash {normalized}: {read_error}")
            )
            continue
        if actual_hash != expected_hash:
            findings.append(
                Finding("hash-mismatch", f"SHA-256 mismatch: {normalized}")
            )

    actual_files = _inventory(candidate, findings)
    expected_files = set(validated_files) | unhashed_names
    for missing in sorted(expected_files - actual_files):
        if not any(
            finding.code == "missing-file" and missing in finding.message
            for finding in findings
        ):
            findings.append(
                Finding("missing-file", f"expected file is missing: {missing}")
            )
    for extra in sorted(actual_files - expected_files):
        findings.append(
            Finding("extra-file", f"unexpected source file: {extra}")
        )

    if policy_path is not None:
        try:
            actual_policy_hash = _sha256(policy_path.resolve(strict=True))
        except (OSError, RuntimeError) as error:
            findings.append(
                Finding("policy-read", f"cannot read policy file: {error}")
            )
        else:
            if actual_policy_hash != policy_sha:
                findings.append(
                    Finding(
                        "policy-file-mismatch",
                        "private policy file hash does not match manifest",
                    )
                )
    return result()


def _write_report(path: Path, result: VerificationResult) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(result.as_dict(), indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def main(argv: Iterable[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "candidate", type=Path, help="generated export directory or fresh checkout"
    )
    parser.add_argument("--expected-source-sha")
    parser.add_argument("--expected-policy-sha")
    parser.add_argument(
        "--policy", type=Path, help="optional private policy file to compare by hash"
    )
    parser.add_argument(
        "--report-json", type=Path, help="optional result file outside the candidate"
    )
    args = parser.parse_args(argv)
    result = verify_export(
        args.candidate,
        expected_source_sha=args.expected_source_sha,
        expected_policy_sha=args.expected_policy_sha,
        policy_path=args.policy,
    )
    if args.report_json:
        _write_report(args.report_json, result)
    if result.ok:
        print(
            f"PASS export={result.candidate} files={result.hashed_files} "
            f"reserved={result.reserved_paths} source={result.source_commit}"
        )
        print(
            "Integrity matches the manifest; authorship and trust are not established by hashes."
        )
        return 0
    print(
        f"FAIL export={result.candidate} findings={len(result.findings)}",
        file=sys.stderr,
    )
    for finding in result.findings:
        print(f"- [{finding.code}] {finding.message}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
