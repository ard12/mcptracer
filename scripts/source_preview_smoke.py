#!/usr/bin/env python3
"""Install an exported MCPTracer candidate and run its first-use tutorial.

The install prefix and Cargo target directory are temporary. The runner uses
only files in the public candidate and invokes the installed binary explicitly.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Iterable

from verify_oss_export import MANIFEST_NAME, verify_export


@dataclass
class Step:
    name: str
    command: list[str]
    exit_code: int
    duration_seconds: float
    output_tail: str | None = None


def _display_command(command: list[str]) -> list[str]:
    return [str(item) for item in command]


def _run(
    name: str,
    command: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    steps: list[Step],
) -> bool:
    started = time.monotonic()
    try:
        completed = subprocess.run(
            command,
            cwd=cwd,
            env=env,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        output = completed.stdout or ""
        exit_code = completed.returncode
    except OSError as error:
        output = str(error)
        exit_code = 127
    duration = round(time.monotonic() - started, 3)
    steps.append(
        Step(
            name=name,
            command=_display_command(command),
            exit_code=exit_code,
            duration_seconds=duration,
            output_tail=output[-8000:] if exit_code else None,
        )
    )
    if exit_code:
        print(f"FAIL {name} (exit {exit_code})", file=sys.stderr)
        if output:
            print(output[-8000:], file=sys.stderr)
        return False
    print(f"PASS {name} ({duration:.1f}s)")
    return True


def _capture_version(command: list[str], cwd: Path, env: dict[str, str]) -> str:
    try:
        completed = subprocess.run(
            command,
            cwd=cwd,
            env=env,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
    except OSError as error:
        return f"unavailable: {error}"
    return (completed.stdout or "").strip().splitlines()[0] if completed.stdout else "unknown"


def _find_bash() -> Path | None:
    if os.name == "nt":
        candidates = [
            Path(os.environ.get("ProgramFiles", r"C:\Program Files"))
            / "Git"
            / "bin"
            / "bash.exe",
            Path(os.environ.get("ProgramFiles", r"C:\Program Files"))
            / "Git"
            / "usr"
            / "bin"
            / "bash.exe",
        ]
        for candidate in candidates:
            if candidate.is_file():
                return candidate
        found = shutil.which("bash")
        if found and "system32" not in found.lower() and "windowsapps" not in found.lower():
            return Path(found)
        return None
    found = shutil.which("bash")
    return Path(found) if found else None


def _bash_path(path: Path, bash: Path) -> str:
    if os.name != "nt":
        return str(path)
    cygpath_candidates = [
        bash.parent.parent / "usr" / "bin" / "cygpath.exe",
        bash.parent / "cygpath.exe",
    ]
    for cygpath in cygpath_candidates:
        if not cygpath.is_file():
            continue
        completed = subprocess.run(
            [str(cygpath), "-u", str(path)],
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        if completed.returncode == 0 and completed.stdout.strip():
            return completed.stdout.strip()
    raise RuntimeError("Git for Windows cygpath.exe was not found")


def _write_report(path: Path | None, report: dict[str, object]) -> None:
    if path is None:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def run_smoke(
    candidate: Path,
    *,
    expected_source_sha: str | None = None,
    report_json: Path | None = None,
) -> int:
    candidate = candidate.resolve()
    verified = verify_export(
        candidate,
        expected_source_sha=expected_source_sha,
    )
    steps: list[Step] = []
    report: dict[str, object] = {
        "format_version": 1,
        "candidate": str(candidate),
        "source_commit": verified.source_commit,
        "platform": platform.platform(),
        "python": platform.python_version(),
        "status": "fail",
        "steps": [],
    }
    if not verified.ok:
        report["failure"] = {
            "stage": "export-verification",
            "findings": [asdict(finding) for finding in verified.findings],
        }
        _write_report(report_json, report)
        print("FAIL export verification", file=sys.stderr)
        return 1

    bash = _find_bash()
    if bash is None:
        report["failure"] = {
            "stage": "prerequisite",
            "message": "Bash is required to run examples/rug-pull-demo/run.sh",
        }
        _write_report(report_json, report)
        print("FAIL Bash is required for the tutorial smoke", file=sys.stderr)
        return 1

    cargo = shutil.which("cargo")
    if cargo is None:
        report["failure"] = {
            "stage": "prerequisite",
            "message": "cargo was not found on PATH",
        }
        _write_report(report_json, report)
        print("FAIL cargo was not found on PATH", file=sys.stderr)
        return 1

    with tempfile.TemporaryDirectory(prefix="mcptracer-source-smoke-") as temporary:
        work = Path(temporary)
        prefix = work / "install"
        target = work / "target"
        env = os.environ.copy()
        env["CARGO_TARGET_DIR"] = str(target)
        env["CARGO_TERM_COLOR"] = "never"

        install = [
            cargo,
            "install",
            "--path",
            str(candidate / "crates" / "mcptracer-proxy"),
            "--locked",
            "--root",
            str(prefix),
        ]
        if not _run("source install", install, cwd=candidate, env=env, steps=steps):
            report["steps"] = [asdict(step) for step in steps]
            _write_report(report_json, report)
            return 1

        binary = prefix / "bin" / ("mcptracer.exe" if os.name == "nt" else "mcptracer")
        if not binary.is_file():
            report["failure"] = {
                "stage": "installed-binary",
                "message": f"cargo install did not create {binary}",
            }
            report["steps"] = [asdict(step) for step in steps]
            _write_report(report_json, report)
            print(f"FAIL installed binary is missing: {binary}", file=sys.stderr)
            return 1

        for name, arguments in (
            ("installed --version", ["--version"]),
            ("installed --help", ["--help"]),
        ):
            if not _run(
                name,
                [str(binary), *arguments],
                cwd=candidate,
                env=env,
                steps=steps,
            ):
                report["steps"] = [asdict(step) for step in steps]
                _write_report(report_json, report)
                return 1

        try:
            env["MCPTRACER_BIN"] = _bash_path(binary, bash)
            env["PYTHON"] = _bash_path(Path(sys.executable).resolve(), bash)
            demo_script = _bash_path(
                candidate / "examples" / "rug-pull-demo" / "run.sh",
                bash,
            )
        except RuntimeError as error:
            report["failure"] = {
                "stage": "path-conversion",
                "message": str(error),
            }
            report["steps"] = [asdict(step) for step in steps]
            _write_report(report_json, report)
            print(f"FAIL {error}", file=sys.stderr)
            return 1

        if not _run(
            "rug-pull tutorial",
            [str(bash), demo_script],
            cwd=candidate / "examples" / "rug-pull-demo",
            env=env,
            steps=steps,
        ):
            report["steps"] = [asdict(step) for step in steps]
            _write_report(report_json, report)
            return 1

        report.update(
            {
                "status": "pass",
                "cargo": _capture_version([cargo, "--version"], candidate, env),
                "rustc": _capture_version(["rustc", "--version"], candidate, env),
                "bash": str(bash),
                "steps": [asdict(step) for step in steps],
            }
        )
        _write_report(report_json, report)

    print(
        f"PASS source preview smoke source={verified.source_commit} "
        f"platform={platform.system()}"
    )
    return 0


def main(argv: Iterable[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "candidate", type=Path, help="generated public tree or fresh public checkout"
    )
    parser.add_argument("--expected-source-sha")
    parser.add_argument("--report-json", type=Path)
    args = parser.parse_args(argv)
    return run_smoke(
        args.candidate,
        expected_source_sha=args.expected_source_sha,
        report_json=args.report_json,
    )


if __name__ == "__main__":
    raise SystemExit(main())
