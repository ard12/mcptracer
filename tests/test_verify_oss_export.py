#!/usr/bin/env python3
"""Tests for the public export verifier."""

from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "verify_oss_export", ROOT / "scripts" / "verify_oss_export.py"
)
assert SPEC and SPEC.loader
VERIFY = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = VERIFY
SPEC.loader.exec_module(VERIFY)

SOURCE_SHA = "1" * 40
POLICY_SHA = "2" * 64


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class VerifyOssExportTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.candidate = Path(self.temporary.name) / "candidate"
        (self.candidate / "nested").mkdir(parents=True)
        (self.candidate / "README.md").write_text("hello\n", encoding="utf-8")
        (self.candidate / "nested" / "data.bin").write_bytes(b"\x00\x01")
        self.manifest = {
            "format_version": 1,
            "source_commit": SOURCE_SHA,
            "source_policy_sha256": POLICY_SHA,
            "license": "LicenseRef-MCPTracer-Noncommercial-Attribution-1.0",
            "reserved_paths_verified_absent": ["private/secret.txt"],
            "files": {
                "README.md": digest(self.candidate / "README.md"),
                "nested/data.bin": digest(self.candidate / "nested" / "data.bin"),
            },
            "unhashed_generated_files": [VERIFY.MANIFEST_NAME],
        }
        self.write_manifest()

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write_manifest(self) -> None:
        (self.candidate / VERIFY.MANIFEST_NAME).write_text(
            json.dumps(self.manifest, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )

    def codes(self, **kwargs: object) -> set[str]:
        result = VERIFY.verify_export(self.candidate, **kwargs)
        return {finding.code for finding in result.findings}

    def test_clean_candidate_passes_before_or_after_git_init(self) -> None:
        (self.candidate / ".git").mkdir()
        (self.candidate / ".git" / "config").write_text("ignored", encoding="utf-8")
        (self.candidate / "target").mkdir()
        (self.candidate / "target" / "build.log").write_text("ignored", encoding="utf-8")
        (self.candidate / "docs-site" / "book").mkdir(parents=True)
        (self.candidate / "docs-site" / "book" / "index.html").write_text(
            "ignored", encoding="utf-8"
        )
        (self.candidate / "nested" / "__pycache__").mkdir()
        (self.candidate / "nested" / "__pycache__" / "x.pyc").write_bytes(b"ignored")
        result = VERIFY.verify_export(
            self.candidate,
            expected_source_sha=SOURCE_SHA,
            expected_policy_sha=POLICY_SHA,
        )
        self.assertTrue(result.ok, result.findings)

    def test_changed_bytes_fail(self) -> None:
        (self.candidate / "README.md").write_text("changed\n", encoding="utf-8")
        self.assertIn("hash-mismatch", self.codes())

    def test_missing_file_fails(self) -> None:
        (self.candidate / "nested" / "data.bin").unlink()
        self.assertIn("missing-file", self.codes())

    def test_extra_source_file_fails(self) -> None:
        (self.candidate / "surprise.txt").write_text("unexpected", encoding="utf-8")
        self.assertIn("extra-file", self.codes())

    def test_wrong_expected_source_sha_fails(self) -> None:
        self.assertIn(
            "source-sha-mismatch",
            self.codes(expected_source_sha="3" * 40),
        )

    def test_reserved_path_fails_even_when_nested(self) -> None:
        private = self.candidate / "private"
        private.mkdir()
        (private / "secret.txt").write_text("must not ship", encoding="utf-8")
        self.assertIn("reserved-path-present", self.codes())

    def test_traversing_manifest_path_fails_without_reading_outside(self) -> None:
        outside = self.candidate.parent / "outside.txt"
        outside.write_text("outside", encoding="utf-8")
        self.manifest["files"]["../outside.txt"] = digest(outside)
        self.write_manifest()
        self.assertIn("unsafe-manifest-path", self.codes())

    def test_noncanonical_manifest_segments_fail(self) -> None:
        self.manifest["files"]["nested//data.bin"] = "0" * 64
        self.manifest["reserved_paths_verified_absent"].append("private/../escape")
        self.write_manifest()
        self.assertIn("unsafe-manifest-path", self.codes())

    def test_absolute_and_windows_manifest_paths_fail(self) -> None:
        self.manifest["files"]["C:\\outside.txt"] = "0" * 64
        self.manifest["reserved_paths_verified_absent"].append("/absolute")
        self.write_manifest()
        self.assertIn("unsafe-manifest-path", self.codes())

    def test_symlink_is_rejected(self) -> None:
        outside = self.candidate.parent / "outside.txt"
        outside.write_text("outside", encoding="utf-8")
        link = self.candidate / "link.txt"
        try:
            os.symlink(outside, link)
        except (OSError, NotImplementedError) as error:
            self.skipTest(f"symlinks unavailable: {error}")
        self.manifest["files"]["link.txt"] = digest(outside)
        self.write_manifest()
        codes = self.codes()
        self.assertTrue(
            {"unsafe-manifest-path", "unsafe-symlink"} & codes,
            codes,
        )

    def test_optional_policy_file_is_compared_by_hash(self) -> None:
        policy = self.candidate.parent / "policy.json"
        policy.write_text("{}\n", encoding="utf-8")
        self.assertIn(
            "policy-file-mismatch",
            self.codes(policy_path=policy),
        )


if __name__ == "__main__":
    unittest.main()
