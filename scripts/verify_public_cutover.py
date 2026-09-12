#!/usr/bin/env python3
"""Read-only, anonymous verification of an MCPTracer public cutover.

Exit 0 is ready, 1 is failed, and 2 is pending. No authorization header is
sent and the optional clone smoke removes GitHub credentials from its process.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable, Protocol
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import Request, urlopen

API = "https://api.github.com"
API_VERSION = "2026-03-10"
REPO_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
EXPECTED_JOBS = {
    "Test (ubuntu-latest)", "Test (windows-latest)", "Test (macos-latest)",
    "Lint", "Security audit", "Docs build",
    "Real-SDK compatibility matrix (ubuntu-latest)",
    "Real-SDK compatibility matrix (windows-latest)",
    "Real-SDK compatibility matrix (macos-latest)",
    "MSRV (Rust 1.85) (ubuntu-latest)",
    "MSRV (Rust 1.85) (windows-latest)",
    "MSRV (Rust 1.85) (macos-latest)",
}


@dataclass(frozen=True)
class Response:
    status: int | None
    data: Any = None
    error: str | None = None


@dataclass(frozen=True)
class Check:
    name: str
    status: str
    detail: str
    url: str | None = None


class Client(Protocol):
    def get_api(self, path: str) -> Response: ...


class AnonymousGitHubClient:
    """Small HTTP client with deliberately no credential support."""

    def get_api(self, path: str) -> Response:
        request = Request(
            f"{API}{path}",
            headers={
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": API_VERSION,
                "User-Agent": "mcptracer-cutover-verifier",
            },
            method="GET",
        )
        try:
            with urlopen(request, timeout=30) as response:
                body, status, error = response.read(), response.status, None
        except HTTPError as caught:
            body, status, error = caught.read(), caught.code, str(caught)
        except (URLError, TimeoutError, OSError) as caught:
            return Response(None, error=str(caught))
        try:
            data = json.loads(body.decode("utf-8")) if body else None
        except (UnicodeError, json.JSONDecodeError):
            data = body.decode("utf-8", errors="replace")
        return Response(status, data, error)


def _add(
    checks: list[Check], name: str, status: str, detail: str, url: str | None = None
) -> None:
    checks.append(Check(name, status, detail, url))


def _error(checks: list[Check], name: str, response: Response, url: str) -> None:
    _add(
        checks, name, "failed",
        f"request failed (HTTP {response.status}): {response.error or 'unexpected response'}",
        url,
    )


def _manifest(response: Response) -> dict[str, Any] | None:
    if not isinstance(response.data, dict):
        return None
    value = response.data
    if isinstance(value.get("content"), str):
        try:
            value = json.loads(base64.b64decode(value["content"]).decode("utf-8"))
        except (ValueError, UnicodeError, json.JSONDecodeError):
            return None
    return value if isinstance(value, dict) else None


def evaluate_cutover(
    client: Client, repository: str, public_sha: str, source_sha: str
) -> list[Check]:
    checks: list[Check] = []
    web = f"https://github.com/{repository}"
    api = f"/repos/{repository}"
    repo = client.get_api(api)
    if repo.status == 404:
        _add(
            checks, "repository", "pending",
            "absent or inaccessible anonymously; this does not prove name availability",
            web,
        )
        return checks
    if repo.status != 200 or not isinstance(repo.data, dict):
        _error(checks, "repository", repo, web)
        return checks

    if repo.data.get("visibility") == "public" and repo.data.get("private") is False:
        _add(checks, "visibility", "passed", "repository is public", web)
    else:
        _add(
            checks, "visibility", "failed",
            f"visibility={repo.data.get('visibility')!r}, private={repo.data.get('private')!r}",
            web,
        )

    branch = repo.data.get("default_branch")
    if not isinstance(branch, str) or not branch:
        _add(checks, "remote HEAD", "failed", "default branch is missing", web)
    else:
        head = client.get_api(f"{api}/commits/{quote(branch, safe='')}")
        actual = head.data.get("sha") if isinstance(head.data, dict) else None
        if head.status != 200:
            _error(checks, "remote HEAD", head, f"{web}/commits/{branch}")
        elif actual == public_sha:
            _add(
                checks, "remote HEAD", "passed", f"{branch} points to {actual}",
                f"{web}/commit/{actual}",
            )
        else:
            _add(
                checks, "remote HEAD", "failed",
                f"expected {public_sha}, found {actual}", f"{web}/commits/{branch}",
            )

    manifest_response = client.get_api(
        f"{api}/contents/OSS_EXPORT_MANIFEST.json?ref={quote(public_sha, safe='')}"
    )
    manifest = _manifest(manifest_response)
    manifest_url = f"{web}/blob/{public_sha}/OSS_EXPORT_MANIFEST.json"
    if manifest_response.status != 200 or manifest is None:
        _error(checks, "export provenance", manifest_response, manifest_url)
    elif manifest.get("source_commit") == source_sha:
        _add(
            checks, "export provenance", "passed",
            f"manifest records source {source_sha}", manifest_url,
        )
    else:
        _add(
            checks, "export provenance", "failed",
            f"expected source {source_sha}, found {manifest.get('source_commit')}",
            manifest_url,
        )

    runs = client.get_api(
        f"{api}/actions/runs?head_sha={quote(public_sha, safe='')}&per_page=100"
    )
    if runs.status != 200 or not isinstance(runs.data, dict):
        _error(checks, "CI", runs, f"{web}/actions")
    else:
        matching = [
            run for run in runs.data.get("workflow_runs", [])
            if isinstance(run, dict) and run.get("name") == "CI"
        ]
        matching.sort(key=lambda run: str(run.get("created_at", "")), reverse=True)
        if not matching:
            _add(
                checks, "CI", "pending", f"no CI run found for {public_sha}",
                f"{web}/actions/workflows/ci.yml",
            )
        else:
            run = matching[0]
            run_url = run.get("html_url") or f"{web}/actions"
            if run.get("status") != "completed":
                _add(checks, "CI", "pending", f"CI is {run.get('status')}", run_url)
            elif run.get("conclusion") != "success":
                _add(
                    checks, "CI", "failed",
                    f"CI concluded {run.get('conclusion')}", run_url,
                )
            else:
                jobs_response = client.get_api(
                    f"{api}/actions/runs/{run.get('id')}/jobs?per_page=100"
                )
                if jobs_response.status != 200 or not isinstance(
                    jobs_response.data, dict
                ):
                    _error(checks, "CI jobs", jobs_response, run_url)
                else:
                    jobs = {
                        job.get("name"): job
                        for job in jobs_response.data.get("jobs", [])
                        if isinstance(job, dict) and isinstance(job.get("name"), str)
                    }
                    missing = sorted(EXPECTED_JOBS - set(jobs))
                    incomplete = sorted(
                        name for name in EXPECTED_JOBS & set(jobs)
                        if jobs[name].get("status") != "completed"
                    )
                    failed = sorted(
                        name for name in EXPECTED_JOBS & set(jobs)
                        if jobs[name].get("status") == "completed"
                        and jobs[name].get("conclusion") != "success"
                    )
                    if failed:
                        _add(
                            checks, "CI jobs", "failed",
                            "unsuccessful: " + ", ".join(failed), run_url,
                        )
                    elif missing or incomplete:
                        parts = []
                        if missing:
                            parts.append("missing: " + ", ".join(missing))
                        if incomplete:
                            parts.append("incomplete: " + ", ".join(incomplete))
                        _add(checks, "CI jobs", "pending", "; ".join(parts), run_url)
                    else:
                        _add(
                            checks, "CI jobs", "passed",
                            f"all {len(EXPECTED_JOBS)} expected jobs succeeded", run_url,
                        )

    pvr = client.get_api(f"{api}/private-vulnerability-reporting")
    security_url = f"{web}/security/advisories/new"
    if pvr.status == 200 and isinstance(pvr.data, dict):
        if pvr.data.get("enabled") is True:
            _add(
                checks, "private vulnerability reporting", "passed",
                "enabled", security_url,
            )
        else:
            _add(
                checks, "private vulnerability reporting", "failed",
                "disabled", security_url,
            )
    elif pvr.status in {403, 404}:
        _add(
            checks, "private vulnerability reporting", "pending",
            "status unavailable anonymously; verify in repository security settings",
            security_url,
        )
    else:
        _error(checks, "private vulnerability reporting", pvr, security_url)
    return checks


def anonymous_clone_smoke(repository: str, public_sha: str, source_sha: str) -> Check:
    git = shutil.which("git")
    if not git:
        return Check("outsider smoke", "failed", "git was not found on PATH")
    with tempfile.TemporaryDirectory(prefix="mcptracer-outsider-") as temporary:
        root, checkout = Path(temporary), Path(temporary) / "checkout"
        config = root / "empty.gitconfig"
        config.write_text("", encoding="utf-8")
        env = os.environ.copy()
        blocked_names = {
            "GH_TOKEN", "GITHUB_TOKEN", "GIT_ASKPASS", "SSH_ASKPASS",
            "SSH_AUTH_SOCK", "GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS",
            "GIT_HTTP_EXTRAHEADER", "GIT_SSH_COMMAND",
        }
        for name in list(env):
            if name in blocked_names or name.startswith("GIT_CONFIG_KEY_") or name.startswith("GIT_CONFIG_VALUE_"):
                env.pop(name, None)
        env.update({
            "GIT_CONFIG_GLOBAL": str(config),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_TERMINAL_PROMPT": "0",
            "GCM_INTERACTIVE": "Never",
        })
        url = f"https://github.com/{repository}.git"
        clone = subprocess.run(
            [git, "-c", "credential.helper=", "clone", "--depth", "1", "--no-tags", url, str(checkout)],
            env=env, check=False, text=True,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        )
        if clone.returncode:
            return Check(
                "outsider smoke", "failed",
                f"anonymous clone failed ({clone.returncode}): {(clone.stdout or '')[-2000:]}",
                url,
            )
        head = subprocess.run(
            [git, "-C", str(checkout), "rev-parse", "HEAD"],
            env=env, check=False, text=True,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        )
        actual = (head.stdout or "").strip()
        if head.returncode or actual != public_sha:
            return Check(
                "outsider smoke", "failed",
                f"fresh HEAD expected {public_sha}, found {actual}", url,
            )
        smoke = subprocess.run(
            [
                sys.executable, str(checkout / "scripts" / "source_preview_smoke.py"),
                str(checkout), "--expected-source-sha", source_sha,
            ],
            cwd=checkout, env=env, check=False, text=True,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        )
        if smoke.returncode:
            return Check(
                "outsider smoke", "failed",
                f"install/tutorial failed ({smoke.returncode}): {(smoke.stdout or '')[-4000:]}",
                url,
            )
        return Check(
            "outsider smoke", "passed",
            "anonymous clone, isolated source install, and tutorial passed", url,
        )


def _write(path: Path | None, value: dict[str, Any]) -> None:
    if path:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def main(argv: Iterable[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repository", help="public owner/repository")
    parser.add_argument("--expected-public-commit", required=True)
    parser.add_argument("--expected-source-sha", required=True)
    parser.add_argument("--run-outsider-smoke", action="store_true")
    parser.add_argument("--report-json", type=Path)
    args = parser.parse_args(argv)
    if not REPO_RE.fullmatch(args.repository):
        parser.error("repository must be owner/name")
    if not SHA_RE.fullmatch(args.expected_public_commit):
        parser.error("--expected-public-commit must be 40 lowercase hex characters")
    if not SHA_RE.fullmatch(args.expected_source_sha):
        parser.error("--expected-source-sha must be 40 lowercase hex characters")

    checked = datetime.now(timezone.utc).isoformat()
    checks = evaluate_cutover(
        AnonymousGitHubClient(), args.repository,
        args.expected_public_commit, args.expected_source_sha,
    )
    visible = any(
        check.name == "visibility" and check.status == "passed" for check in checks
    )
    if args.run_outsider_smoke:
        checks.append(
            anonymous_clone_smoke(
                args.repository, args.expected_public_commit, args.expected_source_sha
            )
            if visible
            else Check(
                "outsider smoke", "pending",
                "not attempted until anonymous visibility is confirmed",
                f"https://github.com/{args.repository}",
            )
        )
    failed = any(check.status == "failed" for check in checks)
    pending = any(check.status == "pending" for check in checks)
    status = "failed" if failed else "pending" if pending else "ready"
    report = {
        "format_version": 1,
        "checked_at": checked,
        "repository": args.repository,
        "expected_public_commit": args.expected_public_commit,
        "expected_source_commit": args.expected_source_sha,
        "platform": platform.platform(),
        "credential_mode": "anonymous; no authorization header or GitHub token",
        "status": status,
        "checks": [asdict(check) for check in checks],
    }
    _write(args.report_json, report)
    for check in checks:
        print(f"{check.status.upper():7} {check.name}: {check.detail}")
        if check.url:
            print(f"        {check.url}")
    print(f"RESULT  {status} at {checked}")
    return 1 if failed else 2 if pending else 0


if __name__ == "__main__":
    raise SystemExit(main())
