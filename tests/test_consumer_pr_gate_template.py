"""Static contract checks for the published consumer PR-gate template."""
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parent.parent
TEMPLATE = ROOT / "examples" / "ci-templates" / "mcp-server-pr-gate.yml"


class ConsumerGateTemplateTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.source = TEMPLATE.read_text(encoding="utf-8")

    def test_uses_machine_readable_session_ids_and_supported_cli_contract(self) -> None:
        self.assertIn("mcptracer sessions list --db \"$DB_PATH\" --json", self.source)
        self.assertIn('mcptracer replay "$BASELINE_SESSION_ID" --db "$DB_PATH"', self.source)
        self.assertIn('mcptracer diff "$BASELINE_SESSION_ID" "$CANDIDATE_SESSION_ID" --db "$DB_PATH" --ignore-latency --json', self.source)
        self.assertIn('mcptracer assert "$CANDIDATE_SESSION_ID" --db "$DB_PATH" --golden "$BASELINE_SESSION_ID"', self.source)
        self.assertNotIn("--format", self.source)
        self.assertNotIn("diff baseline candidate", self.source)
        self.assertNotIn("|| true", self.source)

    def test_uses_a_base_revision_fixture_with_verified_checksum(self) -> None:
        self.assertIn("github.event.pull_request.base.sha || github.sha", self.source)
        self.assertIn("sha256sum -c fixtures/baseline.mtrace.sha256", self.source)
        self.assertIn("contents: read", self.source)
        self.assertEqual(self.source.count("persist-credentials: false"), 2)
        self.assertIn("ard12/mcptracer/.github/actions/mcptracer@v0.3.0-rc1", self.source)

    def test_summary_does_not_print_diff_payloads(self) -> None:
        self.assertIn("Keep the job summary useful without printing request/response data.", self.source)
        self.assertIn('key: len(report.get(key, []))', self.source)
        self.assertNotIn("cat \"$DIFF_REPORT\"", self.source)


if __name__ == "__main__":
    unittest.main()
