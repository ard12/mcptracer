"""Keep declared distribution versions aligned without publishing registries."""

import json
from pathlib import Path
import re
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]


class ReleaseVersionContracts(unittest.TestCase):
    def test_distribution_versions_match_workspace(self):
        cargo = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        expected = cargo["workspace"]["package"]["version"]
        npm = json.loads((ROOT / "packages/npm/mcptracer/package.json").read_text(encoding="utf-8"))
        python = tomllib.loads((ROOT / "packages/python/mcptracer/pyproject.toml").read_text(encoding="utf-8"))
        self.assertEqual(npm["version"], expected)
        self.assertEqual(python["project"]["version"], expected)
        for manifest in (ROOT / "crates").glob("*/Cargo.toml"):
            crate = tomllib.loads(manifest.read_text(encoding="utf-8"))
            for name, dependency in crate.get("dependencies", {}).items():
                if name.startswith("mcptracer-"):
                    self.assertEqual(dependency["version"], "=" + expected)
        for path, variable in (
            ("packages/python/mcptracer/mcptracer/__init__.py", "__version__"),
            ("packages/python/mcptracer/mcptracer/cli.py", "VERSION"),
        ):
            text = (ROOT / path).read_text(encoding="utf-8")
            match = re.search(rf'^{variable} = "([^"]+)"', text, re.MULTILINE)
            self.assertIsNotNone(match)
            self.assertEqual(match.group(1), expected)
        lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
        for package in lock["package"]:
            if package["name"].startswith("mcptracer-"):
                self.assertEqual(package["version"], expected)


if __name__ == "__main__":
    unittest.main()
