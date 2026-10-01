"""Failure-path tests for the Windows installer using mocked network/extraction."""

from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

POWERSHELL = shutil.which("pwsh") or shutil.which("powershell")
INSTALLER = Path(__file__).resolve().parents[1] / "scripts" / "install.ps1"


@unittest.skipUnless(POWERSHELL, "PowerShell is required to exercise install.ps1")
class WindowsInstallFailureTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.install_dir = self.root / "install"
        self.install_dir.mkdir()
        self.existing_binary = self.install_dir / "mcptracer.exe"
        self.existing_binary.write_bytes(b"known-good-install")
        self.binary_source = self.root / "downloaded-mcptracer.exe"
        self.binary_source.write_bytes(b"not a runnable Windows executable")
        self.asset = self.root / "release.zip"
        self.asset.write_bytes(b"fixture archive bytes")
        self.harness = self.root / "invoke-installer.ps1"
        self.request_log = self.root / "requests.log"

    def run_installer(
        self,
        *,
        checksum: str | None = None,
        fail_download: bool = False,
        auto_latest: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        expected = checksum or hashlib.sha256(self.asset.read_bytes()).hexdigest()
        installer = str(INSTALLER).replace("'", "''")
        harness = f"""
$ErrorActionPreference = 'Stop'
function Invoke-RestMethod {{
    param([string]$Uri)
    [pscustomobject]@{{ tag_name = 'v0.3.0-rc1' }}
}}
function Invoke-WebRequest {{
    param([string]$Uri, [string]$OutFile)
    [System.IO.File]::AppendAllText($env:FAKE_REQUEST_LOG, "$Uri`n")
    if ($env:FAKE_FAIL_DOWNLOAD -eq '1' -and -not $Uri.EndsWith('.sha256')) {{
        throw 'simulated download failure'
    }}
    if ($Uri.EndsWith('.sha256')) {{
        [System.IO.File]::WriteAllText($OutFile, "$env:FAKE_CHECKSUM  mcptracer.zip")
    }} else {{
        Copy-Item -LiteralPath $env:FAKE_ASSET -Destination $OutFile
    }}
}}
function Expand-Archive {{
    param([string]$Path, [string]$DestinationPath, [switch]$Force)
    $directory = Join-Path $DestinationPath 'mcptracer-v0.3.0-rc1-x86_64-pc-windows-msvc'
    New-Item -ItemType Directory -Path $directory | Out-Null
    Copy-Item -LiteralPath $env:FAKE_BINARY_SOURCE -Destination (Join-Path $directory 'mcptracer.exe')
}}
if ($env:FAKE_AUTO_LATEST -eq '1') {{
    Remove-Item Env:MCPTRACER_VERSION -ErrorAction SilentlyContinue
}} else {{
    $env:MCPTRACER_VERSION = 'v0.3.0-rc1'
}}
$env:MCPTRACER_INSTALL_DIR = $env:FAKE_INSTALL_DIR
try {{
    & '{installer}'
}} catch {{
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 9
}}
exit 0
"""
        self.harness.write_text(harness, encoding="utf-8")
        env = {
            **os.environ,
            "FAKE_ASSET": str(self.asset),
            "FAKE_BINARY_SOURCE": str(self.binary_source),
            "FAKE_CHECKSUM": expected,
            "FAKE_FAIL_DOWNLOAD": "1" if fail_download else "0",
            "FAKE_INSTALL_DIR": str(self.install_dir),
            "FAKE_REQUEST_LOG": str(self.request_log),
            "FAKE_AUTO_LATEST": "1" if auto_latest else "0",
        }
        return subprocess.run(
            [POWERSHELL, "-NoProfile", "-File", str(self.harness)],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )

    def assert_install_preserved(self) -> None:
        self.assertEqual(self.existing_binary.read_bytes(), b"known-good-install")
        self.assertEqual(list(self.install_dir.glob(".mcptracer.*.tmp.exe")), [])

    def test_download_failure_preserves_existing_install(self) -> None:
        result = self.run_installer(fail_download=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("simulated download failure", result.stderr)
        self.assert_install_preserved()

    def test_checksum_mismatch_preserves_existing_install(self) -> None:
        result = self.run_installer(checksum="0" * 64)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assert_install_preserved()

    def test_unexecutable_binary_preserves_existing_install(self) -> None:
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("downloaded binary does not run", result.stderr)
        self.assert_install_preserved()

    def test_latest_preview_requires_explicit_version_pin(self) -> None:
        result = self.run_installer(auto_latest=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires an explicit version for preview tags", result.stderr)
        self.assertFalse(self.request_log.exists(), "preview resolution must stop before downloading assets")
        self.assert_install_preserved()


if __name__ == "__main__":
    unittest.main()
