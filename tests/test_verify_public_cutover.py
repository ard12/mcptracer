#!/usr/bin/env python3
"""Synthetic response tests for the public cutover verifier."""

from __future__ import annotations

import base64
import importlib.util
import json
import sys
import unittest
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "verify_public_cutover", ROOT / "scripts" / "verify_public_cutover.py"
)
assert SPEC and SPEC.loader
CUTOVER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = CUTOVER
SPEC.loader.exec_module(CUTOVER)

REPOSITORY = "owner/repository"
PUBLIC_SHA = "a" * 40
SOURCE_SHA = "b" * 40


class FakeClient:
    def __init__(self, responses: dict[str, Any]) -> None:
        self.responses = responses
        self.requested: list[str] = []

    def get_api(self, path: str) -> Any:
        self.requested.append(path)
        return self.responses.get(
            path, CUTOVER.Response(500, error=f"unexpected request: {path}")
        )


def ready_responses() -> dict[str, Any]:
    api = f"/repos/{REPOSITORY}"
    manifest = base64.b64encode(
        json.dumps({"source_commit": SOURCE_SHA}).encode("utf-8")
    ).decode("ascii")
    jobs = [
        {"name": name, "status": "completed", "conclusion": "success"}
        for name in CUTOVER.EXPECTED_JOBS
    ]
    return {
        api: CUTOVER.Response(
            200,
            {
                "visibility": "public",
                "private": False,
                "default_branch": "main",
            },
        ),
        f"{api}/commits/main": CUTOVER.Response(200, {"sha": PUBLIC_SHA}),
        f"{api}/contents/OSS_EXPORT_MANIFEST.json?ref={PUBLIC_SHA}": CUTOVER.Response(
            200, {"content": manifest}
        ),
        f"{api}/actions/runs?head_sha={PUBLIC_SHA}&per_page=100": CUTOVER.Response(
            200,
            {
                "workflow_runs": [
                    {
                        "id": 7,
                        "name": "CI",
                        "created_at": "2026-09-08T00:00:00Z",
                        "status": "completed",
                        "conclusion": "success",
                        "html_url": "https://github.com/owner/repository/actions/runs/7",
                    }
                ]
            },
        ),
        f"{api}/actions/runs/7/jobs?per_page=100": CUTOVER.Response(
            200, {"jobs": jobs}
        ),
        f"{api}/private-vulnerability-reporting": CUTOVER.Response(
            200, {"enabled": True}
        ),
    }


def statuses(checks: list[Any]) -> dict[str, str]:
    return {check.name: check.status for check in checks}


class PublicCutoverTests(unittest.TestCase):
    def test_absent_or_inaccessible_repository_is_pending(self) -> None:
        client = FakeClient(
            {f"/repos/{REPOSITORY}": CUTOVER.Response(404, {"message": "Not Found"})}
        )
        checks = CUTOVER.evaluate_cutover(
            client, REPOSITORY, PUBLIC_SHA, SOURCE_SHA
        )
        self.assertEqual(statuses(checks), {"repository": "pending"})
        self.assertIn("does not prove name availability", checks[0].detail)

    def test_ready_snapshot_passes_every_check(self) -> None:
        checks = CUTOVER.evaluate_cutover(
            FakeClient(ready_responses()), REPOSITORY, PUBLIC_SHA, SOURCE_SHA
        )
        self.assertTrue(checks)
        self.assertEqual({check.status for check in checks}, {"passed"})

    def test_public_head_and_source_mismatches_fail(self) -> None:
        responses = ready_responses()
        api = f"/repos/{REPOSITORY}"
        responses[f"{api}/commits/main"] = CUTOVER.Response(200, {"sha": "c" * 40})
        manifest = base64.b64encode(
            json.dumps({"source_commit": "d" * 40}).encode("utf-8")
        ).decode("ascii")
        responses[
            f"{api}/contents/OSS_EXPORT_MANIFEST.json?ref={PUBLIC_SHA}"
        ] = CUTOVER.Response(200, {"content": manifest})
        checks = CUTOVER.evaluate_cutover(
            FakeClient(responses), REPOSITORY, PUBLIC_SHA, SOURCE_SHA
        )
        result = statuses(checks)
        self.assertEqual(result["remote HEAD"], "failed")
        self.assertEqual(result["export provenance"], "failed")

    def test_running_ci_is_pending_not_failed(self) -> None:
        responses = ready_responses()
        api = f"/repos/{REPOSITORY}"
        responses[
            f"{api}/actions/runs?head_sha={PUBLIC_SHA}&per_page=100"
        ] = CUTOVER.Response(
            200,
            {
                "workflow_runs": [
                    {
                        "id": 8,
                        "name": "CI",
                        "created_at": "2026-09-08T01:00:00Z",
                        "status": "in_progress",
                        "conclusion": None,
                    }
                ]
            },
        )
        checks = CUTOVER.evaluate_cutover(
            FakeClient(responses), REPOSITORY, PUBLIC_SHA, SOURCE_SHA
        )
        self.assertEqual(statuses(checks)["CI"], "pending")

    def test_failed_ci_and_failed_job_are_failures(self) -> None:
        responses = ready_responses()
        api = f"/repos/{REPOSITORY}"
        responses[f"{api}/actions/runs/7/jobs?per_page=100"].data["jobs"][0][
            "conclusion"
        ] = "failure"
        checks = CUTOVER.evaluate_cutover(
            FakeClient(responses), REPOSITORY, PUBLIC_SHA, SOURCE_SHA
        )
        self.assertEqual(statuses(checks)["CI jobs"], "failed")

    def test_missing_required_job_is_pending(self) -> None:
        responses = ready_responses()
        api = f"/repos/{REPOSITORY}"
        responses[f"{api}/actions/runs/7/jobs?per_page=100"].data["jobs"].pop()
        checks = CUTOVER.evaluate_cutover(
            FakeClient(responses), REPOSITORY, PUBLIC_SHA, SOURCE_SHA
        )
        self.assertEqual(statuses(checks)["CI jobs"], "pending")

    def test_anonymous_pvr_inaccessibility_is_pending(self) -> None:
        responses = ready_responses()
        responses[
            f"/repos/{REPOSITORY}/private-vulnerability-reporting"
        ] = CUTOVER.Response(403, {"message": "Forbidden"})
        checks = CUTOVER.evaluate_cutover(
            FakeClient(responses), REPOSITORY, PUBLIC_SHA, SOURCE_SHA
        )
        self.assertEqual(
            statuses(checks)["private vulnerability reporting"], "pending"
        )


if __name__ == "__main__":
    unittest.main()
