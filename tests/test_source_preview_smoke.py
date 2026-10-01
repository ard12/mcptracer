from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import source_preview_smoke as smoke


class DefaultCommandSurfaceTests(unittest.TestCase):
    def write_manifest(self, features):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        candidate = Path(temp.name)
        manifest = candidate / "crates" / "mcptracer-proxy" / "Cargo.toml"
        manifest.parent.mkdir(parents=True)
        lines = ["[features]"]
        for name, values in features.items():
            rendered = ", ".join(f'"{value}"' for value in values)
            lines.append(f"{name} = [{rendered}]")
        manifest.write_text("\n".join(lines) + "\n", encoding="utf-8")
        return candidate

    def test_public_source_without_labs_feature_allows_labs_and_hides_semantic(self):
        candidate = self.write_manifest({
            "default": [],
            "semantic-search": ["mcptracer-intel/semantic-search"],
        })
        self.assertEqual(smoke._default_forbidden_commands(candidate), {"semantic"})

    def test_python310_fallback_reads_multiline_feature_arrays(self):
        parsed = smoke._parse_cargo_features(
            "[package]\nname = \"sample\"\n[features]\ndefault = [\n  \"semantic-search\", # comment\n]\n"
            "semantic-search = [\"labs\", \"intel/semantic-search\"]\n"
        )
        self.assertEqual(parsed["default"], ["semantic-search"])
        self.assertEqual(parsed["semantic-search"], ["labs", "intel/semantic-search"])

    def test_private_default_hides_labs_and_semantic(self):
        candidate = self.write_manifest({
            "default": [],
            "labs": ["dep:mcptracer-intel"],
            "semantic-search": ["labs", "mcptracer-intel/semantic-search"],
        })
        self.assertEqual(
            smoke._default_forbidden_commands(candidate),
            {"index", "route", "optimize", "graph", "semantic"},
        )

    def test_default_feature_closure_enables_transitive_labs(self):
        candidate = self.write_manifest({
            "default": ["semantic-search"],
            "labs": ["dep:mcptracer-intel"],
            "semantic-search": ["labs", "mcptracer-intel/semantic-search"],
        })
        self.assertEqual(smoke._default_forbidden_commands(candidate), set())

    def test_help_requires_core_commands_and_rejects_only_forbidden_commands(self):
        help_output = """Commands:
  record       Record
  validate     Validate
  replay       Replay
  diff         Diff
  assert       Assert
  route        Route
Options:
"""
        self.assertEqual(
            smoke._command_surface_issues(help_output, 0, {"semantic"}),
            [],
        )
        self.assertTrue(
            smoke._command_surface_issues("Commands:\n  route Route\nOptions:\n", 0, {"route"})
        )


if __name__ == "__main__":
    unittest.main()
