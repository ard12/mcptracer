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
import re
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


def _read_workspace_version(candidate: Path) -> str:
    """Parse `[workspace.package] version` out of Cargo.toml without running cargo.

    `--binary` mode must not shell out to cargo at all (the whole point is to
    exercise a prebuilt artifact on a machine that may have no Rust
    toolchain), so the expected version is read textually from the
    workspace's own Cargo.toml instead of `cargo metadata`.
    """
    cargo_toml = candidate / "Cargo.toml"
    if not cargo_toml.is_file():
        raise RuntimeError(f"Cargo.toml not found at {cargo_toml}")
    text = cargo_toml.read_text(encoding="utf-8")
    section = re.search(
        r"^\[workspace\.package\]\s*\n(.*?)(?=^\[|\Z)",
        text,
        re.MULTILINE | re.DOTALL,
    )
    if not section:
        raise RuntimeError(f"[workspace.package] section not found in {cargo_toml}")
    version = re.search(r'^version\s*=\s*"([^"]+)"', section.group(1), re.MULTILINE)
    if not version:
        raise RuntimeError(
            f"version key not found in [workspace.package] section of {cargo_toml}"
        )
    return version.group(1)


def _resolve_binary(path: Path) -> Path:
    resolved = path.resolve()
    if not resolved.is_file():
        raise RuntimeError(f"--binary path does not exist or is not a file: {resolved}")
    if os.name != "nt" and not os.access(resolved, os.X_OK):
        raise RuntimeError(f"--binary path is not executable: {resolved}")
    return resolved


def _assert_version(
    binary: Path,
    expected_version: str,
    *,
    cwd: Path,
    env: dict[str, str],
    steps: list[Step],
) -> bool:
    name = "version matches Cargo.toml declaration"
    command = [str(binary), "--version"]
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
    matched = exit_code == 0 and expected_version in output
    failure_text: str | None = None
    if not matched:
        failure_text = (
            f"expected {expected_version!r} in --version output, got: {output.strip()!r}"
            if output
            else f"expected {expected_version!r} in --version output, got nothing (exit {exit_code})"
        )
    steps.append(
        Step(
            name=name,
            command=_display_command(command),
            exit_code=0 if matched else (exit_code or 1),
            duration_seconds=duration,
            output_tail=failure_text,
        )
    )
    if not matched:
        print(f"FAIL {name}", file=sys.stderr)
        print(failure_text, file=sys.stderr)
        return False
    print(f"PASS {name} ({duration:.1f}s)")
    return True


def _run_binary_checks(
    binary: Path,
    candidate: Path,
    expected_version: str,
    bash: Path,
    env: dict[str, str],
    steps: list[Step],
    *,
    label: str,
) -> tuple[bool, dict[str, object] | None]:
    """Run --version/--help, the version assertion, and the rug-pull tutorial.

    Shared by both source-install and --binary mode so they exercise the
    installed/prebuilt binary identically. Returns (ok, failure) where
    `failure` is a ready-to-store report["failure"] payload for a stage that
    doesn't append its own Step (currently only path conversion), or None
    otherwise.
    """
    for name, arguments in (
        (f"{label} --version", ["--version"]),
        (f"{label} --help", ["--help"]),
    ):
        if not _run(name, [str(binary), *arguments], cwd=candidate, env=env, steps=steps):
            return False, None

    if not _assert_version(binary, expected_version, cwd=candidate, env=env, steps=steps):
        return False, None

    try:
        env["MCPTRACER_BIN"] = _bash_path(binary, bash)
        env["PYTHON"] = _bash_path(Path(sys.executable).resolve(), bash)
        demo_script = _bash_path(
            candidate / "examples" / "rug-pull-demo" / "run.sh",
            bash,
        )
    except RuntimeError as error:
        return False, {"stage": "path-conversion", "message": str(error)}

    if not _run(
        "rug-pull tutorial",
        [str(bash), demo_script],
        cwd=candidate / "examples" / "rug-pull-demo",
        env=env,
        steps=steps,
    ):
        return False, None

    return True, None


def run_smoke(
    candidate: Path,
    *,
    expected_source_sha: str | None = None,
    report_json: Path | None = None,
    binary: Path | None = None,
) -> int:
    candidate = candidate.resolve()
    steps: list[Step] = []
    report: dict[str, object] = {
        "format_version": 1,
        "candidate": str(candidate),
        "source_commit": None,
        "platform": platform.platform(),
        "python": platform.python_version(),
        "mode": "binary" if binary is not None else "source",
        "status": "fail",
        "steps": [],
    }

    resolved_binary: Path | None = None
    if binary is not None:
        # Resolve and validate the binary before anything else: this is the
        # whole point of --binary mode, so a bad path should fail fast and
        # clearly rather than after export verification or a Bash lookup.
        try:
            resolved_binary = _resolve_binary(binary)
        except RuntimeError as error:
            report["failure"] = {"stage": "prerequisite", "message": str(error)}
            _write_report(report_json, report)
            print(f"FAIL {error}", file=sys.stderr)
            return 1
        report["binary"] = str(resolved_binary)

    verified = verify_export(
        candidate,
        expected_source_sha=expected_source_sha,
    )
    report["source_commit"] = verified.source_commit
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

    try:
        expected_version = _read_workspace_version(candidate)
    except RuntimeError as error:
        report["failure"] = {"stage": "version-metadata", "message": str(error)}
        _write_report(report_json, report)
        print(f"FAIL {error}", file=sys.stderr)
        return 1

    if resolved_binary is not None:
        # Prebuilt-artifact mode: no cargo, no Rust toolchain required. Only
        # the checks against the already-built binary run.
        with tempfile.TemporaryDirectory(prefix="mcptracer-binary-smoke-") as temporary:
            work = Path(temporary)
            env = os.environ.copy()
            env["CARGO_TERM_COLOR"] = "never"
            # Not used for building anything in this mode, but harmless to
            # set in case the tutorial or its server shells out to cargo for
            # some unrelated reason; keeps behavior parity with source mode.
            env["CARGO_TARGET_DIR"] = str(work / "target")

            ok, failure = _run_binary_checks(
                resolved_binary,
                candidate,
                expected_version,
                bash,
                env,
                steps,
                label="binary",
            )
            if not ok:
                if failure is not None:
                    report["failure"] = failure
                    print(f"FAIL {failure['message']}", file=sys.stderr)
                report["steps"] = [asdict(step) for step in steps]
                _write_report(report_json, report)
                return 1

            report.update(
                {
                    "status": "pass",
                    "bash": str(bash),
                    "expected_version": expected_version,
                    "steps": [asdict(step) for step in steps],
                }
            )
            _write_report(report_json, report)

        print(
            f"PASS binary preview smoke source={verified.source_commit} "
            f"platform={platform.system()}"
        )
        return 0

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

        installed_binary = prefix / "bin" / ("mcptracer.exe" if os.name == "nt" else "mcptracer")
        if not installed_binary.is_file():
            report["failure"] = {
                "stage": "installed-binary",
                "message": f"cargo install did not create {installed_binary}",
            }
            report["steps"] = [asdict(step) for step in steps]
            _write_report(report_json, report)
            print(f"FAIL installed binary is missing: {installed_binary}", file=sys.stderr)
            return 1

        ok, failure = _run_binary_checks(
            installed_binary,
            candidate,
            expected_version,
            bash,
            env,
            steps,
            label="installed",
        )
        if not ok:
            if failure is not None:
                report["failure"] = failure
                print(f"FAIL {failure['message']}", file=sys.stderr)
            report["steps"] = [asdict(step) for step in steps]
            _write_report(report_json, report)
            return 1

        report.update(
            {
                "status": "pass",
                "cargo": _capture_version([cargo, "--version"], candidate, env),
                "rustc": _capture_version(["rustc", "--version"], candidate, env),
                "bash": str(bash),
                "expected_version": expected_version,
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
    parser.add_argument(
        "--binary",
        type=Path,
        help=(
            "Path to a prebuilt mcptracer binary (e.g. an extracted release "
            "archive). Skips `cargo install` entirely and runs the "
            "version/help checks and the rug-pull tutorial against this "
            "binary instead of one built from source. Combinable with "
            "--expected-source-sha."
        ),
    )
    args = parser.parse_args(argv)
    return run_smoke(
        args.candidate,
        expected_source_sha=args.expected_source_sha,
        report_json=args.report_json,
        binary=args.binary,
    )


if __name__ == "__main__":
    raise SystemExit(main())
