"""Compatibility failures must not become successful packaging checks."""

from __future__ import annotations

import importlib.util
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "check_glibc_compat", ROOT / "scripts" / "check_glibc_compat.py"
)
assert SPEC and SPEC.loader
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


class GlibcCompatibilityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.binary = Path(self.temporary.name) / "binary"
        self.binary.write_bytes(b"\x7fELF" + b"\x00" * 60)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def result(self, report: str, returncode: int = 0) -> subprocess.CompletedProcess:
        return subprocess.CompletedProcess(["readelf"], returncode, report, "")

    def test_supported_imports_pass(self) -> None:
        report = ("Version needs section '.gnu.version_r' contains 1 entry:\n"
                  "Name: GLIBC_2.2.5 Flags: none\nName: GLIBC_2.35 Flags: none\n")
        with mock.patch.object(CHECK.subprocess, "run", return_value=self.result(report)):
            self.assertEqual(CHECK.check_binary(self.binary), "2.35")

    def test_newer_import_fails(self) -> None:
        report = "Version needs section '.gnu.version_r':\nName: GLIBC_2.39 Flags: none\n"
        with mock.patch.object(CHECK.subprocess, "run", return_value=self.result(report)):
            with self.assertRaisesRegex(CHECK.CompatibilityError, "GLIBC_2.39"):
                CHECK.check_binary(self.binary)

    def test_failed_inspection_cannot_pass_with_empty_output(self) -> None:
        with mock.patch.object(CHECK.subprocess, "run", return_value=self.result("", 1)):
            with self.assertRaisesRegex(CHECK.CompatibilityError, "readelf rejected"):
                CHECK.check_binary(self.binary)

    def test_empty_successful_inspection_is_rejected(self) -> None:
        with mock.patch.object(CHECK.subprocess, "run", return_value=self.result("")):
            with self.assertRaisesRegex(CHECK.CompatibilityError, "no recognized"):
                CHECK.check_binary(self.binary)

    def test_inspection_diagnostics_cannot_be_ignored(self) -> None:
        result = self.result("No version information found in this file.")
        result.stderr = "readelf: Warning: truncated section"
        with mock.patch.object(CHECK.subprocess, "run", return_value=result):
            with self.assertRaisesRegex(CHECK.CompatibilityError, "readelf rejected"):
                CHECK.check_binary(self.binary)

    def test_genuinely_absent_versions_are_explicit(self) -> None:
        with mock.patch.object(CHECK.subprocess, "run", return_value=self.result(
                "No version information found in this file.\n")):
            self.assertIsNone(CHECK.check_binary(self.binary))

    def test_definitions_are_not_imported_requirements(self) -> None:
        report = "Version definition section '.gnu.version_d':\nName: GLIBC_9.99\n"
        with mock.patch.object(CHECK.subprocess, "run", return_value=self.result(report)):
            self.assertIsNone(CHECK.check_binary(self.binary))

    def test_empty_requirements_section_is_rejected(self) -> None:
        report = "Version needs section '.gnu.version_r':\n"
        with mock.patch.object(CHECK.subprocess, "run", return_value=self.result(report)):
            with self.assertRaisesRegex(CHECK.CompatibilityError, "empty requirements"):
                CHECK.check_binary(self.binary)

    def test_explicit_non_glibc_requirements_pass(self) -> None:
        report = "Version needs section '.gnu.version_r':\nName: GLIBCXX_3.4.30\n"
        with mock.patch.object(CHECK.subprocess, "run", return_value=self.result(report)):
            self.assertIsNone(CHECK.check_binary(self.binary))

    def test_non_numeric_glibc_requirement_fails(self) -> None:
        report = "Version needs section '.gnu.version_r':\nName: GLIBC_ABI_DT_RELR\n"
        with mock.patch.object(CHECK.subprocess, "run", return_value=self.result(report)):
            with self.assertRaisesRegex(CHECK.CompatibilityError, "unrecognized GLIBC"):
                CHECK.check_binary(self.binary)

    def test_missing_and_unreadable_input_fail_before_inspection(self) -> None:
        with self.assertRaisesRegex(CHECK.CompatibilityError, "cannot read"):
            CHECK.check_binary(self.binary.with_name("missing"))
        with mock.patch.object(Path, "open", side_effect=PermissionError):
            with self.assertRaisesRegex(CHECK.CompatibilityError, "cannot read"):
                CHECK.check_binary(self.binary)

    def test_invalid_input_fails_before_inspection(self) -> None:
        self.binary.write_bytes(b"not an ELF")
        with self.assertRaisesRegex(CHECK.CompatibilityError, "not an ELF"):
            CHECK.check_binary(self.binary)

    def test_inspector_unavailable_or_timed_out_fails(self) -> None:
        for failure in (FileNotFoundError(), subprocess.TimeoutExpired("readelf", 30)):
            with self.subTest(failure=type(failure).__name__):
                with mock.patch.object(CHECK.subprocess, "run", side_effect=failure):
                    with self.assertRaisesRegex(CHECK.CompatibilityError, "could not complete"):
                        CHECK.check_binary(self.binary)

    @unittest.skipUnless(shutil.which("readelf") and Path("/bin/true").is_file(), "requires Linux readelf")
    def test_real_system_elf_is_inspected(self) -> None:
        self.assertIsNotNone(CHECK.check_binary(Path("/bin/true"), "99.0"))


if __name__ == "__main__":
    unittest.main()
