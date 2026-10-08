"""Binary selection only: never download or execute a marker binary."""
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


SOURCE = Path(__file__).resolve().parents[1] / "packages/python/mcptracer/mcptracer/cli.py"
SPEC = importlib.util.spec_from_file_location("mcptracer_wrapper_under_test", SOURCE)
CLI = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CLI)


class WrapperDiscoveryTests(unittest.TestCase):
    def test_ambient_cargo_workspace_never_selects_a_dev_binary(self):
        with tempfile.TemporaryDirectory() as scratch:
            workspace = Path(scratch)
            (workspace / "Cargo.toml").write_text('[workspace]\nmembers=["crates/mcptracer-proxy"]\n')
            for profile in ("release", "debug"):
                binary = workspace / "target" / profile / "mcptracer"
                binary.parent.mkdir(parents=True)
                binary.write_text("dummy, not executable")
            package = workspace / ".venv/lib/python3/site-packages/mcptracer/cli.py"
            with patch.dict(os.environ, {}, clear=True), \
                 patch.object(CLI, "__file__", str(package)), \
                 patch.object(CLI, "get_platform_triple", return_value=("test-triple", "tar.gz", "mcptracer")), \
                 patch.object(CLI.Path, "home", return_value=workspace / "home"):
                self.assertIsNone(CLI.find_binary())

    def test_explicit_binary_selection_is_preserved(self):
        with tempfile.TemporaryDirectory() as scratch:
            binary = Path(scratch) / "chosen-binary"
            binary.write_text("dummy, not executable")
            with patch.dict(os.environ, {"MCPTRACER_BIN": str(binary)}, clear=True):
                self.assertEqual(CLI.find_binary(), binary)

    def test_invalid_explicit_selection_fails_closed(self):
        with tempfile.TemporaryDirectory() as scratch:
            for value in ("", str(Path(scratch) / "missing"), scratch):
                with self.subTest(value=value), patch.dict(os.environ, {"MCPTRACER_BIN": value}, clear=True):
                    with self.assertRaisesRegex(RuntimeError, "existing binary file"):
                        CLI.find_binary()
