#!/usr/bin/env python3
"""MCPTracer real-SDK compatibility matrix driver.

Runs the full six-step evidence chain (record -> validate -> assert -> export
+verify -> replay+diff -> rug-pull detection) against each cell in
matrix.toml. Zero external Python dependencies -- stdlib only (tomllib
requires Python 3.11+), matching tests/schema_validator.py's precedent.

Usage:
    python tests/compat/run_matrix.py                 # run every required cell
    python tests/compat/run_matrix.py --only ts-stdio  # run one cell by id
    python tests/compat/run_matrix.py --all            # include non-required cells

Honors MCPTRACER_BIN (default "mcptracer") so CI can point at a built release
binary instead of relying on it being on PATH.
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

COMPAT_ROOT = Path(__file__).resolve().parent
CLIENT_SCRIPT = COMPAT_ROOT / "client_scripts" / "baseline.jsonl"
RECORD_SESSION_ID_RE = re.compile(r"recording session (\S+) ->")
REPLAY_SESSION_ID_RE = re.compile(r"replaying session \S+ as (\S+) ->")
HTTP_REPLAY_SESSION_ID_RE = re.compile(r"HTTP-replaying session \S+ as (\S+) ->")


def mcptracer_bin() -> str:
    return os.environ.get("MCPTRACER_BIN", "mcptracer")


@dataclass
class StepResult:
    name: str
    ok: bool
    detail: str = ""


@dataclass
class CellResult:
    cell_id: str
    required: bool
    steps: list[StepResult] = field(default_factory=list)

    @property
    def passed(self) -> bool:
        return all(s.ok for s in self.steps)

    def record(self, name: str, ok: bool, detail: str = "") -> bool:
        self.steps.append(StepResult(name, ok, detail))
        return ok


def run_cli(db: Path, *args: str, input_bytes: bytes | None = None,
            env: dict[str, str] | None = None, timeout: int = 30) -> subprocess.CompletedProcess:
    full_env = dict(os.environ)
    if env:
        full_env.update(env)
    return subprocess.run(
        [mcptracer_bin(), "--db", str(db), *args],
        input=input_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=full_env,
        timeout=timeout,
    )


def extract_session_id(stderr: bytes, pattern: re.Pattern = RECORD_SESSION_ID_RE) -> str | None:
    match = pattern.search(stderr.decode("utf-8", errors="replace"))
    return match.group(1) if match else None


def record_session(db: Path, server_dir: Path, command: list[str], client: str,
                    extra_env: dict[str, str] | None = None) -> tuple[str | None, str]:
    with open(CLIENT_SCRIPT, "rb") as f:
        client_bytes = f.read()
    full_command = [*command]
    proc = subprocess.run(
        [mcptracer_bin(), "--db", str(db), "record", "--client", client, "--", *full_command],
        input=client_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=server_dir,
        env={**os.environ, **(extra_env or {})},
        timeout=30,
    )
    stderr_text = proc.stderr.decode("utf-8", errors="replace")
    if proc.returncode != 0:
        return None, f"record exited {proc.returncode}: {stderr_text}"
    session_id = extract_session_id(proc.stderr)
    if not session_id:
        return None, f"could not extract session id from stderr: {stderr_text}"
    return session_id, stderr_text


def run_stdio_cell(cell: dict, workdir: Path) -> CellResult:
    cell_id = cell["id"]
    result = CellResult(cell_id=cell_id, required=cell.get("required", True))
    server_dir = COMPAT_ROOT / cell["server_dir"]
    command = cell["command"]
    rugpull_env = cell.get("rugpull_env", "MCPTRACER_COMPAT_RUGPULL")
    db = workdir / "compat.db"

    # Step 1: record the trusted baseline.
    baseline_id, detail = record_session(db, server_dir, command, cell_id)
    if not result.record("record baseline", baseline_id is not None, detail):
        return result

    # Step 2: validate capture integrity.
    proc = run_cli(db, "validate", baseline_id)
    if not result.record("validate", proc.returncode == 0, proc.stderr.decode("utf-8", "replace")):
        return result

    # Step 3: assert --spec pin.toml PASS against the trusted contract. Every
    # cell carries its own pin.toml (bootstrapped once per server the same
    # way examples/rug-pull-demo/pin.toml was: `assert --spec` with an empty
    # hashes table prints the real hash to copy in) -- there is no fallback
    # path, since assert requires exactly one of --spec/--golden.
    pin_path = server_dir / "pin.toml"
    if not pin_path.exists():
        result.record("assert pin (baseline, expect PASS)", False, f"no pin.toml at {pin_path}")
        return result
    proc = run_cli(db, "assert", baseline_id, "--spec", str(pin_path))
    result.record("assert pin (baseline, expect PASS)", proc.returncode == 0,
                   proc.stdout.decode("utf-8", "replace"))

    # Step 4: export + offline verify MATCH.
    artifact = workdir / "baseline.mtrace"
    manifest = workdir / "baseline.manifest.json"
    proc = run_cli(db, "export", baseline_id, "--out", str(artifact), "--allow-unredacted")
    export_ok = proc.returncode == 0
    result.record("export", export_ok, proc.stderr.decode("utf-8", "replace"))
    if export_ok:
        proc = run_cli(db, "assert", baseline_id, "--spec", str(pin_path), "--manifest", str(manifest),
                        "--allow-unredacted")
        manifest_ok = manifest.exists()
        result.record("write evidence manifest", manifest_ok, proc.stderr.decode("utf-8", "replace"))
        if manifest_ok:
            proc = run_cli(db, "verify", str(manifest), "--artifact", str(artifact),
                            "--assert-spec", str(pin_path))
            result.record("verify offline (expect MATCH)", proc.returncode == 0,
                           proc.stdout.decode("utf-8", "replace") + proc.stderr.decode("utf-8", "replace"))

    # Step 5: replay against a fresh server instance; diff must find nothing
    # meaningful (this is the real compatibility test -- the SDK server is
    # driven a second time, independently, from a fresh process).
    proc = subprocess.run(
        [mcptracer_bin(), "--db", str(db), "replay", baseline_id, "--i-understand-side-effects",
         "--", *command],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, cwd=server_dir, timeout=30,
    )
    replay_id = extract_session_id(proc.stderr, REPLAY_SESSION_ID_RE)
    replay_ok = proc.returncode == 0 and replay_id is not None
    result.record("replay against fresh instance", replay_ok, proc.stderr.decode("utf-8", "replace"))
    if replay_ok:
        proc = run_cli(db, "diff", baseline_id, replay_id, "--ignore-latency")
        result.record("diff baseline vs replay (expect no meaningful diff, exit 0)",
                       proc.returncode == 0, proc.stdout.decode("utf-8", "replace"))

    # Step 6: the product's core claim, exercised against a real SDK for the
    # first time -- a silent contract change must be caught even though the
    # call/response text on the wire never changes.
    candidate_id, detail = record_session(db, server_dir, command, cell_id,
                                           extra_env={rugpull_env: "1"})
    if result.record("record rug-pulled candidate", candidate_id is not None, detail):
        proc = run_cli(db, "diff", baseline_id, candidate_id, "--ignore-latency")
        result.record("diff baseline vs rug-pulled candidate (expect exit 1)",
                       proc.returncode == 1, proc.stdout.decode("utf-8", "replace"))
        if pin_path.exists():
            proc = run_cli(db, "assert", candidate_id, "--spec", str(pin_path))
            result.record("assert pin (rug-pulled candidate, expect exit 1)",
                           proc.returncode == 1, proc.stdout.decode("utf-8", "replace"))

    return result


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def wait_for_port(host: str, port: int, proc: subprocess.Popen | None = None,
                   timeout: float = 15.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc is not None and proc.poll() is not None:
            return False
        try:
            with socket.create_connection((host, port), timeout=0.5):
                return True
        except OSError:
            time.sleep(0.1)
    return False


def stop_process(proc: subprocess.Popen) -> None:
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)


def http_post(port: int, path: str, payload: dict,
              headers: dict[str, str]) -> tuple[int, dict[str, str], bytes]:
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        conn.request("POST", path, body=json.dumps(payload, separators=(",", ":")).encode("utf-8"),
                     headers=headers)
        response = conn.getresponse()
        return (response.status, {name.lower(): value for name, value in response.getheaders()},
                response.read())
    finally:
        conn.close()


def http_delete(port: int, path: str, headers: dict[str, str]) -> int:
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        conn.request("DELETE", path, headers=headers)
        response = conn.getresponse()
        response.read()
        return response.status
    finally:
        conn.close()


def parse_http_body(content_type: str, body: bytes) -> list[dict]:
    """Real SDK servers may answer with a bare JSON body or an SSE stream
    (one or more `data: {...}` lines) -- a real client has to handle either,
    since which one a given server (or even a given request) picks is the
    server's choice, not something this harness controls."""
    if not body:
        return []
    if "text/event-stream" in content_type:
        messages = []
        for line in body.decode("utf-8", errors="replace").splitlines():
            if line.startswith("data:"):
                messages.append(json.loads(line[len("data:"):].strip()))
        return messages
    return [json.loads(body.decode("utf-8", errors="replace"))]


def drive_http_client_script(proxy_port: int, client_name: str) -> str | None:
    """POSTs client_scripts/baseline.jsonl's messages through the proxy at
    `proxy_port`, threading the server-assigned Mcp-Session-Id from the
    first response into every subsequent request the way a real Streamable
    HTTP client must, then sends a terminating DELETE -- the MCP Streamable
    HTTP transport's own session-termination mechanism, and the only way
    record-http finalizes ("closes") the logical session immediately rather
    than leaving it open. Without this, `mcptracer validate` reports
    SessionNotClosed even though every message was captured correctly:
    killing the record-http process instead does not flush ended_at_ns.
    Returns the session id, or None if one was never assigned (a
    stateless-mode server)."""
    session_id: str | None = None
    with open(CLIENT_SCRIPT, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            message = json.loads(line)
            headers = {
                "Content-Type": "application/json",
                "Accept": "application/json, text/event-stream",
                "MCP-Protocol-Version": "2025-06-18",
                "Mcp-Name": client_name,
            }
            if session_id:
                headers["Mcp-Session-Id"] = session_id
            status, response_headers, body = http_post(proxy_port, "/mcp", message, headers)
            if status not in (200, 202):
                raise RuntimeError(f"HTTP {status} posting {message.get('method')}: {body!r}")
            if "mcp-session-id" in response_headers:
                session_id = response_headers["mcp-session-id"]
    if session_id:
        http_delete(proxy_port, "/mcp", {"Mcp-Session-Id": session_id})
    return session_id


def session_has_method(db: Path, session_id: str, method: str) -> bool:
    proc = run_cli(db, "sessions", "show", session_id, "--json")
    if proc.returncode != 0:
        return False
    messages = json.loads(proc.stdout.decode("utf-8", "replace") or "[]")
    return any(m.get("method") == method for m in messages)


def latest_recorded_session(db: Path) -> tuple[str | None, str]:
    """The session(s) record-http just wrote, as ONE session id ready for
    validate/assert/export/replay.

    T-70's logical-session partitioning (deliberate, documented in
    docs/spec/transport-http.md) means a client that sends `initialize`
    without an Mcp-Session-Id header -- true of every real Streamable HTTP
    client on its first request, since the server hasn't assigned one yet
    -- gets that single exchange recorded as its own short-lived
    "provisional" session, separate from the session the rest of the
    traffic lands in once the server-assigned id is known. Two sessions
    for one logical client run is therefore normal here, not a bug: this
    merges them back into one via `mcptracer merge`, in the correct
    (initialize-first) order, exactly the way a reader piecing the capture
    back together by hand would have to.
    """
    proc = run_cli(db, "sessions", "list", "--json", "--limit", "2")
    if proc.returncode != 0:
        return None, proc.stderr.decode("utf-8", "replace")
    sessions = json.loads(proc.stdout.decode("utf-8", "replace") or "[]")
    if not sessions:
        return None, "no session appeared in the database after record-http"
    if len(sessions) == 1:
        return sessions[0]["id"], ""

    newer, older = sessions[0]["id"], sessions[1]["id"]
    newer_has_init = session_has_method(db, newer, "initialize")
    older_has_init = session_has_method(db, older, "initialize")
    if older_has_init and not newer_has_init:
        # The expected shape: the provisional initialize-only session
        # started (and is chronologically "older") before the main session
        # that followed once the server assigned a real Mcp-Session-Id.
        merge_order = [older, newer]
    elif newer_has_init and not older_has_init:
        merge_order = [newer, older]
    else:
        return None, (
            f"could not identify which of the two most recent sessions is the "
            f"provisional initialize-only one (newer={newer} has_init={newer_has_init}, "
            f"older={older} has_init={older_has_init})"
        )

    proc = run_cli(db, "merge", *merge_order, "--json")
    if proc.returncode != 0:
        return None, f"merge failed: {proc.stderr.decode('utf-8', 'replace')}"
    merged = json.loads(proc.stdout.decode("utf-8", "replace"))
    return merged["session_id"], ""


def run_http_cell(cell: dict, workdir: Path) -> CellResult:
    cell_id = cell["id"]
    result = CellResult(cell_id=cell_id, required=cell.get("required", True))
    server_dir = COMPAT_ROOT / cell["server_dir"]
    command = cell["command"]
    rugpull_env = cell.get("rugpull_env", "MCPTRACER_COMPAT_RUGPULL")
    pin_path = server_dir / "pin.toml"
    db = workdir / "compat.db"

    def record_via_http(extra_env: dict[str, str] | None = None) -> tuple[str | None, str]:
        upstream_port = free_port()
        proxy_port = free_port()
        upstream = subprocess.Popen(
            command, cwd=server_dir,
            env={**os.environ, **(extra_env or {}), "PORT": str(upstream_port)},
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        proxy = None
        try:
            if not wait_for_port("127.0.0.1", upstream_port, upstream):
                stderr = upstream.stderr.read().decode("utf-8", "replace") if upstream.stderr else ""
                return None, f"upstream server never opened port {upstream_port}: {stderr}"
            proxy = subprocess.Popen(
                [mcptracer_bin(), "--db", str(db), "record-http",
                 "--listen", f"127.0.0.1:{proxy_port}",
                 "--target", f"http://127.0.0.1:{upstream_port}",
                 "--client", cell_id],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            )
            if not wait_for_port("127.0.0.1", proxy_port, proxy):
                stderr = proxy.stderr.read().decode("utf-8", "replace") if proxy.stderr else ""
                return None, f"record-http never opened port {proxy_port}: {stderr}"
            try:
                drive_http_client_script(proxy_port, cell_id)
                time.sleep(0.2)
            except Exception as exc:  # noqa: BLE001 -- surfaced as a step failure, not a crash
                return None, f"driving client script failed: {exc}"
        finally:
            if proxy is not None:
                stop_process(proxy)
            stop_process(upstream)

        return latest_recorded_session(db)

    # Step 1: record the trusted baseline.
    baseline_id, detail = record_via_http()
    if not result.record("record baseline", baseline_id is not None, detail):
        return result

    # Step 2: validate capture integrity.
    proc = run_cli(db, "validate", baseline_id)
    if not result.record("validate", proc.returncode == 0, proc.stderr.decode("utf-8", "replace")):
        return result

    # Step 3: assert --spec pin.toml PASS.
    if not pin_path.exists():
        result.record("assert pin (baseline, expect PASS)", False, f"no pin.toml at {pin_path}")
        return result
    proc = run_cli(db, "assert", baseline_id, "--spec", str(pin_path))
    result.record("assert pin (baseline, expect PASS)", proc.returncode == 0,
                   proc.stdout.decode("utf-8", "replace"))

    # Step 4: export + offline verify MATCH.
    artifact = workdir / "baseline.mtrace"
    manifest = workdir / "baseline.manifest.json"
    proc = run_cli(db, "export", baseline_id, "--out", str(artifact), "--allow-unredacted")
    export_ok = proc.returncode == 0
    result.record("export", export_ok, proc.stderr.decode("utf-8", "replace"))
    if export_ok:
        proc = run_cli(db, "assert", baseline_id, "--spec", str(pin_path), "--manifest", str(manifest),
                        "--allow-unredacted")
        manifest_ok = manifest.exists()
        result.record("write evidence manifest", manifest_ok, proc.stderr.decode("utf-8", "replace"))
        if manifest_ok:
            proc = run_cli(db, "verify", str(manifest), "--artifact", str(artifact),
                            "--assert-spec", str(pin_path))
            result.record("verify offline (expect MATCH)", proc.returncode == 0,
                           proc.stdout.decode("utf-8", "replace") + proc.stderr.decode("utf-8", "replace"))

    # Step 5: replay-http against a fresh server instance; diff must find
    # nothing meaningful. Unlike stdio replay, replay-http drives a live
    # target directly -- no local reverse proxy in this direction.
    fresh_port = free_port()
    fresh_upstream = subprocess.Popen(
        command, cwd=server_dir, env={**os.environ, "PORT": str(fresh_port)},
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
    )
    try:
        if not wait_for_port("127.0.0.1", fresh_port, fresh_upstream):
            result.record("replay-http against fresh instance", False,
                           "fresh upstream server never opened its port")
        else:
            proc = subprocess.run(
                [mcptracer_bin(), "--db", str(db), "replay-http", baseline_id,
                 "--target", f"http://127.0.0.1:{fresh_port}/mcp", "--i-understand-side-effects"],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30,
            )
            replay_id = extract_session_id(proc.stderr, HTTP_REPLAY_SESSION_ID_RE)
            replay_ok = proc.returncode == 0 and replay_id is not None
            result.record("replay-http against fresh instance", replay_ok,
                           proc.stderr.decode("utf-8", "replace"))
            if replay_ok:
                proc = run_cli(db, "diff", baseline_id, replay_id, "--ignore-latency")
                result.record("diff baseline vs replay (expect no meaningful diff, exit 0)",
                               proc.returncode == 0, proc.stdout.decode("utf-8", "replace"))
    finally:
        stop_process(fresh_upstream)

    # Step 6: the rug-pull check, driven through record-http a second time.
    candidate_id, detail = record_via_http(extra_env={rugpull_env: "1"})
    if result.record("record rug-pulled candidate", candidate_id is not None, detail):
        proc = run_cli(db, "diff", baseline_id, candidate_id, "--ignore-latency")
        result.record("diff baseline vs rug-pulled candidate (expect exit 1)",
                       proc.returncode == 1, proc.stdout.decode("utf-8", "replace"))
        proc = run_cli(db, "assert", candidate_id, "--spec", str(pin_path))
        result.record("assert pin (rug-pulled candidate, expect exit 1)",
                       proc.returncode == 1, proc.stdout.decode("utf-8", "replace"))

    return result


def load_cells(only: str | None, include_optional: bool) -> list[dict]:
    with open(COMPAT_ROOT / "matrix.toml", "rb") as f:
        data = tomllib.load(f)
    cells = data.get("cell", [])
    if only:
        cells = [c for c in cells if c["id"] == only]
    elif not include_optional:
        cells = [c for c in cells if c.get("required", True)]
    return cells


def print_report(results: list[CellResult]) -> bool:
    all_ok = True
    for result in results:
        status = "PASS" if result.passed else "FAIL"
        print(f"\n=== {result.cell_id} [{status}]"
              f"{'' if result.required else ' (not required)'} ===")
        for step in result.steps:
            mark = "ok  " if step.ok else "FAIL"
            print(f"  [{mark}] {step.name}")
            if not step.ok and step.detail:
                for line in step.detail.strip().splitlines()[:10]:
                    print(f"        {line}")
        if not result.passed and result.required:
            all_ok = False
    return all_ok


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--only", help="run a single cell by id")
    parser.add_argument("--all", action="store_true",
                         help="include cells marked required = false")
    args = parser.parse_args()

    if not shutil.which(mcptracer_bin()) and not Path(mcptracer_bin()).exists():
        print(f"error: MCPTRACER_BIN={mcptracer_bin()!r} not found on PATH or as a path", file=sys.stderr)
        return 2

    cells = load_cells(args.only, args.all)
    if not cells:
        print(f"error: no matching cell (--only={args.only!r})", file=sys.stderr)
        return 2

    results = []
    for cell in cells:
        with tempfile.TemporaryDirectory(prefix="mcptracer-compat-") as tmp:
            workdir = Path(tmp)
            if cell["transport"] == "stdio":
                result = run_stdio_cell(cell, workdir)
            elif cell["transport"] == "http":
                result = run_http_cell(cell, workdir)
            else:
                result = CellResult(cell_id=cell["id"], required=cell.get("required", True))
                result.record(f"transport {cell['transport']!r}", False, "unknown transport")
            results.append(result)

    ok = print_report(results)
    print()
    if args.only and len(results) == 1:
        # Day-1 style single-cell smoke check: print the session id the way
        # the "done when" criterion expects, if the record step succeeded.
        first_step = results[0].steps[0] if results[0].steps else None
        if first_step and first_step.ok:
            print(f"{results[0].cell_id}: recorded a session")
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
