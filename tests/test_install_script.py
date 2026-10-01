"""POSIX installer target selection and failure-safe replacement tests."""

import base64
import hashlib
import io
import os
import shutil
import stat
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

# Resolved once: a bare "bash" passed to subprocess can hit the WSL launcher in
# System32 on Windows even when Git's bash is first on PATH.
BASH = shutil.which("bash") if os.name != "nt" else None
SCRIPT = Path(__file__).resolve().parent.parent / "scripts" / "install.sh"


FAKE_UNAME = """#!/bin/sh
case "$1" in
  -s) echo "$FAKE_UNAME_S" ;;
  -m) echo "$FAKE_UNAME_M" ;;
  *) echo "$FAKE_UNAME_S" ;;
esac
"""

FAKE_CURL = """#!/bin/sh
for arg in "$@"; do last="$arg"; done
echo "$last" >> "$FAKE_CURL_LOG"
exit 22
"""


@unittest.skipUnless(BASH, "bash is required to run install.sh")
class InstallScriptTargetTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        for name, body in (("uname", FAKE_UNAME), ("curl", FAKE_CURL)):
            path = self.bin / name
            path.write_text(body, encoding="utf-8", newline="\n")
            path.chmod(path.stat().st_mode | stat.S_IEXEC)
        self.curl_log = self.tmp / "curl.log"
        self.work = self.tmp / "work"
        self.work.mkdir()

    def run_script(self, uname_s: str, uname_m: str) -> subprocess.CompletedProcess:
        env = {
            **os.environ,
            "PATH": self.path_for_bash(self.bin) + ":/usr/bin:/bin",
            "FAKE_UNAME_S": uname_s,
            "FAKE_UNAME_M": uname_m,
            "FAKE_CURL_LOG": self.path_for_bash(self.curl_log),
            "MCPTRACER_VERSION": "v0.3.0-rc1",
            "MCPTRACER_INSTALL_DIR": self.path_for_bash(self.tmp / "install"),
        }
        return subprocess.run(
            [BASH, str(SCRIPT)],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
        )

    def path_for_bash(self, path: Path) -> str:
        if os.name != "nt":
            return str(path)
        candidates = [
            Path(BASH).parent.parent / "usr" / "bin" / "cygpath.exe",
            Path(BASH).parent / "cygpath.exe",
        ]
        cygpath = next((candidate for candidate in candidates if candidate.is_file()), None)
        if cygpath is None:
            self.skipTest("Git for Windows cygpath.exe is unavailable")
        result = subprocess.run(
            [str(cygpath), "-u", str(path)],
            check=True,
            capture_output=True,
            text=True,
        )
        return result.stdout.strip()

    def make_fake_release(
        self, binary: bytes, *, checksum: str | None = None
    ) -> tuple[str, dict[str, str]]:
        target = "x86_64-unknown-linux-gnu"
        asset = f"mcptracer-v0.3.0-rc1-{target}.tar.gz"
        payload = io.BytesIO()
        with tarfile.open(fileobj=payload, mode="w:gz") as archive:
            entry = tarfile.TarInfo(f"mcptracer-v0.3.0-rc1-{target}/mcptracer")
            entry.mode = 0o755
            entry.size = len(binary)
            archive.addfile(entry, io.BytesIO(binary))
        archive_bytes = payload.getvalue()
        expected = checksum or hashlib.sha256(archive_bytes).hexdigest()
        checksum_line = f"{expected}  {asset}\n"

        (self.bin / "curl").write_text(
            """#!/bin/sh
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) output="$2"; shift 2 ;;
    *) url="$1"; shift ;;
  esac
done
if [ "${FAKE_CURL_FAIL:-}" = "1" ] && [ "${url##*.}" != "sha256" ]; then
  exit 22
fi
if [ "${url##*.}" = "sha256" ]; then
  printf '%s' "$FAKE_CHECKSUM" > "$output"
else
  printf '%s' "$FAKE_ARCHIVE_B64" | base64 -d > "$output"
fi
""",
            encoding="utf-8",
            newline="\n",
        )
        (self.bin / "curl").chmod((self.bin / "curl").stat().st_mode | stat.S_IEXEC)
        env = {
            **os.environ,
            "PATH": self.path_for_bash(self.bin) + ":/usr/bin:/bin",
            "FAKE_UNAME_S": "Linux",
            "FAKE_UNAME_M": "x86_64",
            "FAKE_CURL_LOG": self.path_for_bash(self.curl_log),
            "TMPDIR": self.path_for_bash(self.work),
            "FAKE_CURL_FAIL": "0",
            "FAKE_CHECKSUM": checksum_line,
            "FAKE_ARCHIVE_B64": base64.b64encode(archive_bytes).decode("ascii"),
            "MCPTRACER_VERSION": "v0.3.0-rc1",
            "MCPTRACER_INSTALL_DIR": self.path_for_bash(self.tmp / "install"),
        }
        return asset, env

    def run_fake_install(
        self,
        binary: bytes,
        *,
        checksum: str | None = None,
        fail_download: bool = False,
    ) -> tuple[subprocess.CompletedProcess[str], str]:
        asset, env = self.make_fake_release(binary, checksum=checksum)
        env["FAKE_CURL_FAIL"] = "1" if fail_download else "0"
        result = subprocess.run(
            [BASH, str(SCRIPT)],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
        )
        self.assertEqual(
            list(self.work.iterdir()),
            [],
            "installer must remove its extraction directory on every exit path",
        )
        install_dir = self.tmp / "install"
        if install_dir.exists():
            self.assertEqual(
                list(install_dir.glob(".mcptracer.*")),
                [],
                "installer must remove its staged binary on every exit path",
            )
        return result, asset

    def requested_url(self, result: subprocess.CompletedProcess) -> str:
        self.assertTrue(
            self.curl_log.exists(),
            f"the script stopped before requesting any asset: {result.stderr}",
        )
        return self.curl_log.read_text(encoding="utf-8").splitlines()[0]

    def test_release_targets_are_selected_per_platform(self) -> None:
        cases = [
            ("Linux", "x86_64", "x86_64-unknown-linux-gnu"),
            ("Linux", "aarch64", "aarch64-unknown-linux-gnu"),
            ("Linux", "arm64", "aarch64-unknown-linux-gnu"),
            ("Darwin", "x86_64", "x86_64-apple-darwin"),
            ("Darwin", "arm64", "aarch64-apple-darwin"),
        ]
        for uname_s, uname_m, target in cases:
            with self.subTest(os=uname_s, arch=uname_m):
                if self.curl_log.exists():
                    self.curl_log.unlink()
                result = self.run_script(uname_s, uname_m)
                self.assertNotEqual(result.returncode, 0, "the stub curl always fails")
                self.assertIn(
                    f"mcptracer-v0.3.0-rc1-{target}.tar.gz",
                    self.requested_url(result),
                    result.stderr,
                )

    def test_an_unpublished_architecture_is_refused_before_any_download(self) -> None:
        result = self.run_script("Linux", "armv7l")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsupported Linux architecture: armv7l", result.stderr)
        self.assertFalse(self.curl_log.exists(), "nothing may be downloaded first")

    def test_latest_preview_requires_explicit_version_pin(self) -> None:
        (self.bin / "curl").write_text(
            """#!/bin/sh
for arg in "$@"; do url="$arg"; done
printf '%s\n' "$url" >> "$FAKE_CURL_LOG"
case "$url" in
  */releases/latest) printf '{\"tag_name\":\"v0.3.0-rc1\"}\n'; exit 0 ;;
  *) exit 22 ;;
esac
""",
            encoding="utf-8",
            newline="\n",
        )
        env = {
            **os.environ,
            "PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
            "FAKE_CURL_LOG": str(self.curl_log),
            "FAKE_UNAME_S": "Linux",
            "FAKE_UNAME_M": "x86_64",
            "MCPTRACER_INSTALL_DIR": str(self.tmp / "install"),
        }
        env.pop("MCPTRACER_VERSION", None)
        result = subprocess.run(
            [BASH, str(SCRIPT)], env=env, capture_output=True, text=True, timeout=60
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires an explicit version for preview tags", result.stderr)
        self.assertEqual(
            self.curl_log.read_text(encoding="utf-8").splitlines(),
            ["https://api.github.com/repos/ard12/mcptracer/releases/latest"],
        )

    def seed_existing_install(self) -> Path:
        install_dir = self.tmp / "install"
        install_dir.mkdir()
        binary = install_dir / "mcptracer"
        binary.write_bytes(b"known-good-install")
        return binary

    def assert_existing_install_preserved(self, binary: Path) -> None:
        self.assertEqual(binary.read_bytes(), b"known-good-install")
        self.assertEqual(list(binary.parent.glob(".mcptracer.*")), [])

    def test_download_failure_preserves_existing_install(self) -> None:
        installed = self.seed_existing_install()
        result, _asset = self.run_fake_install(
            b"#!/bin/sh\nprintf 'mcptracer 0.3.0-rc1\n'\n",
            fail_download=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("download failed", result.stderr)
        self.assert_existing_install_preserved(installed)

    def test_checksum_mismatch_preserves_existing_install(self) -> None:
        installed = self.seed_existing_install()
        result, _asset = self.run_fake_install(
            b"#!/bin/sh\nprintf 'mcptracer 0.3.0-rc1\n'\n",
            checksum="0" * 64,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assert_existing_install_preserved(installed)

    def test_unexecutable_binary_preserves_existing_install(self) -> None:
        installed = self.seed_existing_install()
        invalid = b"#!/definitely/not/a/real/interpreter\nexit 0\n"
        result, _asset = self.run_fake_install(invalid)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("downloaded binary does not run", result.stderr)
        self.assert_existing_install_preserved(installed)

    def test_version_mismatch_preserves_existing_install(self) -> None:
        installed = self.seed_existing_install()
        result, _asset = self.run_fake_install(
            b"#!/bin/sh\nprintf 'mcptracer 0.3.0-rc2\n'\n"
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("downloaded binary version mismatch", result.stderr)
        self.assert_existing_install_preserved(installed)

    def test_verified_binary_replaces_existing_install(self) -> None:
        installed = self.seed_existing_install()
        result, _asset = self.run_fake_install(
            b"#!/bin/sh\nprintf 'mcptracer 0.3.0-rc1\n'\n"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            installed.read_bytes(),
            b"#!/bin/sh\nprintf 'mcptracer 0.3.0-rc1\n'\n",
        )
        self.assertEqual(list(installed.parent.glob(".mcptracer.*")), [])


if __name__ == "__main__":
    unittest.main()
