"""Catch release/rehearsal compatibility drift before scheduling builds."""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]


class ReleaseWorkflowContracts(unittest.TestCase):
    def setUp(self):
        self.release = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        self.rehearsal = (ROOT / ".github/workflows/release-rehearsal.yml").read_text(encoding="utf-8")

    def targets(self, workflow):
        return re.findall(
            r"- os: ([^\n]+)\n\s+target: ([^\n]+)\n\s+archive_ext: ([^\n]+)",
            workflow,
        )

    def test_rehearsal_uses_release_environments_for_all_five_targets(self):
        targets = self.targets(self.release)
        self.assertEqual(len(targets), 5)
        self.assertEqual(self.targets(self.rehearsal), targets)
        linux = {target: runner for runner, target, _ in targets if "linux-gnu" in target}
        self.assertEqual(linux, {
            "x86_64-unknown-linux-gnu": "ubuntu-22.04",
            "aarch64-unknown-linux-gnu": "ubuntu-22.04-arm",
        })

    def test_both_workflows_check_built_and_extracted_elf(self):
        for workflow in (self.release, self.rehearsal):
            self.assertEqual(workflow.count("python scripts/check_glibc_compat.py"), 2)
            self.assertEqual(workflow.count("--max-glibc 2.35"), 2)
            self.assertNotIn("readelf --version-info", workflow)
            self.assertIn("name: Check packaged Linux glibc requirements", workflow)
        self.assertIn("package-smoke/", self.release)
        self.assertIn('env.EXTRACTED', self.rehearsal)

    def test_rehearsal_cannot_publish_a_release(self):
        self.assertIn("  pull_request:\n    branches: [main]", self.rehearsal)
        self.assertIn("REHEARSAL_VERSION:", self.rehearsal)
        self.assertIn("inputs.version || 'source-candidate'", self.rehearsal)
        self.assertNotIn("softprops/action-gh-release", self.rehearsal)
        self.assertNotRegex(self.rehearsal, r"(?m)^\s+contents:\s+write\s*$")
        self.assertNotRegex(self.rehearsal, r"(?m)^\s+push:\s*$")

    def test_ci_uses_explicit_discovery(self):
        ci = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn('python -m unittest discover -s tests -p "test_*.py" -v', ci)
        self.assertNotIn("python -m unittest tests.", ci)


if __name__ == "__main__":
    unittest.main()
