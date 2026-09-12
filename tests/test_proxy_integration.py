"""
Runs real messages through mcptracer and verifies passthrough plus recording.
"""

import gzip
import http.client
import json
import os
import re
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from pathlib import Path

from schema_validator import validate as validate_against_schema


def frame(message: dict) -> bytes:
    return json.dumps(message, separators=(",", ":")).encode("utf-8") + b"\n"


def parse_frames(raw: bytes) -> list[dict]:
    return [json.loads(line) for line in raw.splitlines() if line.strip()]


def make_workdir(repo: Path) -> Path:
    root = repo / "target" / "integration-tmp"
    root.mkdir(parents=True, exist_ok=True)
    return Path(tempfile.mkdtemp(dir=root))


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def wait_for_port(port: int, proc: subprocess.Popen[bytes], timeout: float = 30) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            stderr = proc.stderr.read().decode("utf-8", errors="replace") if proc.stderr else ""
            raise AssertionError(f"process exited before opening port {port}: {stderr}")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)
    raise AssertionError(f"process did not open port {port}")


def stop_process(proc: subprocess.Popen[bytes]) -> None:
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)


def http_post(port: int, path: str, payload: dict, headers: dict[str, str]) -> tuple[int, dict[str, str], bytes]:
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        conn.request(
            "POST",
            path,
            body=json.dumps(payload, separators=(",", ":")).encode("utf-8"),
            headers=headers,
        )
        response = conn.getresponse()
        return response.status, {name.lower(): value for name, value in response.getheaders()}, response.read()
    finally:
        conn.close()


def test_proxy_records_session() -> None:
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"

    messages = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "0.1.0"},
            },
        },
        {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}},
        {"jsonrpc": "2.0", "id": "list-1", "method": "tools/list", "params": {}},
        {
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"message": "hello mcptracer"}},
        },
    ]
    stdin = b"".join(frame(message) for message in messages)

    proc = subprocess.run(
        [
            "cargo",
            "run",
            "--quiet",
            "--bin",
            "mcptracer",
            "--",
            "--db",
            str(db_path),
            "record",
            "--client",
            "test-client",
            "--",
            sys.executable,
            str(fake_server),
        ],
        input=stdin,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=repo,
        timeout=60,
        check=False,
    )

    assert proc.returncode == 0, proc.stderr.decode("utf-8", errors="replace")
    responses = parse_frames(proc.stdout)
    assert len(responses) == 3
    assert responses[0]["result"]["serverInfo"]["name"] == "fake-mcp"
    assert responses[1]["result"]["tools"][0]["name"] == "echo"
    assert responses[2]["result"]["content"][0]["text"] == "Echo: hello mcptracer"

    assert db_path.exists()
    conn = sqlite3.connect(db_path)
    sessions = conn.execute("SELECT client,total_messages,dropped_messages FROM sessions").fetchall()
    assert sessions == [("test-client", 7, 0)]

    rows = conn.execute(
        "SELECT direction,message_kind,rpc_id,method,tool_name,is_error "
        "FROM messages ORDER BY seq"
    ).fetchall()
    methods = [row[3] for row in rows]
    assert "initialize" in methods
    assert "notifications/initialized" in methods
    assert "tools/list" in methods
    assert "tools/call" in methods
    assert any(row[4] == "echo" for row in rows)
    assert any(row[2] == '"list-1"' for row in rows)
    conn.close()


def test_sessions_show_calls_json_correlates_exchanges() -> None:
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"

    messages = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "0.1.0"},
            },
        },
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"message": "hello mcptracer"}},
        },
    ]
    stdin = b"".join(frame(message) for message in messages)

    record = subprocess.run(
        [
            "cargo",
            "run",
            "--quiet",
            "--bin",
            "mcptracer",
            "--",
            "--db",
            str(db_path),
            "record",
            "--client",
            "test-client",
            "--",
            sys.executable,
            str(fake_server),
        ],
        input=stdin,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=repo,
        timeout=60,
        check=False,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    (session_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    conn.close()

    show = subprocess.run(
        [
            "cargo",
            "run",
            "--quiet",
            "--bin",
            "mcptracer",
            "--",
            "--db",
            str(db_path),
            "sessions",
            "show",
            session_id,
            "--calls",
            "--json",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=repo,
        timeout=60,
        check=False,
    )
    assert show.returncode == 0, show.stderr.decode("utf-8", errors="replace")

    model = json.loads(show.stdout)
    exchanges = {e["method"]: e for e in model["exchanges"]}

    assert exchanges["initialize"]["status"] == "ok"
    assert exchanges["initialize"]["latency_ns"] is not None

    call = exchanges["tools/call"]
    assert call["tool_name"] == "echo"
    assert call["status"] == "ok"
    assert call["latency_ns"] is not None

    assert model["stats"]["total_exchanges"] == 2
    assert model["stats"]["ok"] == 2


def test_replay_reproduces_methods_and_tools_in_order() -> None:
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"

    messages = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "0.1.0"},
            },
        },
        {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}},
        {"jsonrpc": "2.0", "id": "list-1", "method": "tools/list", "params": {}},
        {
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"message": "hello mcptracer"}},
        },
    ]
    stdin = b"".join(frame(message) for message in messages)

    def mcptracer(*args: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    record = mcptracer(
        "record", "--client", "test-client", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    (source_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    conn.close()

    replay = mcptracer("replay", source_id, "--client", "replay-test", "--", sys.executable, str(fake_server))
    assert replay.returncode == 0, replay.stderr.decode("utf-8", errors="replace")
    # Replay drives the server as the "client" - nothing should reach real stdout.
    assert replay.stdout == b""

    # By default, replay warns that it re-executes recorded calls for real,
    # naming the specific tool. --i-understand-side-effects suppresses it.
    replay_stderr = replay.stderr.decode("utf-8", errors="replace")
    assert "WARNING: replay re-executes" in replay_stderr
    assert "tools/call echo" in replay_stderr

    conn = sqlite3.connect(db_path)
    session_ids = [row[0] for row in conn.execute("SELECT id FROM sessions").fetchall()]
    assert len(session_ids) == 2
    (target_id,) = [sid for sid in session_ids if sid != source_id]

    rows = conn.execute(
        "SELECT seq, direction, message_kind, method, tool_name "
        "FROM messages WHERE session_id = ? ORDER BY seq",
        (target_id,),
    ).fetchall()
    conn.close()

    c2s_methods = [row[3] for row in rows if row[1] == "c2s" and row[3] is not None]
    assert c2s_methods == [
        "initialize",
        "notifications/initialized",
        "tools/list",
        "tools/call",
    ]
    assert any(row[4] == "echo" for row in rows)

    init_seq = next(row[0] for row in rows if row[3] == "initialize")
    call_seq = next(row[0] for row in rows if row[3] == "tools/call")
    assert init_seq < call_seq

    # --i-understand-side-effects suppresses the warning.
    quiet_replay = mcptracer(
        "replay", source_id, "--client", "replay-test-quiet", "--i-understand-side-effects",
        "--", sys.executable, str(fake_server),
    )
    assert quiet_replay.returncode == 0, quiet_replay.stderr.decode("utf-8", errors="replace")
    assert "WARNING: replay re-executes" not in quiet_replay.stderr.decode("utf-8", errors="replace")


def test_serve_replays_recorded_responses_without_the_upstream_server() -> None:
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    source_messages = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "serve-test", "version": "0.1.0"},
            },
        },
        {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}},
        {"jsonrpc": "2.0", "id": "source-list", "method": "tools/list", "params": {}},
        {
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"message": "offline"}},
        },
    ]
    source_stdin = b"".join(frame(message) for message in source_messages)

    def mcptracer(*args: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    record = mcptracer(
        "record", "--client", "serve-source", "--", sys.executable, str(fake_server),
        input_bytes=source_stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")
    conn = sqlite3.connect(db_path)
    (source_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    conn.close()

    live_messages = [
        {**source_messages[0], "id": "fresh-init"},
        source_messages[1],
        {**source_messages[2], "id": 22},
        {**source_messages[3], "id": "fresh-call"},
    ]
    served = mcptracer("serve", source_id, input_bytes=b"".join(frame(message) for message in live_messages))
    assert served.returncode == 0, served.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    source_payloads = [
        row[0]
        for row in conn.execute(
            "SELECT payload FROM messages "
            "WHERE session_id = ? AND direction = 's2c' AND message_kind = 'response' "
            "ORDER BY seq",
            (source_id,),
        )
    ]
    conn.close()
    served_lines = [line for line in served.stdout.splitlines() if line]
    assert len(source_payloads) == len(served_lines) == 3
    for source_payload, served_line, live_id in zip(source_payloads, served_lines, ["fresh-init", 22, "fresh-call"]):
        expected = json.loads(source_payload)
        expected["id"] = live_id
        assert served_line == json.dumps(expected, separators=(",", ":")).encode("utf-8")

    strict = mcptracer(
        "serve",
        source_id,
        "--strict",
        input_bytes=frame({"jsonrpc": "2.0", "id": 99, "method": "unknown/method", "params": {}}),
    )
    assert strict.returncode != 0
    strict_responses = parse_frames(strict.stdout)
    assert strict_responses[0]["error"]["code"] == -32601

    # --match-strategy sequential replays in recorded order regardless of
    # what the live request actually asked for. The source session has three
    # answered client-originated exchanges: initialize, tools/list, tools/call.
    sequential = mcptracer(
        "serve",
        source_id,
        "--match-strategy",
        "sequential",
        input_bytes=b"".join(
            frame(message)
            for message in [
                {"jsonrpc": "2.0", "id": "s1", "method": "anything/goes", "params": {}},
                {"jsonrpc": "2.0", "id": "s2", "method": "still/anything", "params": {}},
                {"jsonrpc": "2.0", "id": "s3", "method": "whatever/else", "params": {}},
            ]
        ),
    )
    assert sequential.returncode == 0, sequential.stderr.decode("utf-8", errors="replace")
    sequential_responses = parse_frames(sequential.stdout)
    assert len(sequential_responses) == 3
    assert sequential_responses[0]["id"] == "s1"
    assert "protocolVersion" in sequential_responses[0]["result"]
    assert sequential_responses[1]["id"] == "s2"
    assert "tools" in sequential_responses[1]["result"]
    assert sequential_responses[2]["id"] == "s3"
    assert sequential_responses[2]["result"]["content"][0]["text"] == "Echo: offline"


def test_proxy_redacts_stored_secrets_but_forwards_them_unchanged() -> None:
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"

    # Two distinct values: "message" is not a sensitive key and must survive
    # redaction verbatim (and keep reaching the server, since forwarding
    # never depends on the recording/redaction path); "api_key" is a
    # sensitive key and must be masked in the stored payload only.
    plain_value = "hello mcptracer"
    secret_value = "sk-super-secret-value"
    messages = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "0.1.0"},
            },
        },
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "echo",
                "arguments": {"message": plain_value, "api_key": secret_value},
            },
        },
    ]
    stdin = b"".join(frame(message) for message in messages)

    proc = subprocess.run(
        [
            "cargo",
            "run",
            "--quiet",
            "--bin",
            "mcptracer",
            "--",
            "--db",
            str(db_path),
            "record",
            "--client",
            "test-client",
            "--redact",
            "default",
            "--",
            sys.executable,
            str(fake_server),
        ],
        input=stdin,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=repo,
        timeout=60,
        check=False,
    )

    assert proc.returncode == 0, proc.stderr.decode("utf-8", errors="replace")

    # The server only received (and can only prove it received) the
    # "message" argument, so a correct echo response confirms the proxy
    # still forwards frames normally with redaction enabled.
    responses = parse_frames(proc.stdout)
    assert responses[1]["result"]["content"][0]["text"] == f"Echo: {plain_value}"

    conn = sqlite3.connect(db_path)
    sessions = conn.execute("SELECT redaction_policy FROM sessions").fetchall()
    assert sessions == [("default",)]

    payloads = [
        row[0]
        for row in conn.execute(
            "SELECT payload FROM messages WHERE tool_name = 'echo' ORDER BY seq"
        ).fetchall()
    ]
    assert payloads, "expected a recorded tools/call message"
    for payload in payloads:
        assert secret_value not in payload, "sensitive-key value leaked into storage"
        assert '"api_key":"***REDACTED***"' in payload
        assert plain_value in payload, "non-sensitive key must survive redaction"
    conn.close()


SCRIPT = [
    {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test-client", "version": "0.1.0"},
        },
    },
    {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}},
    {"jsonrpc": "2.0", "id": "list-1", "method": "tools/list", "params": {}},
    {
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {"name": "echo", "arguments": {"message": "hello mcptracer"}},
    },
]


def test_diff_detects_changes_and_security_findings() -> None:
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    def mcptracer(*args: str, input_bytes: bytes | None = None, env: dict | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
            env=env,
        )

    baseline = mcptracer(
        "record", "--client", "baseline", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert baseline.returncode == 0, baseline.stderr.decode("utf-8", errors="replace")

    changed = mcptracer(
        "record", "--client", "changed", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
        env={**os.environ, "FAKE_MCP_VARIANT": "changed"},
    )
    assert changed.returncode == 0, changed.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    ids = [row[0] for row in conn.execute("SELECT id FROM sessions ORDER BY started_at").fetchall()]
    conn.close()
    baseline_id, changed_id = ids

    # Identical sessions: no differences, exit 0. Latency is ignored because
    # two real recordings always jitter.
    same = mcptracer("diff", baseline_id, baseline_id, "--ignore-latency")
    assert same.returncode == 0, same.stderr.decode("utf-8", errors="replace")
    assert b"Sessions match" in same.stdout

    # Changed server: response text changed AND the tool description changed
    # (the rug-pull signature). Exit 1 plus a SECURITY finding.
    diff = mcptracer("diff", baseline_id, changed_id, "--ignore-latency")
    assert diff.returncode == 1, diff.stdout.decode("utf-8", errors="replace")
    out = diff.stdout.decode("utf-8", errors="replace")
    assert "SECURITY" in out
    assert "description changed" in out
    assert "/content/0/text" in out

    diff_json = mcptracer("diff", baseline_id, changed_id, "--ignore-latency", "--json")
    assert diff_json.returncode == 1
    report = json.loads(diff_json.stdout)
    kinds = [finding["kind"] for finding in report["security"]]
    assert "tool_description_changed" in kinds
    assert any(change["key"] == "tools/call echo#0" for change in report["changed"])


def test_diff_batch_aggregates_pairs_and_gates_on_fail_on_breaking() -> None:
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    def mcptracer(*args: str, input_bytes: bytes | None = None, env: dict | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
            env=env,
        )

    baseline = mcptracer(
        "record", "--client", "batch-baseline", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert baseline.returncode == 0, baseline.stderr.decode("utf-8", errors="replace")

    changed = mcptracer(
        "record", "--client", "batch-changed", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
        env={**os.environ, "FAKE_MCP_VARIANT": "changed"},
    )
    assert changed.returncode == 0, changed.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    ids = [row[0] for row in conn.execute("SELECT id FROM sessions ORDER BY started_at").fetchall()]
    conn.close()
    baseline_id, changed_id = ids

    config = workdir / "diff-batch.toml"
    config.write_text(
        "\n".join(
            [
                "[[pair]]",
                'label = "clean"',
                f'baseline = "{baseline_id}"',
                f'candidate = "{baseline_id}"',
                "",
                "[[pair]]",
                'label = "changed"',
                f'baseline = "{baseline_id}"',
                f'candidate = "{changed_id}"',
            ]
        ),
        encoding="utf-8",
    )

    # Without --fail-on-breaking, diff-batch always reports and exits 0.
    report_only = mcptracer("diff-batch", str(config))
    out = report_only.stdout.decode("utf-8", errors="replace")
    assert report_only.returncode == 0, out + report_only.stderr.decode("utf-8", errors="replace")
    assert "PASS clean" in out
    assert "CHANGED changed" in out
    assert "2 pair(s), 1 changed" in out

    # --fail-on-breaking turns any changed pair into a non-zero exit.
    gated = mcptracer("diff-batch", str(config), "--fail-on-breaking")
    assert gated.returncode != 0

    json_out = mcptracer("diff-batch", str(config), "--json")
    assert json_out.returncode == 0, json_out.stderr.decode("utf-8", errors="replace")
    results = json.loads(json_out.stdout)
    assert len(results) == 2
    assert results[0]["label"] == "clean"
    assert results[0]["changed"] is False
    assert results[1]["label"] == "changed"
    assert results[1]["changed"] is True
    assert results[1]["report"]["security"]


def test_optimize_mines_history_for_suggestions() -> None:
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    def mcptracer(*args: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    empty = mcptracer("optimize")
    assert empty.returncode == 0, empty.stderr.decode("utf-8", errors="replace")
    assert "No recorded sessions" in empty.stdout.decode("utf-8", errors="replace")

    for client in ["opt-1", "opt-2", "opt-3"]:
        record = mcptracer(
            "record", "--client", client, "--redact", "default", "--", sys.executable, str(fake_server),
            input_bytes=stdin,
        )
        assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")

    result = mcptracer("optimize", "--json")
    assert result.returncode == 0, result.stderr.decode("utf-8", errors="replace")
    suggestions = json.loads(result.stdout)
    assert suggestions, "expected at least one suggestion from three recorded sessions"

    kinds = {s["kind"] for s in suggestions}
    assert "latency_threshold" in kinds
    assert "golden_session" in kinds
    assert "bench_params" in kinds
    for suggestion in suggestions:
        assert 0.0 <= suggestion["confidence"] <= 1.0
        assert suggestion["provenance"], suggestion

    unredacted = mcptracer(
        "record", "--client", "opt-unredacted", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert unredacted.returncode == 0, unredacted.stderr.decode("utf-8", errors="replace")

    refused = mcptracer("optimize")
    assert refused.returncode != 0
    assert b"--allow-unredacted" in refused.stderr

    allowed = mcptracer("optimize", "--allow-unredacted", "--json")
    assert allowed.returncode == 0, allowed.stderr.decode("utf-8", errors="replace")


def test_index_rebuild_surfaces_tool_versions_and_supersession() -> None:
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    def mcptracer(*args: str, input_bytes: bytes | None = None, env: dict | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
            env=env,
        )

    # Two sessions against the SAME server command (so they share a
    # server_key), the second with a changed tools/list description - the
    # rug-pull-shaped drift the derived index is meant to catch.
    baseline = mcptracer(
        "record", "--client", "baseline", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert baseline.returncode == 0, baseline.stderr.decode("utf-8", errors="replace")

    changed = mcptracer(
        "record", "--client", "changed", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
        env={**os.environ, "FAKE_MCP_VARIANT": "changed"},
    )
    assert changed.returncode == 0, changed.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    ids = [row[0] for row in conn.execute("SELECT id FROM sessions ORDER BY started_at").fetchall()]
    conn.close()
    assert len(ids) == 2

    rebuild = mcptracer("index", "rebuild")
    out = rebuild.stdout.decode("utf-8", errors="replace")
    assert rebuild.returncode == 0, rebuild.stderr.decode("utf-8", errors="replace")
    assert "Rebuilt derived memory for 2 session(s)." in out
    assert "tool versions: 2" in out
    assert "version_supersedes edges: 1" in out

    conn = sqlite3.connect(db_path)
    versions = conn.execute(
        "SELECT server_key, tool_name, description_hash FROM tool_versions ORDER BY first_seen_at"
    ).fetchall()
    assert len(versions) == 2
    assert versions[0][0] == versions[1][0], "both sessions used the same server command -> same server_key"
    assert versions[0][1] == versions[1][1] == "echo"
    assert versions[0][2] != versions[1][2], "description hash must differ after the drift"

    supersedes = conn.execute(
        "SELECT session_id, from_type, to_type FROM memory_edges WHERE edge_type = 'version_supersedes'"
    ).fetchall()
    assert len(supersedes) == 1
    assert supersedes[0][0] is None, "supersession edges are cross-session (session_id NULL)"
    assert supersedes[0][1] == supersedes[0][2] == "tool_version"

    observations = conn.execute("SELECT COUNT(*) FROM tool_version_observations").fetchone()[0]
    assert observations == 2
    conn.close()

    # Rebuilding again must not duplicate anything (idempotent).
    rebuild_again = mcptracer("index", "rebuild")
    assert rebuild_again.returncode == 0
    out_again = rebuild_again.stdout.decode("utf-8", errors="replace")
    assert "tool versions: 2" in out_again
    assert "version_supersedes edges: 1" in out_again

    conn = sqlite3.connect(db_path)
    version_count = conn.execute("SELECT COUNT(*) FROM tool_versions").fetchone()[0]
    edge_count = conn.execute(
        "SELECT COUNT(*) FROM memory_edges WHERE edge_type = 'version_supersedes'"
    ).fetchone()[0]
    conn.close()
    assert version_count == 2, "rebuild must not duplicate tool versions"
    assert edge_count == 1, "rebuild must not duplicate supersession edges"

    # index facts --json is valid, non-empty, and carries no secret payload text.
    facts_json = mcptracer("index", "facts", "--json")
    assert facts_json.returncode == 0, facts_json.stderr.decode("utf-8", errors="replace")
    facts = json.loads(facts_json.stdout)
    assert len(facts) > 0
    fact_types = {f["fact_type"] for f in facts}
    for expected in ("session_observed_server", "session_called_tool", "tool_has_version"):
        assert expected in fact_types
    blob = json.dumps(facts)
    assert "hello mcptracer" not in blob, "raw payload text must not leak into derived facts"

    # --session narrows to one session's facts only.
    scoped = mcptracer("index", "facts", "--session", ids[0], "--json")
    assert scoped.returncode == 0
    scoped_facts = json.loads(scoped.stdout)
    assert scoped_facts, "expected facts for the baseline session"
    assert all(f["session_id"] == ids[0] for f in scoped_facts)

    # route surfaces the security route for a session that used a tool
    # version now superseded by a later, drifted one, and cites a real
    # sibling session for the suggested diff.
    routed = mcptracer("route", ids[0], "--json")
    assert routed.returncode == 0, routed.stderr.decode("utf-8", errors="replace")
    recommendations = json.loads(routed.stdout)
    security = next(r for r in recommendations if r["route"] == "security")
    assert "echo" in security["reason"]
    assert any(ids[1] in command for command in security["commands"])

    # graph exports both sessions, the echo tool, both tool versions, and a
    # supersedes edge, as JSONL.
    graphed = mcptracer("graph")
    assert graphed.returncode == 0, graphed.stderr.decode("utf-8", errors="replace")
    records = [json.loads(line) for line in graphed.stdout.decode("utf-8").splitlines() if line]
    node_types = {r["type"] for r in records if r["type"] in ("session", "tool", "tool_version")}
    assert node_types == {"session", "tool", "tool_version"}
    session_node_ids = {r["id"] for r in records if r["type"] == "session"}
    assert session_node_ids == {f"session:{ids[0]}", f"session:{ids[1]}"}
    assert sum(1 for r in records if r["type"] == "tool_version") == 2
    assert any(r["type"] == "calls" for r in records)
    assert any(r["type"] == "has_version" for r in records)
    supersedes = [r for r in records if r["type"] == "supersedes"]
    assert len(supersedes) == 1

    # --tool narrows the graph; DOT renders the supersedes edge in red.
    scoped_graph = mcptracer("graph", "--tool", "echo", "--format", "dot")
    assert scoped_graph.returncode == 0, scoped_graph.stderr.decode("utf-8", errors="replace")
    dot = scoped_graph.stdout.decode("utf-8")
    assert dot.startswith("digraph mcptracer {")
    assert 'label="supersedes", color=red' in dot

    missing_tool_graph = mcptracer("graph", "--tool", "nonexistent-tool")
    assert missing_tool_graph.returncode == 0
    assert missing_tool_graph.stdout.decode("utf-8").strip() == ""


def test_semantic_search_finds_drift_and_gates_on_redaction() -> None:
    """`semantic` is feature-gated (off by default); this test builds and
    runs the binary with --features semantic-search explicitly, separate
    from every other test's default-feature binary."""
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    def mcptracer(*args: str, input_bytes: bytes | None = None, env: dict | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            [
                "cargo", "run", "--quiet", "-p", "mcptracer-proxy", "--bin", "mcptracer",
                "--features", "semantic-search", "--",
                "--db", str(db_path), *args,
            ],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
            env=env,
        )

    baseline = mcptracer(
        "record", "--client", "baseline", "--redact", "default", "--",
        sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert baseline.returncode == 0, baseline.stderr.decode("utf-8", errors="replace")

    changed = mcptracer(
        "record", "--client", "changed", "--redact", "default", "--",
        sys.executable, str(fake_server),
        input_bytes=stdin,
        env={**os.environ, "FAKE_MCP_VARIANT": "changed"},
    )
    assert changed.returncode == 0, changed.stderr.decode("utf-8", errors="replace")

    rebuild = mcptracer("index", "rebuild")
    assert rebuild.returncode == 0, rebuild.stderr.decode("utf-8", errors="replace")

    result = mcptracer("semantic", "rug pull", "--json")
    assert result.returncode == 0, result.stderr.decode("utf-8", errors="replace")
    hits = json.loads(result.stdout)
    assert hits, "expected at least one hit for 'rug pull'"
    assert any(hit["document"]["kind"] == "tool_version" and "echo" in hit["document"]["id"] for hit in hits)
    assert all(hit["provider"] == "local-lexical-v1" for hit in hits)

    unrelated = mcptracer("semantic", "banana smoothie recipe", "--json")
    assert unrelated.returncode == 0, unrelated.stderr.decode("utf-8", errors="replace")
    unrelated_hits = json.loads(unrelated.stdout)
    assert not any("echo" in hit["document"]["id"] for hit in unrelated_hits)

    # A session recorded without redaction refuses semantic indexing.
    plain_db = make_workdir(repo) / "plain.db"

    def mcptracer_plain(*args: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            [
                "cargo", "run", "--quiet", "-p", "mcptracer-proxy", "--bin", "mcptracer",
                "--features", "semantic-search", "--",
                "--db", str(plain_db), *args,
            ],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    unredacted_record = mcptracer_plain(
        "record", "--client", "plain", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert unredacted_record.returncode == 0, unredacted_record.stderr.decode("utf-8", errors="replace")

    refused = mcptracer_plain("semantic", "anything")
    assert refused.returncode != 0
    assert b"--allow-unredacted" in refused.stderr

    overridden = mcptracer_plain("semantic", "anything", "--allow-unredacted")
    assert overridden.returncode == 0, overridden.stderr.decode("utf-8", errors="replace")


def test_assert_rule_and_snapshot_modes() -> None:
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    def mcptracer(*args: str, input_bytes: bytes | None = None, env: dict | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
            env=env,
        )

    record = mcptracer(
        "record", "--client", "assert-test", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")

    changed = mcptracer(
        "record", "--client", "assert-changed", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
        env={**os.environ, "FAKE_MCP_VARIANT": "changed"},
    )
    assert changed.returncode == 0, changed.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    ids = [row[0] for row in conn.execute("SELECT id FROM sessions ORDER BY started_at").fetchall()]
    conn.close()
    session_id, changed_id = ids

    passing_spec = workdir / "checks.toml"
    passing_spec.write_text(
        "\n".join(
            [
                '[[assert]]',
                'kind = "no_errors"',
                "",
                '[[assert]]',
                'kind = "tool_called"',
                'tool = "echo"',
                "min = 1",
                "max = 1",
                "",
                '[[assert]]',
                'kind = "call_order"',
                'before = "initialize"',
                'after = "tools/call"',
                "",
                '[[assert]]',
                'kind = "latency"',
                "p95_under_ms = 30000.0",
                "",
                '[[assert]]',
                'kind = "response_matches"',
                'tool = "echo"',
                'pointer = "/result/isError"',
                "equals = false",
            ]
        ),
        encoding="utf-8",
    )
    ok = mcptracer("assert", session_id, "--spec", str(passing_spec))
    out = ok.stdout.decode("utf-8", errors="replace")
    assert ok.returncode == 0, out + ok.stderr.decode("utf-8", errors="replace")
    assert "5 passed, 0 failed" in out

    failing_spec = workdir / "failing.toml"
    failing_spec.write_text(
        '[[assert]]\nkind = "tool_called"\ntool = "nonexistent"\n', encoding="utf-8"
    )
    fail = mcptracer("assert", session_id, "--spec", str(failing_spec))
    assert fail.returncode == 1, fail.stdout.decode("utf-8", errors="replace")
    assert b"FAIL tool_called nonexistent" in fail.stdout

    malformed_spec = workdir / "malformed.toml"
    malformed_spec.write_text('[[assert]]\nkind = "bogus_kind"\n', encoding="utf-8")
    broken = mcptracer("assert", session_id, "--spec", str(malformed_spec))
    assert broken.returncode == 2, broken.stderr.decode("utf-8", errors="replace")
    assert b"spec error" in broken.stderr

    # Snapshot mode: a session always matches itself; the changed server fails.
    snap_ok = mcptracer("assert", session_id, "--golden", session_id)
    assert snap_ok.returncode == 0, snap_ok.stdout.decode("utf-8", errors="replace")
    snap_fail = mcptracer("assert", changed_id, "--golden", session_id)
    assert snap_fail.returncode == 1, snap_fail.stdout.decode("utf-8", errors="replace")
    assert b"security finding" in snap_fail.stdout

    # tools_pinned: bootstrap against the baseline to discover its hash, pin
    # it, then confirm the drifted "changed" session fails the pin.
    bootstrap_spec = workdir / "bootstrap.toml"
    bootstrap_spec.write_text('[[assert]]\nkind = "tools_pinned"\n', encoding="utf-8")
    bootstrap = mcptracer("assert", session_id, "--spec", str(bootstrap_spec))
    assert bootstrap.returncode == 1, bootstrap.stdout.decode("utf-8", errors="replace")
    match = re.search(r'echo = "([0-9a-f]{64})"', bootstrap.stdout.decode("utf-8"))
    assert match, bootstrap.stdout.decode("utf-8", errors="replace")
    echo_hash = match.group(1)

    pin_spec = workdir / "pin.toml"
    pin_spec.write_text(
        f'[[assert]]\nkind = "tools_pinned"\nhashes = {{ echo = "{echo_hash}" }}\n',
        encoding="utf-8",
    )
    pin_ok = mcptracer("assert", session_id, "--spec", str(pin_spec))
    assert pin_ok.returncode == 0, pin_ok.stdout.decode("utf-8", errors="replace")

    pin_fail = mcptracer("assert", changed_id, "--spec", str(pin_spec))
    assert pin_fail.returncode == 1, pin_fail.stdout.decode("utf-8", errors="replace")
    assert b"hash mismatch" in pin_fail.stdout


def test_assert_manifest_and_offline_verify() -> None:
    # T-73: assert --manifest writes an evidence manifest tying a canonical
    # content digest to the recorded outcome; verify must confirm a real
    # exported artifact matches it and reject one that doesn't, entirely
    # offline (no --db passed to verify at all).
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    def mcptracer(
        *args: str, input_bytes: bytes | None = None, env: dict | None = None
    ) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
            env=env,
        )

    def mcptracer_no_db(*args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", *args],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    record = mcptracer(
        "record", "--client", "manifest-test", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")
    changed = mcptracer(
        "record", "--client", "manifest-test-changed", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
        env={**os.environ, "FAKE_MCP_VARIANT": "changed"},
    )
    assert changed.returncode == 0, changed.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    ids = [row[0] for row in conn.execute("SELECT id FROM sessions ORDER BY started_at").fetchall()]
    conn.close()
    session_id, changed_id = ids

    spec = workdir / "checks.toml"
    spec.write_text('[[assert]]\nkind = "no_errors"\n', encoding="utf-8")
    manifest_path = workdir / "evidence.json"

    result = mcptracer(
        "assert", session_id, "--spec", str(spec),
        "--manifest", str(manifest_path), "--allow-unredacted",
    )
    assert result.returncode == 0, result.stderr.decode("utf-8", errors="replace")
    assert manifest_path.exists()

    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    assert manifest["schema_version"] == 1
    assert manifest["outcome"] == "pass"
    assert manifest["signed"] is False
    assert len(manifest["artifact"]["sha256"]) == 64
    assert "baseline" not in manifest
    assert len(manifest["assertion_spec"]["sha256"]) == 64

    # Export the exact session that was checked, and a different one, to
    # real .mtrace files for offline verification.
    matching_artifact = workdir / "matching.mtrace"
    exported = mcptracer(
        "export", session_id, "--out", str(matching_artifact), "--allow-unredacted",
    )
    assert exported.returncode == 0, exported.stderr.decode("utf-8", errors="replace")

    wrong_artifact = workdir / "wrong.mtrace"
    exported_wrong = mcptracer(
        "export", changed_id, "--out", str(wrong_artifact), "--allow-unredacted",
    )
    assert exported_wrong.returncode == 0, exported_wrong.stderr.decode("utf-8", errors="replace")

    # verify takes no --db at all: purely offline, manifest + local files.
    verified = mcptracer_no_db(
        "verify", str(manifest_path),
        "--artifact", str(matching_artifact),
        "--assert-spec", str(spec),
    )
    verified_out = verified.stdout.decode("utf-8", errors="replace")
    assert verified.returncode == 0, verified_out + verified.stderr.decode("utf-8", errors="replace")
    assert "artifact         MATCH" in verified_out
    assert "assertion_spec   MATCH" in verified_out
    assert "signature: NONE" in verified_out

    tampered = mcptracer_no_db(
        "verify", str(manifest_path),
        "--artifact", str(wrong_artifact),
        "--assert-spec", str(spec),
    )
    tampered_out = tampered.stdout.decode("utf-8", errors="replace")
    assert tampered.returncode == 1, tampered_out
    assert "artifact         MISMATCH" in tampered_out

    # T-73A: verification must be complete by default. Omitting a file for a
    # digest the manifest records (here, --assert-spec) is a hard error, not
    # a silent pass -- a CI script that only checks the exit code must not
    # be fooled into thinking everything was checked when it wasn't.
    incomplete = mcptracer_no_db(
        "verify", str(manifest_path), "--artifact", str(matching_artifact),
    )
    incomplete_out = incomplete.stdout.decode("utf-8", errors="replace")
    incomplete_err = incomplete.stderr.decode("utf-8", errors="replace")
    assert incomplete.returncode != 0, incomplete_out + incomplete_err
    assert "--assert-spec" in incomplete_err
    assert "artifact         MATCH" not in incomplete_out

    # --allow-partial explicitly opts into skipping it instead.
    partial = mcptracer_no_db(
        "verify", str(manifest_path),
        "--artifact", str(matching_artifact),
        "--allow-partial",
    )
    partial_out = partial.stdout.decode("utf-8", errors="replace")
    assert partial.returncode == 0, partial_out + partial.stderr.decode("utf-8", errors="replace")
    assert "artifact         MATCH" in partial_out
    assert "assertion_spec   SKIPPED" in partial_out
    assert "WARNING: partial verification" in partial_out

    # Golden-snapshot mode records a baseline digest instead of an
    # assertion-spec digest.
    golden_manifest_path = workdir / "golden-evidence.json"
    golden_result = mcptracer(
        "assert", session_id, "--golden", session_id,
        "--manifest", str(golden_manifest_path), "--allow-unredacted",
    )
    assert golden_result.returncode == 0, golden_result.stderr.decode("utf-8", errors="replace")
    golden_manifest = json.loads(golden_manifest_path.read_text(encoding="utf-8"))
    assert "assertion_spec" not in golden_manifest
    assert len(golden_manifest["baseline"]["sha256"]) == 64
    assert golden_manifest["baseline"]["sha256"] == golden_manifest["artifact"]["sha256"]


def test_verify_rejects_hostile_manifests() -> None:
    # T-73A: verify must reject a manifest that claims to be signed (and
    # must never print "signature: verified" for one -- this build has no
    # signature-checking code at all), and must reject structurally invalid
    # manifests before ever touching a local file. dummy_artifact is never
    # read in any of these cases: rejection happens at manifest parse time.
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    dummy_artifact = workdir / "does-not-exist.mtrace"

    def verify(manifest_path: Path) -> subprocess.CompletedProcess:
        return subprocess.run(
            [
                "cargo", "run", "--quiet", "--bin", "mcptracer", "--",
                "verify", str(manifest_path), "--artifact", str(dummy_artifact),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    base = {
        "schema_version": 1,
        "mcptracer_version": "0.2.0",
        "generated_at_ns": 1,
        "artifact": {"sha256": "a" * 64},
        "assertion_spec": {"sha256": "b" * 64},
        "outcome": "pass",
        "signed": False,
    }

    signed_true = workdir / "signed-true.json"
    signed_true.write_text(json.dumps({**base, "signed": True}), encoding="utf-8")
    result = verify(signed_true)
    out = result.stdout.decode("utf-8", errors="replace")
    err = result.stderr.decode("utf-8", errors="replace")
    assert result.returncode != 0, out + err
    assert "signed: true" in err
    assert "signature: verified" not in out

    bad_digest = workdir / "bad-digest.json"
    bad_digest.write_text(
        json.dumps({**base, "artifact": {"sha256": "not-a-real-digest"}}), encoding="utf-8"
    )
    result = verify(bad_digest)
    assert result.returncode != 0
    assert "hex SHA-256" in result.stderr.decode("utf-8", errors="replace")

    uppercase_digest = workdir / "uppercase-digest.json"
    uppercase_digest.write_text(
        json.dumps({**base, "artifact": {"sha256": "A" * 64}}), encoding="utf-8"
    )
    result = verify(uppercase_digest)
    assert result.returncode != 0

    both_modes = workdir / "both-modes.json"
    both_modes.write_text(
        json.dumps({**base, "baseline": {"sha256": "c" * 64}}), encoding="utf-8"
    )
    result = verify(both_modes)
    assert result.returncode != 0
    assert "not record both" in result.stderr.decode("utf-8", errors="replace")

    neither_mode = workdir / "neither-mode.json"
    neither = dict(base)
    del neither["assertion_spec"]
    neither_mode.write_text(json.dumps(neither), encoding="utf-8")
    result = verify(neither_mode)
    assert result.returncode != 0
    assert "exactly one" in result.stderr.decode("utf-8", errors="replace")

    wrong_schema = workdir / "wrong-schema.json"
    wrong_schema.write_text(json.dumps({**base, "schema_version": 999}), encoding="utf-8")
    result = verify(wrong_schema)
    assert result.returncode != 0
    assert "unsupported evidence manifest schema version" in result.stderr.decode(
        "utf-8", errors="replace"
    )


def test_baseline_lifecycle_and_ci_resolve_composition() -> None:
    # T-74: a baseline is addressed by (project, scenario, environment), not
    # a raw session id. candidate -> approved -> superseded/revoked, with the
    # digest captured only at promotion. `baseline resolve` prints exactly
    # the session id on stdout and nothing else, so a CI workflow can compose
    # it directly into `assert --golden "$(mcptracer baseline resolve ...)"`
    # without ever hardcoding a transient session id.
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    def mcptracer(
        *args: str, input_bytes: bytes | None = None, env: dict | None = None
    ) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
            env=env,
        )

    record = mcptracer(
        "record", "--client", "baseline-test", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")
    changed = mcptracer(
        "record", "--client", "baseline-test-changed", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
        env={**os.environ, "FAKE_MCP_VARIANT": "changed"},
    )
    assert changed.returncode == 0, changed.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    ids = [row[0] for row in conn.execute("SELECT id FROM sessions ORDER BY started_at").fetchall()]
    conn.close()
    session_id, changed_id = ids

    project, scenario, environment = "demo-proj", "smoke", "ci"

    # Resolving before any promotion fails clearly -- a candidate alone is
    # not usable as a baseline.
    too_early = mcptracer("baseline", "resolve", project, scenario, environment)
    assert too_early.returncode != 0
    assert "no approved baseline" in too_early.stderr.decode("utf-8", errors="replace")

    candidate = mcptracer("baseline", "candidate", project, scenario, environment, session_id)
    assert candidate.returncode == 0, candidate.stderr.decode("utf-8", errors="replace")

    promote = mcptracer(
        "baseline", "promote", project, scenario, environment, session_id,
        "--by", "alice", "--reason", "initial baseline", "--allow-unredacted",
    )
    assert promote.returncode == 0, promote.stderr.decode("utf-8", errors="replace")
    assert "digest:" in promote.stdout.decode("utf-8", errors="replace")

    resolved = mcptracer("baseline", "resolve", project, scenario, environment)
    assert resolved.returncode == 0, resolved.stderr.decode("utf-8", errors="replace")
    resolved_id = resolved.stdout.decode("utf-8").strip()
    assert resolved_id == session_id

    resolved_json = mcptracer("baseline", "resolve", project, scenario, environment, "--json")
    assert resolved_json.returncode == 0
    resolved_record = json.loads(resolved_json.stdout)
    assert resolved_record["state"] == "approved"
    assert resolved_record["session_id"] == session_id
    assert resolved_record["promoted_by"] == "alice"
    assert resolved_record["promotion_reason"] == "initial baseline"
    assert len(resolved_record["digest"]) == 64

    # The exact CI composition pattern this feature exists for: resolve,
    # then feed the result into assert --golden without ever hardcoding a
    # transient session id.
    matches_itself = mcptracer("assert", session_id, "--golden", resolved_id)
    assert matches_itself.returncode == 0, matches_itself.stdout.decode("utf-8", errors="replace")

    differs = mcptracer("assert", changed_id, "--golden", resolved_id)
    assert differs.returncode == 1, differs.stdout.decode("utf-8", errors="replace")

    # Promoting a new candidate for the same triple supersedes the old one.
    new_candidate = mcptracer(
        "baseline", "candidate", project, scenario, environment, changed_id
    )
    assert new_candidate.returncode == 0, new_candidate.stderr.decode("utf-8", errors="replace")
    new_promote = mcptracer(
        "baseline", "promote", project, scenario, environment, changed_id,
        "--by", "bob", "--reason", "updated baseline", "--allow-unredacted",
    )
    assert new_promote.returncode == 0, new_promote.stderr.decode("utf-8", errors="replace")

    now_resolved = mcptracer("baseline", "resolve", project, scenario, environment)
    assert now_resolved.stdout.decode("utf-8").strip() == changed_id

    listing = mcptracer("baseline", "list", "--project", project, "--json")
    assert listing.returncode == 0, listing.stderr.decode("utf-8", errors="replace")
    rows = {row["session_id"]: row["state"] for row in json.loads(listing.stdout)}
    assert rows[session_id] == "superseded"
    assert rows[changed_id] == "approved"

    revoke = mcptracer(
        "baseline", "revoke", project, scenario, environment, "--reason", "regression found"
    )
    assert revoke.returncode == 0, revoke.stderr.decode("utf-8", errors="replace")

    after_revoke = mcptracer("baseline", "resolve", project, scenario, environment)
    assert after_revoke.returncode != 0
    assert "no approved baseline" in after_revoke.stderr.decode("utf-8", errors="replace")


def test_json_output_conforms_to_published_schemas() -> None:
    # T-74 (deferred half): the golden test the accept criterion asks for --
    # "schema golden tests reject breaking output drift". Runs the real CLI
    # for every --json-emitting command this project publishes a schema for
    # and validates the actual output against the published schemas/*.json
    # file, so an accidental shape change fails a test instead of silently
    # shipping to whoever depends on these schemas.
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    def mcptracer(*args: str, input_bytes: bytes | None = None, env: dict | None = None):
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
            env=env,
        )

    def schema(name: str) -> dict:
        with open(repo / "schemas" / name, encoding="utf-8") as handle:
            return json.load(handle)

    record = mcptracer(
        "record", "--client", "schema-test", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")
    changed = mcptracer(
        "record", "--client", "schema-test-changed", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
        env={**os.environ, "FAKE_MCP_VARIANT": "changed"},
    )
    assert changed.returncode == 0, changed.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    ids = [row[0] for row in conn.execute("SELECT id FROM sessions ORDER BY started_at").fetchall()]
    conn.close()
    session_id, changed_id = ids

    validate_result = mcptracer("validate", session_id, "--json")
    assert validate_result.returncode == 0, validate_result.stderr.decode("utf-8", errors="replace")
    validate_against_schema(
        json.loads(validate_result.stdout), schema("session-integrity-report.v2.schema.json")
    )

    diff_result = mcptracer("diff", session_id, changed_id, "--json")
    assert diff_result.returncode in (0, 1), diff_result.stderr.decode("utf-8", errors="replace")
    diff_schema = schema("diff-report.v3.schema.json")
    validate_against_schema(json.loads(diff_result.stdout), diff_schema)

    spec = workdir / "checks.toml"
    spec.write_text('[[assert]]\nkind = "no_errors"\n', encoding="utf-8")
    assert_spec_result = mcptracer("assert", session_id, "--spec", str(spec), "--json")
    assert assert_spec_result.returncode == 0, assert_spec_result.stderr.decode("utf-8", errors="replace")
    validate_against_schema(
        json.loads(assert_spec_result.stdout), schema("assert-results.v2.schema.json")
    )

    # assert --golden --json emits the same DiffReport shape as diff --json,
    # not assert-results.v1 -- both are checked against the same schema.
    assert_golden_result = mcptracer("assert", changed_id, "--golden", session_id, "--json")
    assert assert_golden_result.returncode in (0, 1), assert_golden_result.stderr.decode(
        "utf-8", errors="replace"
    )
    validate_against_schema(json.loads(assert_golden_result.stdout), diff_schema)

    eval_spec = workdir / "eval.toml"
    eval_spec.write_text('[[expect]]\ntool = "echo"\n', encoding="utf-8")
    eval_result = mcptracer("eval", session_id, "--spec", str(eval_spec), "--json")
    assert eval_result.returncode == 0, eval_result.stderr.decode("utf-8", errors="replace")
    validate_against_schema(json.loads(eval_result.stdout), schema("eval-report.v2.schema.json"))

    bench_result = mcptracer(
        "bench", session_id, "--repeat", "1", "--concurrency", "1",
        "--i-understand-side-effects", "--json", "--", sys.executable, str(fake_server),
    )
    assert bench_result.returncode == 0, bench_result.stderr.decode("utf-8", errors="replace")
    validate_against_schema(json.loads(bench_result.stdout), schema("bench-report.v2.schema.json"))

    quota_result = mcptracer(
        "quota", session_id, "--rate", "1000000", "--capacity", "1000000", "--json",
    )
    assert quota_result.returncode == 0, quota_result.stderr.decode("utf-8", errors="replace")
    quota_payload = json.loads(quota_result.stdout)
    validate_against_schema(quota_payload, schema("quota-report.v2.schema.json"))
    assert quota_payload["schema_version"] == 2
    assert quota_payload["kind"] == "single_session"
    assert quota_payload["result"]["token_sources"]["reported_events"] == 0
    assert quota_payload["result"]["token_sources"]["estimated_events"] > 0

    # Exercise the new variants using this test's disposable recording.
    with sqlite3.connect(db_path) as conn:
        rows = conn.execute("SELECT seq, payload FROM messages WHERE session_id = ?", (changed_id,)).fetchall()
        changed_requests = changed_results = 0
        for seq, raw in rows:
            payload = json.loads(raw)
            if payload.get("method") == "tools/call":
                payload["params"]["arguments"] = {"text": "different input"}
                changed_requests += 1
            elif isinstance(payload.get("result"), dict) and "isError" in payload["result"]:
                payload["result"]["isError"] = True
                changed_results += 1
            else:
                continue
            encoded = json.dumps(payload)
            conn.execute("UPDATE messages SET payload = ?, payload_bytes = ? WHERE session_id = ? AND seq = ?",
                         (encoded, len(encoded.encode("utf-8")), changed_id, seq))
    assert changed_requests == changed_results == 1
    changed_result = mcptracer("diff", session_id, changed_id, "--json")
    assert changed_result.returncode == 1, changed_result.stderr.decode("utf-8", errors="replace")
    changed_payload = json.loads(changed_result.stdout)
    validate_against_schema(changed_payload, diff_schema)
    deltas = [delta for change in changed_payload["changed"] for delta in change["deltas"]]
    assert any(delta["kind"] == "request_changed" for delta in deltas)
    assert any(delta["kind"] == "status_changed" and delta["to"] == "tool_error" for delta in deltas)


def test_junit_sarif_and_github_annotation_adapters() -> None:
    # T-74 (deferred half): assert --junit/--github and diff --sarif render
    # real CLI results into the three CI formats the accept criterion names.
    # Checked against the real files/stdout the CLI produces, not just the
    # Rust unit tests for the rendering functions in isolation.
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    def mcptracer(*args: str, input_bytes: bytes | None = None, env: dict | None = None):
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
            env=env,
        )

    record = mcptracer(
        "record", "--client", "ci-format-test", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")
    changed = mcptracer(
        "record", "--client", "ci-format-test-changed", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
        env={**os.environ, "FAKE_MCP_VARIANT": "changed"},
    )
    assert changed.returncode == 0, changed.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    ids = [row[0] for row in conn.execute("SELECT id FROM sessions ORDER BY started_at").fetchall()]
    conn.close()
    session_id, changed_id = ids

    # --- assert --spec --junit / --github: one passing, one failing rule ---
    spec = workdir / "checks.toml"
    spec.write_text(
        '[[assert]]\nkind = "no_errors"\n\n[[assert]]\nkind = "tool_called"\ntool = "nonexistent"\n',
        encoding="utf-8",
    )
    junit_path = workdir / "assert.junit.xml"
    result = mcptracer(
        "assert", session_id, "--spec", str(spec), "--junit", str(junit_path), "--github",
    )
    assert result.returncode == 1, result.stdout.decode("utf-8", errors="replace")
    assert junit_path.exists()

    root = ET.fromstring(junit_path.read_text(encoding="utf-8"))
    assert root.tag == "testsuite"
    assert root.attrib["tests"] == "2"
    assert root.attrib["failures"] == "1"
    testcases = root.findall("testcase")
    assert len(testcases) == 2
    failing_cases = [tc for tc in testcases if tc.find("failure") is not None]
    assert len(failing_cases) == 1

    stdout = result.stdout.decode("utf-8", errors="replace")
    assert "::notice::PASS" in stdout
    assert "::error::FAIL" in stdout

    # --- assert --golden --junit: single test case ---
    golden_junit_path = workdir / "golden.junit.xml"
    golden_result = mcptracer(
        "assert", changed_id, "--golden", session_id, "--junit", str(golden_junit_path),
    )
    assert golden_result.returncode == 1, golden_result.stdout.decode("utf-8", errors="replace")
    golden_root = ET.fromstring(golden_junit_path.read_text(encoding="utf-8"))
    assert golden_root.attrib["tests"] == "1"
    assert golden_root.attrib["failures"] == "1"

    # --- diff --sarif: real security findings from the "changed" variant ---
    sarif_path = workdir / "diff.sarif.json"
    diff_result = mcptracer(
        "diff", session_id, changed_id, "--sarif", str(sarif_path), "--exit-zero",
    )
    assert diff_result.returncode == 0, diff_result.stderr.decode("utf-8", errors="replace")
    assert sarif_path.exists()

    sarif = json.loads(sarif_path.read_text(encoding="utf-8"))
    assert sarif["version"] == "2.1.0"
    results = sarif["runs"][0]["results"]
    assert len(results) >= 1
    assert all("ruleId" in r and "message" in r for r in results)
    assert any(r["ruleId"] == "tool_description_changed" for r in results)

    # A diff with no security drift produces an empty SARIF results array,
    # not an absent one.
    empty_sarif_path = workdir / "empty.sarif.json"
    identical_diff = mcptracer(
        "diff", session_id, session_id, "--sarif", str(empty_sarif_path), "--exit-zero",
    )
    assert identical_diff.returncode == 0, identical_diff.stderr.decode("utf-8", errors="replace")
    empty_sarif = json.loads(empty_sarif_path.read_text(encoding="utf-8"))
    assert empty_sarif["runs"][0]["results"] == []

    # --- diff --github-check-json (T-83): payload only, no live GitHub call ---
    check_path = workdir / "check.json"
    check_result = mcptracer(
        "diff", session_id, changed_id,
        "--github-check-json", str(check_path),
        "--github-check-sha", "deadbeef123",
        "--github-check-details-url", "https://registry.example.test/artifacts/xyz",
        "--exit-zero",
    )
    assert check_result.returncode == 0, check_result.stderr.decode("utf-8", errors="replace")
    check_payload = json.loads(check_path.read_text(encoding="utf-8"))
    assert check_payload["head_sha"] == "deadbeef123"
    assert check_payload["conclusion"] == "failure"
    assert check_payload["details_url"] == "https://registry.example.test/artifacts/xyz"
    assert "tool_description_changed" in check_payload["output"]["summary"]
    # Ordinary response content must never leak into the check summary -
    # only security-finding detail (tool contract metadata) belongs there.
    assert "Echo2:" not in check_payload["output"]["summary"]

    # --github-check-json without --github-check-sha is a clap usage error,
    # not a silent no-op.
    missing_sha = mcptracer(
        "diff", session_id, changed_id,
        "--github-check-json", str(workdir / "unused.json"),
        "--exit-zero",
    )
    assert missing_sha.returncode == 2
    assert b"--github-check-sha" in missing_sha.stderr


def test_eval_scores_expected_and_forbidden_tool_calls() -> None:
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    def mcptracer(*args: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    record = mcptracer(
        "record", "--client", "eval-test", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")
    conn = sqlite3.connect(db_path)
    (session_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    conn.close()

    # SCRIPT calls echo with {"message": "hello mcptracer"} and nothing else.
    passing_spec = workdir / "eval-pass.toml"
    passing_spec.write_text(
        "\n".join(
            [
                "[[expect]]",
                'tool = "echo"',
                "required_arguments = { message = \"hello mcptracer\" }",
                "",
                "[[forbidden]]",
                'tool = "delete_everything"',
            ]
        ),
        encoding="utf-8",
    )
    ok = mcptracer("eval", session_id, "--spec", str(passing_spec), "--json")
    assert ok.returncode == 0, ok.stderr.decode("utf-8", errors="replace")
    report = json.loads(ok.stdout)
    assert report["accuracy"] == 1.0
    assert all(r["satisfied"] for r in report["results"])

    # A wrong-tool expectation scores below 1.0 with a per-expectation
    # breakdown identifying exactly which check failed.
    failing_spec = workdir / "eval-fail.toml"
    failing_spec.write_text(
        '[[expect]]\ntool = "echo"\n\n[[expect]]\ntool = "nonexistent_tool"\n',
        encoding="utf-8",
    )
    partial = mcptracer("eval", session_id, "--spec", str(failing_spec), "--json")
    assert partial.returncode == 0, partial.stderr.decode("utf-8", errors="replace")
    partial_report = json.loads(partial.stdout)
    assert partial_report["accuracy"] == 0.5
    assert partial_report["results"][0]["satisfied"] is True
    assert partial_report["results"][1]["satisfied"] is False
    assert "nonexistent_tool" in partial_report["results"][1]["reason"]

    # A malformed spec is a distinct exit-2 spec error, like assert's.
    broken_spec = workdir / "eval-broken.toml"
    broken_spec.write_text("not toml at [[", encoding="utf-8")
    broken = mcptracer("eval", session_id, "--spec", str(broken_spec))
    assert broken.returncode == 2, broken.stderr.decode("utf-8", errors="replace")
    assert b"spec error" in broken.stderr


def test_validate_and_gates_reject_incomplete_sessions() -> None:
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    def mcptracer(*args: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    record = mcptracer(
        "record", "--client", "validate-test", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    (session_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    conn.close()

    healthy = mcptracer("validate", session_id, "--json")
    assert healthy.returncode == 0, healthy.stderr.decode("utf-8", errors="replace")
    assert json.loads(healthy.stdout) == {
        "schema_version": 2,
        "session_id": session_id,
        "healthy": True,
        "issues": [],
    }

    # Simulate a recording backpressure loss. A session with an accounting gap
    # must be visible as unhealthy and cannot become a passing gate.
    conn = sqlite3.connect(db_path)
    conn.execute("UPDATE sessions SET dropped_messages = 1 WHERE id = ?", (session_id,))
    conn.commit()
    conn.close()

    invalid = mcptracer("validate", session_id, "--json")
    assert invalid.returncode == 1
    invalid_report = json.loads(invalid.stdout)
    assert invalid_report["healthy"] is False
    assert invalid_report["issues"][0]["kind"] == "dropped_messages"

    diff = mcptracer("diff", session_id, session_id, "--exit-zero")
    assert diff.returncode != 0
    assert b"unhealthy" in diff.stderr

    spec = workdir / "checks.toml"
    spec.write_text('[[assert]]\nkind = "no_errors"\n', encoding="utf-8")
    assertion = mcptracer("assert", session_id, "--spec", str(spec))
    assert assertion.returncode != 0
    assert b"unhealthy" in assertion.stderr


def test_merge_combines_and_deduplicates_sessions() -> None:
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    messages = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "merge-test", "version": "0.1.0"},
            },
        },
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"message": "same"}},
        },
    ]
    stdin = b"".join(frame(message) for message in messages)

    def mcptracer(*args: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    for client in ["merge-one", "merge-two"]:
        record = mcptracer(
            "record", "--client", client, "--", sys.executable, str(fake_server),
            input_bytes=stdin,
        )
        assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    source_ids = [row[0] for row in conn.execute("SELECT id FROM sessions ORDER BY rowid")]
    conn.close()
    assert len(source_ids) == 2

    merged = mcptracer("merge", *source_ids, "--json")
    assert merged.returncode == 0, merged.stderr.decode("utf-8", errors="replace")
    merged_output = json.loads(merged.stdout)
    assert merged_output["source_session_ids"] == source_ids
    assert merged_output["total_messages"] == 8
    assert merged_output["deduplicated_calls"] == 0

    conn = sqlite3.connect(db_path)
    merged_rows = conn.execute(
        "SELECT seq FROM messages WHERE session_id = ? ORDER BY seq",
        (merged_output["session_id"],),
    ).fetchall()
    source_counts = conn.execute(
        "SELECT total_messages FROM sessions WHERE id IN (?, ?) ORDER BY rowid",
        source_ids,
    ).fetchall()
    conn.close()
    assert merged_rows == [(0,), (1,), (2,), (3,), (4,), (5,), (6,), (7,)]
    assert source_counts == [(4,), (4,)]

    deduplicated = mcptracer("merge", *source_ids, "--deduplicate", "--json")
    assert deduplicated.returncode == 0, deduplicated.stderr.decode("utf-8", errors="replace")
    deduplicated_output = json.loads(deduplicated.stdout)
    assert deduplicated_output["total_messages"] == 4
    assert deduplicated_output["deduplicated_calls"] == 2

    conn = sqlite3.connect(db_path)
    rows = conn.execute(
        "SELECT seq,message_kind FROM messages WHERE session_id = ? ORDER BY seq",
        (deduplicated_output["session_id"],),
    ).fetchall()
    conn.close()
    assert [row[0] for row in rows] == [0, 1, 2, 3]
    assert [row[1] for row in rows].count("request") == 2
    assert [row[1] for row in rows].count("response") == 2

    healthy = mcptracer("validate", deduplicated_output["session_id"], "--json")
    assert healthy.returncode == 0, healthy.stderr.decode("utf-8", errors="replace")
    assert json.loads(healthy.stdout)["healthy"] is True


def test_mtrace_export_import_round_trip() -> None:
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    source_db = workdir / "source.db"
    imported_db = workdir / "imported.db"
    artifact = workdir / "session.mtrace"
    reexported_artifact = workdir / "session-reexported.mtrace"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    secret = "do-not-export-this-secret"
    messages = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "mtrace-test", "version": "0.1.0"},
            },
        },
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "echo",
                "arguments": {"message": "portable", "api_key": secret},
            },
        },
    ]
    stdin = b"".join(frame(message) for message in messages)

    def mcptracer(
        db_path: Path, *args: str, input_bytes: bytes | None = None
    ) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    record = mcptracer(
        source_db,
        "record",
        "--client",
        "mtrace-test",
        "--redact",
        "default",
        "--",
        sys.executable,
        str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(source_db)
    (source_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    conn.close()

    exported = mcptracer(source_db, "export", source_id, "--out", str(artifact))
    assert exported.returncode == 0, exported.stderr.decode("utf-8", errors="replace")
    assert artifact.exists()
    with gzip.open(artifact, "rt", encoding="utf-8") as artifact_file:
        document = json.load(artifact_file)
    assert document["format"] == "mtrace"
    assert document["version"] == 1
    assert document["session"]["redaction_policy"] == "default"
    assert document["session"]["redaction_keys"] == []
    assert document["session"]["dropped_messages"] == 0
    assert secret not in json.dumps(document)
    tool_call = next(message for message in document["messages"] if message["method"] == "tools/call")
    assert tool_call["payload"]["params"]["arguments"]["api_key"] == "***REDACTED***"

    imported = mcptracer(imported_db, "import", str(artifact), "--strict")
    assert imported.returncode == 0, imported.stderr.decode("utf-8", errors="replace")
    conn = sqlite3.connect(imported_db)
    (imported_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    payloads = [row[0] for row in conn.execute("SELECT payload FROM messages").fetchall()]
    conn.close()
    assert all(secret not in payload for payload in payloads)

    reexported = mcptracer(imported_db, "export", imported_id, "--out", str(reexported_artifact))
    assert reexported.returncode == 0, reexported.stderr.decode("utf-8", errors="replace")
    with gzip.open(reexported_artifact, "rt", encoding="utf-8") as artifact_file:
        reexported_document = json.load(artifact_file)
    reexported_document["exported_at_ns"] = document["exported_at_ns"]
    assert reexported_document == document


def test_export_blocks_on_sensitive_content_lint_finding() -> None:
    # T-63: key-name redaction can't catch a secret sitting in free text
    # under an innocuous key like "text" -- e.g. a PEM block embedded in a
    # tool's echoed response. The pre-export lint must catch it regardless of
    # --redact, block export by default without echoing the value, and only
    # proceed with --allow-sensitive-content.
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"
    artifact = workdir / "session.mtrace"
    fake_server = repo / "tests" / "fake_mcp_server.py"

    pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAK\n-----END RSA PRIVATE KEY-----"
    messages = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"message": pem}},
        },
    ]
    stdin = b"".join(frame(message) for message in messages)

    def mcptracer(*args: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    record = mcptracer(
        "record", "--client", "lint-test", "--redact", "default", "--",
        sys.executable, str(fake_server), input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    (source_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    conn.close()

    blocked = mcptracer("export", source_id, "--out", str(artifact))
    assert blocked.returncode != 0
    blocked_stderr = blocked.stderr.decode("utf-8", errors="replace")
    assert "sensitive content detected" in blocked_stderr
    assert "pem_block" in blocked_stderr
    assert pem not in blocked_stderr
    assert not artifact.exists()

    allowed = mcptracer("export", source_id, "--out", str(artifact), "--allow-sensitive-content")
    assert allowed.returncode == 0, allowed.stderr.decode("utf-8", errors="replace")
    allowed_stderr = allowed.stderr.decode("utf-8", errors="replace")
    assert "pem_block" in allowed_stderr
    assert pem not in allowed_stderr
    assert artifact.exists()


def test_mtrace_import_rejects_derived_fields_that_disagree_with_the_payload() -> None:
    # T-72: a crafted .mtrace artifact must not be able to lie about a
    # message's derived fields relative to what its own payload actually
    # says -- e.g. claiming tool_name "echo" for a payload that actually
    # calls "delete_all". Import must reject the whole session atomically,
    # not write a partial one.
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"
    artifact = workdir / "malicious.mtrace"

    document = {
        "format": "mtrace",
        "version": 1,
        "exported_at_ns": 1,
        "exporter": "test",
        "session": {
            "client": "attacker",
            "server_command": "server",
            "transport": "stdio",
            "started_at_ns": 0,
            "ended_at_ns": 1,
            "redaction_policy": "none",
            "redaction_keys": [],
            "dropped_messages": 0,
            "tags": [],
        },
        "messages": [
            {
                "seq": 0,
                "ts_ns": 0,
                "direction": "c2s",
                "message_kind": "request",
                "rpc_id": "1",
                "method": "tools/call",
                "tool_name": "echo",
                "payload": {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {"name": "delete_all"},
                },
                "payload_bytes": 64,
                "is_error": False,
                "error_code": None,
            }
        ],
    }
    artifact.write_bytes(gzip.compress(json.dumps(document).encode("utf-8")))

    proc = subprocess.run(
        ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), "import", str(artifact)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=repo,
        timeout=60,
        check=False,
    )
    assert proc.returncode != 0
    stderr = proc.stderr.decode("utf-8", errors="replace")
    assert "claims tool_name" in stderr, stderr

    conn = sqlite3.connect(db_path)
    session_count = conn.execute("SELECT COUNT(*) FROM sessions").fetchone()[0]
    conn.close()
    assert session_count == 0


def test_export_otel_produces_deterministic_span_names_and_gates_on_redaction() -> None:
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    def mcptracer(*args: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    record = mcptracer(
        "record", "--client", "otel-test", "--redact", "default", "--",
        sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")
    conn = sqlite3.connect(db_path)
    (session_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    conn.close()

    spans_path = workdir / "spans.json"
    exported = mcptracer("sessions", "export-otel", session_id, "--out", str(spans_path))
    assert exported.returncode == 0, exported.stderr.decode("utf-8", errors="replace")

    document = json.loads(spans_path.read_text(encoding="utf-8"))
    resource = document["resourceSpans"][0]
    assert resource["resource"]["attributes"][0] == {"key": "service.name", "value": {"stringValue": "otel-test"}}
    spans = resource["scopeSpans"][0]["spans"]
    names = [span["name"] for span in spans]
    assert "tools/call echo" in names
    assert "initialize" in names

    tool_span = next(span for span in spans if span["name"] == "tools/call echo")
    assert tool_span["status"]["code"] == 1  # STATUS_CODE_OK
    attribute_keys = {a["key"] for a in tool_span["attributes"]}
    assert "gen_ai.operation.name" in attribute_keys
    assert "gen_ai.tool.name" in attribute_keys
    assert len(tool_span["traceId"]) == 32
    assert len(tool_span["spanId"]) == 16

    # Re-exporting to a fresh path is byte-identical (deterministic ids).
    spans_path_2 = workdir / "spans-2.json"
    exported_again = mcptracer("sessions", "export-otel", session_id, "--out", str(spans_path_2))
    assert exported_again.returncode == 0
    assert spans_path.read_bytes() == spans_path_2.read_bytes()

    # Exporting to an existing path refuses.
    refused = mcptracer("sessions", "export-otel", session_id, "--out", str(spans_path))
    assert refused.returncode != 0
    assert b"overwrite" in refused.stderr

    # An unredacted session refuses export without an explicit override.
    plain_record = mcptracer(
        "record", "--client", "plain", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert plain_record.returncode == 0, plain_record.stderr.decode("utf-8", errors="replace")
    conn = sqlite3.connect(db_path)
    ids = [row[0] for row in conn.execute("SELECT id FROM sessions ORDER BY started_at").fetchall()]
    conn.close()
    plain_id = ids[-1]

    plain_spans = workdir / "plain-spans.json"
    plain_refused = mcptracer("sessions", "export-otel", plain_id, "--out", str(plain_spans))
    assert plain_refused.returncode != 0
    assert b"--allow-unredacted" in plain_refused.stderr

    plain_allowed = mcptracer(
        "sessions", "export-otel", plain_id, "--out", str(plain_spans), "--allow-unredacted",
    )
    assert plain_allowed.returncode == 0, plain_allowed.stderr.decode("utf-8", errors="replace")


def test_vcr_cassette_import_produces_sane_correlated_exchanges() -> None:
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"

    def mcptracer(*args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    cassette = workdir / "session.vcr"
    cassette.write_text(
        json.dumps(
            {
                "version": 1,
                "interactions": [
                    {
                        "request": {
                            "jsonrpc": "2.0",
                            "id": 1,
                            "method": "initialize",
                            "params": {"protocolVersion": "2025-06-18"},
                        },
                        "response": {"jsonrpc": "2.0", "id": 1, "result": {"serverInfo": {"name": "vcr-fixture"}}},
                    },
                    {
                        "request": {
                            "jsonrpc": "2.0",
                            "id": 2,
                            "method": "tools/call",
                            "params": {"name": "echo", "arguments": {"message": "hi"}},
                        },
                        "response": {
                            "jsonrpc": "2.0",
                            "id": 2,
                            "result": {"content": [{"type": "text", "text": "Echo: hi"}]},
                        },
                    },
                ],
            }
        ),
        encoding="utf-8",
    )

    imported = mcptracer("import", str(cassette), "--client", "vcr-import-test")
    assert imported.returncode == 0, imported.stderr.decode("utf-8", errors="replace")
    match = re.search(r"as session ([0-9a-f-]{36})", imported.stdout.decode("utf-8"))
    assert match, imported.stdout.decode("utf-8", errors="replace")
    session_id = match.group(1)

    calls = mcptracer("sessions", "show", session_id, "--calls", "--json")
    assert calls.returncode == 0, calls.stderr.decode("utf-8", errors="replace")
    model = json.loads(calls.stdout)
    assert model["stats"]["total_exchanges"] == 2
    assert model["stats"]["ok"] == 2
    assert model["stats"]["errors"] == 0
    methods = {e["method"] for e in model["exchanges"]}
    assert methods == {"initialize", "tools/call"}
    tool_exchange = next(e for e in model["exchanges"] if e["method"] == "tools/call")
    assert tool_exchange["tool_name"] == "echo"
    assert tool_exchange["status"] == "ok"

    # Imported sessions are marked redaction policy `none` (the cassette
    # carries no redaction metadata).
    conn = sqlite3.connect(db_path)
    (policy,) = conn.execute(
        "SELECT redaction_policy FROM sessions WHERE id = ?", (session_id,)
    ).fetchone()
    conn.close()
    assert policy == "none"

    # An unrecognized cassette version fails cleanly.
    bad_cassette = workdir / "bad.vcr"
    bad_cassette.write_text(json.dumps({"version": 99, "interactions": []}), encoding="utf-8")
    bad_import = mcptracer("import", str(bad_cassette))
    assert bad_import.returncode != 0
    assert b"unsupported .vcr cassette version" in bad_import.stderr


def test_stats_and_search_surface_recorded_activity() -> None:
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    def mcptracer(*args: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    record = mcptracer(
        "record", "--client", "stats-test", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    (session_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    conn.close()

    stats = mcptracer("stats", session_id, "--json")
    assert stats.returncode == 0, stats.stderr.decode("utf-8", errors="replace")
    stats_json = json.loads(stats.stdout)
    # initialize, tools/list, tools/call all answered Ok.
    assert stats_json["ok"] == 3
    assert stats_json["errors"] == 0
    assert any(tool == "echo" and count == 1 for tool, count in stats_json["tool_call_counts"])
    assert stats_json["latency_p50_ns"] is not None

    search = mcptracer("search", "--tool", "echo", "--json")
    assert search.returncode == 0, search.stderr.decode("utf-8", errors="replace")
    hits = json.loads(search.stdout)
    assert len(hits) == 1
    assert hits[0]["session_id"] == session_id
    assert hits[0]["tool_name"] == "echo"

    empty = mcptracer("search", "--tool", "nonexistent", "--json")
    assert empty.returncode == 0
    assert json.loads(empty.stdout) == []


def test_bench_replays_recorded_session_as_load() -> None:
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    def mcptracer(*args: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    record = mcptracer(
        "record", "--client", "bench-test", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    (session_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    conn.close()

    bench = mcptracer(
        "bench",
        session_id,
        "--repeat",
        "20",
        "--concurrency",
        "4",
        "--json",
        "--",
        sys.executable,
        str(fake_server),
    )
    assert bench.returncode == 0, bench.stderr.decode("utf-8", errors="replace")
    report = json.loads(bench.stdout)
    assert report["iterations"] == 20
    assert report["concurrency"] == 4
    # SCRIPT has initialize, tools/list, tools/call: three answered requests.
    assert report["total_requests"] == 60
    assert report["total_errors"] == 0
    assert report["unanswered"] == 0
    assert report["throughput_sessions_per_sec"] > 0
    assert report["throughput_requests_per_sec"] > 0
    assert report["latency_p95_ms"] is not None

    # By default, bench warns that it re-executes recorded calls for real,
    # repeat times at up to concurrency concurrent iterations. Warning goes
    # to stderr even with --json on stdout; --i-understand-side-effects
    # suppresses it.
    bench_stderr = bench.stderr.decode("utf-8", errors="replace")
    assert "WARNING: bench re-executes" in bench_stderr
    assert "tools/call echo" in bench_stderr

    quiet_bench = mcptracer(
        "bench", session_id, "--repeat", "2", "--concurrency", "1",
        "--i-understand-side-effects", "--json",
        "--", sys.executable, str(fake_server),
    )
    assert quiet_bench.returncode == 0, quiet_bench.stderr.decode("utf-8", errors="replace")
    assert "WARNING: bench re-executes" not in quiet_bench.stderr.decode("utf-8", errors="replace")


def test_replay_and_bench_warn_when_source_session_is_redacted() -> None:
    # Replaying/benching a redacted session sends the stored payload as-is,
    # so a redacted "api_key" argument reaches the live target as the
    # literal "***REDACTED***" string, not the original secret. This is a
    # faithfulness bug (wrong data sent to a real server), not just a
    # confidentiality one, so both commands must warn loudly - detected from
    # the actual payload, not just the recorded policy - without blocking.
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"

    messages = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "0.1.0"},
            },
        },
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "echo",
                "arguments": {"message": "hi", "api_key": "sk-super-secret-value"},
            },
        },
    ]
    stdin = b"".join(frame(message) for message in messages)

    def mcptracer(*args: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    record = mcptracer(
        "record", "--client", "redact-source", "--redact", "default", "--",
        sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    (source_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    conn.close()

    replay = mcptracer(
        "replay", source_id, "--client", "replay-redacted", "--",
        sys.executable, str(fake_server),
    )
    assert replay.returncode == 0, replay.stderr.decode("utf-8", errors="replace")
    replay_stderr = replay.stderr.decode("utf-8", errors="replace")
    assert "still contain the \"***REDACTED***\" placeholder" in replay_stderr
    assert "tools/call echo" in replay_stderr

    # Non-blocking: the target session still gets recorded, and it too
    # stores the literal placeholder (replay sent it verbatim).
    conn = sqlite3.connect(db_path)
    target_payloads = [
        row[0]
        for row in conn.execute(
            "SELECT payload FROM messages WHERE tool_name = 'echo' AND direction = 'c2s' "
            "AND session_id != ?",
            (source_id,),
        ).fetchall()
    ]
    conn.close()
    assert target_payloads, "expected the replayed tools/call to be recorded"
    assert '"api_key":"***REDACTED***"' in target_payloads[0]

    quiet_replay = mcptracer(
        "replay", source_id, "--client", "replay-redacted-quiet",
        "--i-understand-side-effects", "--",
        sys.executable, str(fake_server),
    )
    assert quiet_replay.returncode == 0, quiet_replay.stderr.decode("utf-8", errors="replace")
    quiet_replay_stderr = quiet_replay.stderr.decode("utf-8", errors="replace")
    assert "still contain the \"***REDACTED***\" placeholder" not in quiet_replay_stderr

    bench = mcptracer(
        "bench", source_id, "--repeat", "2", "--concurrency", "1", "--json", "--",
        sys.executable, str(fake_server),
    )
    assert bench.returncode == 0, bench.stderr.decode("utf-8", errors="replace")
    bench_stderr = bench.stderr.decode("utf-8", errors="replace")
    assert "still contain the \"***REDACTED***\" placeholder" in bench_stderr
    assert "tools/call echo" in bench_stderr

    quiet_bench = mcptracer(
        "bench", source_id, "--repeat", "2", "--concurrency", "1",
        "--i-understand-side-effects", "--json", "--",
        sys.executable, str(fake_server),
    )
    assert quiet_bench.returncode == 0, quiet_bench.stderr.decode("utf-8", errors="replace")
    quiet_bench_stderr = quiet_bench.stderr.decode("utf-8", errors="replace")
    assert "still contain the \"***REDACTED***\" placeholder" not in quiet_bench_stderr


def test_oversized_unterminated_frame_is_capped() -> None:
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"

    # 9 MiB of junk with no newline: the proxy must cap its read buffer and
    # stop that direction instead of growing without bound, then exit cleanly.
    stdin = b"x" * (9 * 1024 * 1024)

    proc = subprocess.run(
        [
            "cargo", "run", "--quiet", "--bin", "mcptracer", "--",
            "--db", str(db_path),
            "record", "--client", "cap-test", "--",
            sys.executable, str(fake_server),
        ],
        input=stdin,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=repo,
        timeout=120,
        check=False,
    )

    # Non-zero: forwarding stopped mid-session and nothing was captured, so
    # exiting 0 would let a truncated recording pass a CI gate as healthy.
    assert proc.returncode != 0, proc.stdout.decode("utf-8", errors="replace")
    stderr_text = proc.stderr.decode("utf-8", errors="replace")
    assert "exceeded" in stderr_text, stderr_text

    # The session is still finalized despite the failure, so `validate` can
    # report on it rather than finding a dangling open session.
    conn = sqlite3.connect(db_path)
    sessions = conn.execute("SELECT total_messages, ended_at FROM sessions").fetchall()
    conn.close()
    assert len(sessions) == 1, sessions
    assert sessions[0][0] == 0, sessions
    assert sessions[0][1] is not None, "oversized-frame session was left unclosed"


def test_record_exits_when_the_server_exits_while_client_stdin_stays_open() -> None:
    """A wrapped server that exits must not strand the MCP client.

    Regression test: the two forwarding pumps used to be joined, so when the
    server closed stdout the client->server pump kept blocking on client stdin
    forever. mcptracer never exited and never closed its own stdout, so a
    server crash became a silent hang for the host client instead of the EOF
    it would have seen without the proxy in the middle.
    """
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"

    # Answers exactly one request, then exits — standing in for a server that
    # crashes or shuts itself down mid-session.
    server = workdir / "exits_after_one_response.py"
    server.write_text(
        "import sys\n"
        "sys.stdin.readline()\n"
        'sys.stdout.write(\'{"jsonrpc":"2.0","id":1,"result":{}}\\n\')\n'
        "sys.stdout.flush()\n"
        "sys.exit(0)\n",
        encoding="utf-8",
    )

    proc = subprocess.Popen(
        [
            "cargo", "run", "--quiet", "--bin", "mcptracer", "--",
            "--db", str(db_path),
            "record", "--client", "exit-test", "--",
            sys.executable, str(server),
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=repo,
    )
    try:
        assert proc.stdin is not None and proc.stdout is not None
        proc.stdin.write(frame({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))
        proc.stdin.flush()

        # The forwarded response proves the proxy is live before the server exits.
        assert proc.stdout.readline().strip(), "no response forwarded from the server"

        # Client stdin is deliberately still open here. The proxy must notice
        # the server is gone and exit on its own anyway.
        proc.wait(timeout=60)
    except subprocess.TimeoutExpired:
        proc.kill()
        raise AssertionError(
            "mcptracer record did not exit after its server exited with client stdin open"
        )
    finally:
        if proc.stdin is not None:
            proc.stdin.close()
        stderr_text = proc.stderr.read().decode("utf-8", errors="replace") if proc.stderr else ""
        proc.stdout.close()
        if proc.stderr is not None:
            proc.stderr.close()

    assert proc.returncode == 0, stderr_text
    assert "session" in stderr_text and "ended" in stderr_text, stderr_text

    # The exchange is captured and the session is closed, so it is usable as
    # evidence rather than being abandoned half-written.
    conn = sqlite3.connect(db_path)
    session_id, ended_at = conn.execute("SELECT id, ended_at FROM sessions").fetchone()
    messages = conn.execute(
        "SELECT direction, method FROM messages WHERE session_id = ? ORDER BY seq",
        (session_id,),
    ).fetchall()
    conn.close()
    assert ended_at is not None, "session was left open after the server exited"
    assert messages == [("c2s", "initialize"), ("s2c", None)], messages


def test_streamable_http_proxy_records_json_and_sse() -> None:
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "http-sessions.db"
    upstream_port = free_port()
    proxy_port = free_port()
    fake_server = repo / "tests" / "fake_mcp_http_server.py"
    binary_name = "mcptracer.exe" if os.name == "nt" else "mcptracer"
    binary = repo / "target" / "debug" / binary_name

    if not binary.exists():
        build = subprocess.run(
            ["cargo", "build", "--quiet", "--bin", "mcptracer"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=90,
            check=False,
        )
        assert build.returncode == 0, build.stderr.decode("utf-8", errors="replace")

    upstream = subprocess.Popen(
        [sys.executable, str(fake_server), str(upstream_port)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=repo,
    )
    proxy = None
    try:
        # This test runs late in a long, subprocess-heavy suite; on a loaded
        # CI runner (macOS especially) even a trivial Python HTTP fixture can
        # be slow to get scheduled and bind its port. Generous, not tight.
        wait_for_port(upstream_port, upstream, timeout=60)
        proxy = subprocess.Popen(
            [
                str(binary),
                "--db",
                str(db_path),
                "record-http",
                "--listen",
                f"127.0.0.1:{proxy_port}",
                "--target",
                f"http://127.0.0.1:{upstream_port}",
                "--client",
                "http-test",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
        )
        wait_for_port(proxy_port, proxy, timeout=60)

        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "Mcp-Session-Id": "client-session",
            "MCP-Protocol-Version": "2025-06-18",
            "Mcp-Method": "tools/list",
            "Mcp-Name": "http-test",
        }
        request = {"jsonrpc": "2.0", "id": "json-1", "method": "tools/list", "params": {}}
        status, response_headers, body = http_post(proxy_port, "/", request, headers)
        assert status == 200
        assert response_headers["mcp-session-id"] == "upstream-session"
        assert json.loads(body)["result"]["serverInfo"]["name"] == "fake-http-mcp"

        sse_request = {"jsonrpc": "2.0", "id": "sse-request", "method": "tools/list", "params": {}}
        status, response_headers, body = http_post(proxy_port, "/sse", sse_request, headers)
        assert status == 200
        assert response_headers["content-type"].startswith("text/event-stream")
        assert b'"sse-1"' in body and b'"sse-2"' in body

        with urllib.request.urlopen(f"http://127.0.0.1:{upstream_port}/observed", timeout=10) as response:
            observed = json.load(response)["headers"]
        assert observed["mcp-session-id"] == "client-session"
        assert observed["mcp-protocol-version"] == "2025-06-18"
        assert observed["mcp-method"] == "tools/list"
        assert observed["mcp-name"] == "http-test"

        deadline = time.monotonic() + 5
        rows = []
        while time.monotonic() < deadline:
            conn = sqlite3.connect(db_path)
            rows = conn.execute(
                "SELECT direction,rpc_id,method FROM messages ORDER BY seq"
            ).fetchall()
            conn.close()
            if len(rows) == 5:
                break
            time.sleep(0.05)
        assert len(rows) == 5, rows
        assert [row[0] for row in rows] == ["c2s", "s2c", "c2s", "s2c", "s2c"]
        assert rows[0][1:] == ('"json-1"', "tools/list")
        assert rows[3][1] == '"sse-1"'
        assert rows[4][1] == '"sse-2"'

        conn = sqlite3.connect(db_path)
        session = conn.execute("SELECT transport,client FROM sessions").fetchone()
        conn.close()
        assert session == ("streamable-http", "http-test")
    finally:
        if proxy is not None:
            stop_process(proxy)
        stop_process(upstream)


def test_replay_http_reproduces_json_and_sse_sessions_and_fails_clearly_on_bad_envelope() -> None:
    # T-75: replay-http re-sends a recorded session's client traffic directly
    # against a live target (no record-http proxy in the loop this time) and
    # must reproduce an equivalent session for both JSON and SSE responses
    # (diff passes), while an unsupported response envelope (neither
    # application/json nor text/event-stream) fails the run clearly instead
    # of silently losing data.
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "replay-http.db"
    upstream_port = free_port()
    proxy_port = free_port()
    fake_server = repo / "tests" / "fake_mcp_http_server.py"
    binary_name = "mcptracer.exe" if os.name == "nt" else "mcptracer"
    binary = repo / "target" / "debug" / binary_name

    if not binary.exists():
        build = subprocess.run(
            ["cargo", "build", "--quiet", "--bin", "mcptracer"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=90,
            check=False,
        )
        assert build.returncode == 0, build.stderr.decode("utf-8", errors="replace")

    def mcptracer(*args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(binary), "--db", str(db_path), *args],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    upstream = subprocess.Popen(
        [sys.executable, str(fake_server), str(upstream_port)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=repo,
    )
    proxy = None
    try:
        wait_for_port(upstream_port, upstream, timeout=60)

        # Record two header-less (one-shot, provisional-session) exchanges
        # through record-http: one against the JSON route, one against the
        # SSE route.
        proxy = subprocess.Popen(
            [
                str(binary), "--db", str(db_path), "record-http",
                "--listen", f"127.0.0.1:{proxy_port}",
                "--target", f"http://127.0.0.1:{upstream_port}",
                "--client", "replay-http-source",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
        )
        wait_for_port(proxy_port, proxy, timeout=60)

        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        }
        json_request = {"jsonrpc": "2.0", "id": "json-1", "method": "tools/list", "params": {}}
        status, _, _ = http_post(proxy_port, "/", json_request, headers)
        assert status == 200

        # `/sse-echo` (unlike `/sse`) echoes the request's own id in a single
        # response event, so the recording correlates cleanly and passes
        # `require_healthy_session` the way a real server's traffic would.
        sse_request = {"jsonrpc": "2.0", "id": "sse-request", "method": "tools/list", "params": {}}
        status, _, _ = http_post(proxy_port, "/sse-echo", sse_request, headers)
        assert status == 200

        # Both exchanges are header-less (one-shot, provisional) sessions
        # that record-http finalizes asynchronously right after the
        # response finishes streaming; wait for that finalization to land
        # before killing the proxy, or the session can still show
        # `ended_at IS NULL` (SessionNotClosed) despite the client already
        # having the full response.
        deadline = time.monotonic() + 10
        closed_count = 0
        while time.monotonic() < deadline:
            conn = sqlite3.connect(db_path)
            closed_count = conn.execute(
                "SELECT COUNT(*) FROM sessions WHERE ended_at IS NOT NULL"
            ).fetchone()[0]
            conn.close()
            if closed_count == 2:
                break
            time.sleep(0.05)
        assert closed_count == 2, f"expected 2 closed sessions, got {closed_count}"

        stop_process(proxy)
        proxy = None

        conn = sqlite3.connect(db_path)
        source_ids = [
            row[0]
            for row in conn.execute("SELECT id FROM sessions ORDER BY started_at").fetchall()
        ]
        conn.close()
        assert len(source_ids) == 2, source_ids
        json_session_id, sse_session_id = source_ids

        def replay(session_id: str, target: str) -> str:
            result = mcptracer(
                "replay-http", session_id,
                "--target", target,
                "--client", "replay-http-replayed",
                "--i-understand-side-effects",
            )
            assert result.returncode == 0, result.stderr.decode("utf-8", errors="replace")
            stderr_text = result.stderr.decode("utf-8", errors="replace")
            match = re.search(r"HTTP-replaying session \S+ as (\S+) ->", stderr_text)
            assert match, stderr_text
            return match.group(1)

        json_replayed_id = replay(json_session_id, f"http://127.0.0.1:{upstream_port}/")
        sse_replayed_id = replay(sse_session_id, f"http://127.0.0.1:{upstream_port}/sse-echo")

        json_diff = mcptracer("diff", json_session_id, json_replayed_id, "--ignore-latency")
        assert json_diff.returncode == 0, json_diff.stdout.decode("utf-8", errors="replace")
        assert b"Sessions match" in json_diff.stdout

        sse_diff = mcptracer("diff", sse_session_id, sse_replayed_id, "--ignore-latency")
        assert sse_diff.returncode == 0, sse_diff.stdout.decode("utf-8", errors="replace")
        assert b"Sessions match" in sse_diff.stdout

        # An unsupported response envelope (neither application/json nor
        # text/event-stream) must fail the run clearly rather than silently
        # dropping the response.
        bad_envelope = mcptracer(
            "replay-http", json_session_id,
            "--target", f"http://127.0.0.1:{upstream_port}/plain",
            "--client", "replay-http-bad-envelope",
            "--i-understand-side-effects",
        )
        assert bad_envelope.returncode != 0
        bad_envelope_stderr = bad_envelope.stderr.decode("utf-8", errors="replace")
        assert "unsupported response envelope" in bad_envelope_stderr, bad_envelope_stderr
    finally:
        if proxy is not None:
            stop_process(proxy)
        stop_process(upstream)


def test_replay_http_caps_oversized_json_response() -> None:
    # replay-http's own response capture must be bounded the same way
    # record-http's is (MAX_FRAME_BYTES, 8 MiB): a target returning an
    # oversized JSON body must not be buffered without limit. The read stops
    # at the cap (proving memory use is actually bounded, not just slow to
    # grow), but a dropped capture still fails the run's exit code, same as
    # any other capture loss during replay.
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "replay-http-oversized.db"
    upstream_port = free_port()
    proxy_port = free_port()
    fake_server = repo / "tests" / "fake_mcp_http_server.py"
    binary_name = "mcptracer.exe" if os.name == "nt" else "mcptracer"
    binary = repo / "target" / "debug" / binary_name

    if not binary.exists():
        build = subprocess.run(
            ["cargo", "build", "--quiet", "--bin", "mcptracer"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=90,
            check=False,
        )
        assert build.returncode == 0, build.stderr.decode("utf-8", errors="replace")

    def mcptracer(*args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(binary), "--db", str(db_path), *args],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    upstream = subprocess.Popen(
        [sys.executable, str(fake_server), str(upstream_port)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=repo,
    )
    proxy = None
    try:
        wait_for_port(upstream_port, upstream, timeout=60)

        proxy = subprocess.Popen(
            [
                str(binary), "--db", str(db_path), "record-http",
                "--listen", f"127.0.0.1:{proxy_port}",
                "--target", f"http://127.0.0.1:{upstream_port}",
                "--client", "replay-http-oversized-source",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
        )
        wait_for_port(proxy_port, proxy, timeout=60)

        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        }
        json_request = {"jsonrpc": "2.0", "id": "json-1", "method": "tools/list", "params": {}}
        status, _, _ = http_post(proxy_port, "/", json_request, headers)
        assert status == 200

        deadline = time.monotonic() + 10
        closed_count = 0
        while time.monotonic() < deadline:
            conn = sqlite3.connect(db_path)
            closed_count = conn.execute(
                "SELECT COUNT(*) FROM sessions WHERE ended_at IS NOT NULL"
            ).fetchone()[0]
            conn.close()
            if closed_count == 1:
                break
            time.sleep(0.05)
        assert closed_count == 1, f"expected 1 closed session, got {closed_count}"

        stop_process(proxy)
        proxy = None

        conn = sqlite3.connect(db_path)
        (source_session_id,) = conn.execute("SELECT id FROM sessions").fetchone()
        conn.close()

        replayed = mcptracer(
            "replay-http", source_session_id,
            "--target", f"http://127.0.0.1:{upstream_port}/big",
            "--client", "replay-http-oversized-replayed",
            "--i-understand-side-effects",
        )
        stderr_text = replayed.stderr.decode("utf-8", errors="replace")
        # A dropped capture fails the run's exit code (same "any drop is a
        # hard failure" contract stdio `replay` already has via the shared
        # `finish_storage_writer`/`spawn_storage_writer` machinery) even
        # though the request itself was sent and the drop was bounded, not
        # unbounded memory growth.
        assert replayed.returncode != 0, stderr_text
        assert "exceeded" in stderr_text, stderr_text
        assert "capture lost" in stderr_text, stderr_text

        match = re.search(r"HTTP-replaying session \S+ as (\S+) ->", stderr_text)
        assert match, stderr_text
        replayed_session_id = match.group(1)

        conn = sqlite3.connect(db_path)
        total_messages, dropped_messages = conn.execute(
            "SELECT total_messages, dropped_messages FROM sessions WHERE id = ?",
            (replayed_session_id,),
        ).fetchone()
        conn.close()
        # The outgoing request was recorded; the oversized response was not.
        assert total_messages == 1, total_messages
        assert dropped_messages >= 1, dropped_messages
    finally:
        if proxy is not None:
            stop_process(proxy)
        stop_process(upstream)


def test_modern_streamable_http_records_grouped_stateless_calls_and_replays_required_headers() -> None:
    # MCP 2026-07-28 has no initialize exchange or Mcp-Session-Id. The
    # recorder therefore needs an explicit caller-owned trace boundary to
    # group independent HTTP requests, while replay-http must reconstruct
    # the required protocol/method/name routing headers from body metadata.
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "modern-http.db"
    upstream_port = free_port()
    proxy_port = free_port()
    fake_server = repo / "tests" / "fake_mcp_http_server.py"
    binary_name = "mcptracer.exe" if os.name == "nt" else "mcptracer"
    binary = repo / "target" / "debug" / binary_name

    if not binary.exists():
        build = subprocess.run(
            ["cargo", "build", "--quiet", "--bin", "mcptracer"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=90,
            check=False,
        )
        assert build.returncode == 0, build.stderr.decode("utf-8", errors="replace")

    def mcptracer(*args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(binary), "--db", str(db_path), *args],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    def modern_request(request_id: str, method: str, **params: object) -> dict:
        return {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": {
                **params,
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": {
                        "name": "modern-test-client",
                        "version": "1.0",
                    },
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            },
        }

    upstream = subprocess.Popen(
        [sys.executable, str(fake_server), str(upstream_port)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=repo,
    )
    proxy = None
    subscription_connection = None
    try:
        wait_for_port(upstream_port, upstream, timeout=60)
        proxy = subprocess.Popen(
            [
                str(binary), "--db", str(db_path), "record-http",
                "--listen", f"127.0.0.1:{proxy_port}",
                "--target", f"http://127.0.0.1:{upstream_port}/modern",
                "--client", "modern-http-source",
                "--group-stateless-by-header", "X-Mcptracer-Trace",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
        )
        wait_for_port(proxy_port, proxy, timeout=60)

        common = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "MCP-Protocol-Version": "2026-07-28",
            "X-Mcptracer-Trace": "trace-1",
        }
        discover = modern_request("discover-1", "server/discover")
        status, response_headers, body = http_post(
            proxy_port, "/", discover, {**common, "Mcp-Method": "server/discover"}
        )
        assert status == 200
        assert "mcp-session-id" not in response_headers
        assert json.loads(body)["result"]["resultType"] == "complete"

        listed = modern_request("list-1", "tools/list")
        status, response_headers, body = http_post(
            proxy_port, "/", listed, {**common, "Mcp-Method": "tools/list"}
        )
        assert status == 200
        assert "mcp-session-id" not in response_headers
        assert json.loads(body)["result"]["tools"][0]["name"] == "echo"

        listen = modern_request(
            "listen-1", "subscriptions/listen", notifications={"toolsListChanged": True}
        )
        subscription_connection = http.client.HTTPConnection(
            "127.0.0.1", proxy_port, timeout=10
        )
        subscription_connection.request(
            "POST",
            "/",
            body=json.dumps(listen, separators=(",", ":")).encode("utf-8"),
            headers={**common, "Mcp-Method": "subscriptions/listen"},
        )
        subscription_response = subscription_connection.getresponse()
        assert subscription_response.status == 200
        assert subscription_response.getheader("Content-Type", "").startswith(
            "text/event-stream"
        )

        def read_subscription_event() -> dict:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                line = subscription_response.fp.readline()
                assert line, "subscription stream closed before expected event"
                if not line.startswith(b"data: "):
                    continue
                payload = json.loads(line[6:].strip())
                assert subscription_response.fp.readline() in (b"\n", b"\r\n")
                return payload
            raise AssertionError("timed out waiting for subscription event")

        acknowledgement = read_subscription_event()
        assert acknowledgement["method"] == "notifications/subscriptions/acknowledged"
        assert acknowledgement["params"]["_meta"][
            "io.modelcontextprotocol/subscriptionId"
        ] == "listen-1"

        mrtr_first = modern_request("mrtr-1", "tools/call", name="mrtr-echo", arguments={})
        status, response_headers, body = http_post(
            proxy_port,
            "/",
            mrtr_first,
            {**common, "Mcp-Method": "tools/call", "Mcp-Name": "mrtr-echo"},
        )
        assert status == 200
        assert "mcp-session-id" not in response_headers
        input_required = json.loads(body)["result"]
        assert input_required["resultType"] == "input_required"

        mrtr_retry = modern_request(
            "mrtr-2",
            "tools/call",
            name="mrtr-echo",
            arguments={},
            inputResponses={"approval": {"action": "accept", "content": {"approved": True}}},
            requestState=input_required["requestState"],
        )
        status, response_headers, body = http_post(
            proxy_port,
            "/",
            mrtr_retry,
            {**common, "Mcp-Method": "tools/call", "Mcp-Name": "mrtr-echo"},
        )
        assert status == 200
        assert "mcp-session-id" not in response_headers
        assert json.loads(body)["result"]["resultType"] == "complete"

        call = modern_request("call-1", "tools/call", name="echo", arguments={"text": "hi"})
        status, response_headers, body = http_post(
            proxy_port,
            "/",
            call,
            {**common, "Mcp-Method": "tools/call", "Mcp-Name": "echo", "Mcp-Param-Text": "hi"},
        )
        assert status == 200
        assert "mcp-session-id" not in response_headers
        assert json.loads(body)["result"]["ok"] is True
        change = read_subscription_event()
        assert change["method"] == "notifications/tools/list_changed"
        assert change["params"]["_meta"][
            "io.modelcontextprotocol/subscriptionId"
        ] == "listen-1"
        subscription_response.close()
        subscription_connection.close()
        subscription_connection = None

        # Explicitly close the local correlation group. A modern upstream is
        # allowed to reject DELETE; the recorder still treats this as the
        # caller-owned end of the evidence trace after forwarding it.
        conn = http.client.HTTPConnection("127.0.0.1", proxy_port, timeout=10)
        conn.request("DELETE", "/", headers={"X-Mcptracer-Trace": "trace-1"})
        delete_response = conn.getresponse()
        delete_response.read()
        conn.close()

        deadline = time.monotonic() + 10
        source_id = None
        while time.monotonic() < deadline:
            conn = sqlite3.connect(db_path)
            row = conn.execute(
                "SELECT id FROM sessions WHERE client = ? AND ended_at IS NOT NULL",
                ("modern-http-source",),
            ).fetchone()
            conn.close()
            if row:
                source_id = row[0]
                break
            time.sleep(0.05)
        assert source_id is not None

        validate = mcptracer("validate", source_id)
        assert validate.returncode == 0, validate.stdout.decode("utf-8", errors="replace")

        replay = mcptracer(
            "replay-http", source_id,
            "--target", f"http://127.0.0.1:{upstream_port}/modern",
            "--client", "modern-http-replayed",
            "--i-understand-side-effects",
        )
        assert replay.returncode == 0, replay.stderr.decode("utf-8", errors="replace")
        match = re.search(
            r"HTTP-replaying session \S+ as (\S+) ->",
            replay.stderr.decode("utf-8", errors="replace"),
        )
        assert match, replay.stderr.decode("utf-8", errors="replace")

        diff = mcptracer("diff", source_id, match.group(1), "--ignore-latency")
        assert diff.returncode == 0, diff.stdout.decode("utf-8", errors="replace")
        assert b"Sessions match" in diff.stdout

        with urllib.request.urlopen(
            f"http://127.0.0.1:{upstream_port}/observed", timeout=10
        ) as response:
            observed_response = json.load(response)
        observed = observed_response["headers"]
        assert observed_response["mrtr_live_retry_used_fresh_id"] is True
        assert observed_response["subscription_event_delivery_count"] >= 2
        assert observed["mcp-protocol-version"] == "2026-07-28"
        assert observed["mcp-method"] == "tools/call"
        assert observed["mcp-name"] == "echo"
        assert observed["mcp-param-text"] == "hi"
        assert "mcp-session-id" not in observed
    finally:
        if subscription_connection is not None:
            subscription_connection.close()
        if proxy is not None:
            stop_process(proxy)
        stop_process(upstream)


def test_streamable_http_partitions_recordings_by_logical_session() -> None:
    # T-70: one proxy lifetime must not collapse into one recording. A
    # header-less exchange (no Mcp-Session-Id yet, e.g. initialize or a
    # stateless server) gets its own one-shot session; two concurrent
    # established sessions reusing the same JSON-RPC id must never
    # correlate; a DELETE against an established session finalizes it.
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "http-partition-sessions.db"
    upstream_port = free_port()
    proxy_port = free_port()
    fake_server = repo / "tests" / "fake_mcp_http_server.py"
    binary_name = "mcptracer.exe" if os.name == "nt" else "mcptracer"
    binary = repo / "target" / "debug" / binary_name

    if not binary.exists():
        build = subprocess.run(
            ["cargo", "build", "--quiet", "--bin", "mcptracer"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=90,
            check=False,
        )
        assert build.returncode == 0, build.stderr.decode("utf-8", errors="replace")

    upstream = subprocess.Popen(
        [sys.executable, str(fake_server), str(upstream_port)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=repo,
    )
    proxy = None
    try:
        wait_for_port(upstream_port, upstream, timeout=60)
        proxy = subprocess.Popen(
            [
                str(binary),
                "--db",
                str(db_path),
                "record-http",
                "--listen",
                f"127.0.0.1:{proxy_port}",
                "--target",
                f"http://127.0.0.1:{upstream_port}",
                "--client",
                "partition-test",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
        )
        wait_for_port(proxy_port, proxy, timeout=60)

        base_headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        }
        request = {"jsonrpc": "2.0", "id": "1", "method": "tools/list", "params": {}}

        # No Mcp-Session-Id yet: a provisional, one-shot exchange.
        status, _, _ = http_post(proxy_port, "/", request, dict(base_headers))
        assert status == 200

        # Two concurrent established sessions reusing the identical id "1".
        status, _, _ = http_post(
            proxy_port, "/", request, {**base_headers, "Mcp-Session-Id": "session-A"}
        )
        assert status == 200
        status, _, _ = http_post(
            proxy_port, "/", request, {**base_headers, "Mcp-Session-Id": "session-B"}
        )
        assert status == 200

        # DELETE terminates session-A per the Streamable HTTP spec; session-B
        # stays open (mirrors a client that never explicitly tears down).
        conn = http.client.HTTPConnection("127.0.0.1", proxy_port, timeout=10)
        conn.request("DELETE", "/", headers={"Mcp-Session-Id": "session-A"})
        delete_response = conn.getresponse()
        delete_response.read()
        conn.close()
        assert delete_response.status == 204

        deadline = time.monotonic() + 5
        sessions: list[tuple[str, "int | None"]] = []
        while time.monotonic() < deadline:
            conn = sqlite3.connect(db_path)
            sessions = conn.execute("SELECT id, ended_at FROM sessions").fetchall()
            conn.close()
            if len(sessions) == 3 and sum(1 for _, ended in sessions if ended is not None) == 2:
                break
            time.sleep(0.05)

        assert len(sessions) == 3, sessions
        ended_count = sum(1 for _, ended in sessions if ended is not None)
        assert ended_count == 2, sessions  # provisional exchange + DELETEd session-A

        conn = sqlite3.connect(db_path)
        rows_by_session = {
            session_id: conn.execute(
                "SELECT direction, rpc_id FROM messages WHERE session_id = ? ORDER BY seq",
                (session_id,),
            ).fetchall()
            for session_id, _ in sessions
        }
        conn.close()

        # Each session recorded exactly its own request/response pair for
        # rpc_id "1" — the shared id never merges the three sessions.
        for rows in rows_by_session.values():
            assert rows == [("c2s", '"1"'), ("s2c", '"1"')], rows_by_session
    finally:
        if proxy is not None:
            stop_process(proxy)
        stop_process(upstream)


def test_replay_and_bench_plan_mode() -> None:
    # T-71: --plan must report the calls a real run would make — including a
    # tool call whose risk is unknown because it was never in the session's
    # own tools/list capture — without spawning a server process (no trailing
    # server command needed at all), and --allow-tool/--deny-tool must filter
    # both the plan report and what real execution actually sends.
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"

    messages = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "0.1.0"},
            },
        },
        {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}},
        {"jsonrpc": "2.0", "id": "list-1", "method": "tools/list", "params": {}},
        {
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"message": "hi"}},
        },
        {
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {"name": "delete_all", "arguments": {}},
        },
    ]
    stdin = b"".join(frame(message) for message in messages)

    def mcptracer(*args: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    record = mcptracer(
        "record", "--client", "plan-test", "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    (source_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    conn.close()

    # No trailing server command at all: --plan launches nothing.
    replay_plan_result = mcptracer("replay", source_id, "--plan")
    assert replay_plan_result.returncode == 0, replay_plan_result.stderr.decode("utf-8", errors="replace")
    plan = json.loads(replay_plan_result.stdout)

    assert plan["total_calls"] == 5  # initialize, notifications/initialized, tools/list, 2x tools/call
    assert plan["allowed_calls"] == 5
    assert not plan["requires_acknowledgement"]

    calls_by_tool = {c["tool_name"]: c for c in plan["calls"] if c["tool_name"] is not None}
    assert set(calls_by_tool) == {"echo", "delete_all"}
    # "echo" was observed in this session's own tools/list capture (with no
    # hints declared); "delete_all" was never listed at all. Both still
    # classify as unknown risk, but the JSON distinguishes "saw it, no hints"
    # from "never saw it" via the annotations field.
    assert calls_by_tool["echo"]["risk"] == "unknown"
    assert calls_by_tool["echo"]["annotations"] is not None
    assert calls_by_tool["delete_all"]["risk"] == "unknown"
    assert calls_by_tool["delete_all"]["annotations"] is None

    # --allow-tool/--deny-tool mark filtered-out calls without dropping them
    # from the report.
    filtered_plan = json.loads(
        mcptracer("replay", source_id, "--plan", "--deny-tool", "delete_all").stdout
    )
    assert filtered_plan["total_calls"] == 5
    assert filtered_plan["allowed_calls"] == 4
    assert filtered_plan["filtered_out_calls"] == 1
    denied_call = next(c for c in filtered_plan["calls"] if c["tool_name"] == "delete_all")
    assert denied_call["allowed"] is False

    bench_plan_result = mcptracer("bench", source_id, "--plan")
    assert bench_plan_result.returncode == 0, bench_plan_result.stderr.decode("utf-8", errors="replace")
    bench_plan = json.loads(bench_plan_result.stdout)
    assert bench_plan["total_calls"] == 5
    assert bench_plan["allowed_calls"] == 5

    # Real execution: --deny-tool must actually stop the denied call from
    # being sent, not just report it.
    replay_result = mcptracer(
        "replay", source_id, "--client", "replay-filtered", "--deny-tool", "delete_all",
        "--i-understand-side-effects", "--", sys.executable, str(fake_server),
    )
    assert replay_result.returncode == 0, replay_result.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    session_ids = [row[0] for row in conn.execute("SELECT id FROM sessions").fetchall()]
    (target_id,) = [sid for sid in session_ids if sid != source_id]
    tool_calls = [
        row[0]
        for row in conn.execute(
            "SELECT tool_name FROM messages WHERE session_id = ? AND method = 'tools/call'",
            (target_id,),
        ).fetchall()
    ]
    conn.close()
    assert tool_calls == ["echo"]


def test_inspect_serves_read_only_session_ui() -> None:
    repo = Path(__file__).resolve().parent.parent
    db_path = make_workdir(repo) / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"
    stdin = b"".join(frame(message) for message in SCRIPT)

    record = subprocess.run(
        [
            "cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path),
            "record", "--client", "inspect-test", "--", sys.executable, str(fake_server),
        ],
        input=stdin,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=repo,
        timeout=60,
        check=False,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    (session_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    conn.close()

    port = free_port()
    stderr_path = db_path.parent / "inspector.stderr"
    with stderr_path.open("wb") as stderr_file:
        proc = subprocess.Popen(
            [
                "cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path),
                "inspect", "--listen", f"127.0.0.1:{port}",
            ],
            stdout=subprocess.PIPE,
            stderr=stderr_file,
            cwd=repo,
        )
        try:
            wait_for_port(port, proc, timeout=30)
            match = None
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                match = re.search(r"http://127\.0\.0\.1:\d+/\#token=([a-f0-9]{32})", stderr_path.read_text())
                if match:
                    break
                time.sleep(0.01)
            assert match, "inspector did not print its authenticated launch link"
            token = match.group(1)
            base = f"http://127.0.0.1:{port}"

            def request(path: str, headers: dict[str, str] | None = None, expected: int = 200):
                req = urllib.request.Request(base + path, headers=headers or {})
                try:
                    response = urllib.request.urlopen(req, timeout=5)
                except urllib.error.HTTPError as error:
                    response = error
                with response:
                    body = response.read()
                    assert response.status == expected, (path, response.status, expected)
                    assert response.headers.get("Cache-Control") == "no-store"
                    assert token.encode() not in body
                    if expected != 200:
                        assert b"hello mcptracer" not in body
                    return body

            assert b"mcptracer inspector" in request("/")
            assert b"Authorization" in request("/app.js")
            auth = {"Authorization": f"Bearer {token}", "Origin": base}
            for endpoint in ["/api/sessions", f"/api/sessions/{session_id}", "/api/sessions/nonexistent-session"]:
                request(endpoint, expected=401)
                request(endpoint, {"Authorization": "Bearer invalid"}, expected=401)
            request(f"/api/sessions?token={token}", expected=401)
            for endpoint in ["/", "/app.js", "/api/sessions", f"/api/sessions/{session_id}"]:
                request(endpoint, {**auth, "Host": f"audit.invalid:{port}"}, expected=403)
                request(endpoint, {**auth, "Origin": "http://audit.invalid"}, expected=403)
                request(endpoint, {**auth, "Origin": "null"}, expected=403)
            sessions = json.loads(request("/api/sessions", auth))
            assert any(s["id"] == session_id for s in sessions)
            detail = json.loads(request(f"/api/sessions/{session_id}", auth))
            assert detail["session"]["id"] == session_id
            assert len(detail["messages"]) == 7
            assert detail["model"]["stats"]["total_exchanges"] == 3
            assert detail["model"]["stats"]["ok"] == 3
            call = next(m for m in detail["messages"] if m["tool_name"] == "echo")
            assert call["payload"]["params"]["arguments"]["message"] == "hello mcptracer"
            request("/api/sessions/nonexistent-session", auth, expected=404)
        finally:
            stop_process(proc)


def test_export_sensitive_content_lint_aborts_on_bearer_token() -> None:
    """T-63: export must refuse to write an artifact when a bearer token is
    embedded in a free-text payload field that key-name redaction cannot mask,
    and must report the JSON pointer and category without echoing the value."""
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"

    # The token that the client sends in a free-text field (not a sensitive key
    # name, so key-name redaction will leave it in the stored payload verbatim).
    bearer_token = "Bearer eyJhbGciOiJSUzI1NiJ9.sensitivePayloadHere"

    messages = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "lint-test", "version": "0.1.0"},
            },
        },
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            # 'message' is NOT a sensitive key name, so key-name redaction
            # leaves it alone. The bearer token is embedded in its value.
            "params": {"name": "echo", "arguments": {"message": bearer_token}},
        },
    ]
    stdin = b"".join(frame(msg) for msg in messages)

    def mcptracer(*args: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=120,
            check=False,
        )

    # Record without redaction so the bearer token survives into storage.
    record = mcptracer(
        "record", "--client", "lint-test",
        "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    (session_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    conn.close()

    out_path = workdir / "session.mtrace"

    # Export without --allow-unredacted and without --allow-sensitive-content
    # must FAIL because the payload contains a bearer token.
    export = mcptracer(
        "export", session_id,
        "--out", str(out_path),
        "--allow-unredacted",  # allow the missing key-name redaction
        # --allow-sensitive-content is intentionally NOT passed
    )
    assert export.returncode != 0, (
        "export must fail when a bearer token is in the payload\n"
        + export.stdout.decode("utf-8", errors="replace")
    )
    stderr = export.stderr.decode("utf-8", errors="replace")
    # The error must report the category and a JSON pointer, never the token.
    assert "bearer_token" in stderr, f"expected 'bearer_token' in stderr, got:\n{stderr}"
    assert bearer_token not in stderr, "the actual token value must never be echoed"
    # The output file must not have been written.
    assert not out_path.exists(), "artifact must not exist after an aborted export"


def test_export_allow_sensitive_content_overrides_abort() -> None:
    """T-63: --allow-sensitive-content overrides the lint abort, writes the
    artifact, and prints a warning listing pointer + category (not the value)."""
    repo = Path(__file__).resolve().parent.parent
    workdir = make_workdir(repo)
    db_path = workdir / "sessions.db"
    fake_server = repo / "tests" / "fake_mcp_server.py"

    bearer_token = "Bearer eyJhbGciOiJSUzI1NiJ9.sensitivePayloadHere"

    messages = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "lint-override-test", "version": "0.1.0"},
            },
        },
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"message": bearer_token}},
        },
    ]
    stdin = b"".join(frame(msg) for msg in messages)

    def mcptracer(*args: str, input_bytes: bytes | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["cargo", "run", "--quiet", "--bin", "mcptracer", "--", "--db", str(db_path), *args],
            input=input_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=120,
            check=False,
        )

    record = mcptracer(
        "record", "--client", "lint-override-test",
        "--", sys.executable, str(fake_server),
        input_bytes=stdin,
    )
    assert record.returncode == 0, record.stderr.decode("utf-8", errors="replace")

    conn = sqlite3.connect(db_path)
    (session_id,) = conn.execute("SELECT id FROM sessions").fetchone()
    conn.close()

    out_path = workdir / "session.mtrace"

    export = mcptracer(
        "export", session_id,
        "--out", str(out_path),
        "--allow-unredacted",
        "--allow-sensitive-content",
    )
    assert export.returncode == 0, (
        "export with --allow-sensitive-content must succeed\n"
        + export.stderr.decode("utf-8", errors="replace")
    )
    stderr = export.stderr.decode("utf-8", errors="replace")
    # Warning must include the category but never the actual token value.
    assert "WARNING" in stderr, f"expected a WARNING in stderr, got:\n{stderr}"
    assert "bearer_token" in stderr, f"expected 'bearer_token' in warning, got:\n{stderr}"
    assert bearer_token not in stderr, "the actual token value must never be echoed"
    # The artifact must have been written.
    assert out_path.exists(), "artifact must exist when --allow-sensitive-content is passed"
    assert out_path.stat().st_size > 0, "artifact must not be empty"


def test_canonical_demos_catch_their_scenarios() -> None:
    # T-76: the two published examples/ demos are runnable regression checks,
    # not just narrated walkthroughs - this exercises the same scenarios
    # their run.sh scripts do, directly against the real compiled CLI, so
    # the demos can't silently bit-rot out of sync with the commands they
    # showcase.
    repo = Path(__file__).resolve().parent.parent
    binary_name = "mcptracer.exe" if os.name == "nt" else "mcptracer"
    binary = repo / "target" / "debug" / binary_name

    if not binary.exists():
        build = subprocess.run(
            ["cargo", "build", "--quiet", "--bin", "mcptracer"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=90,
            check=False,
        )
        assert build.returncode == 0, build.stderr.decode("utf-8", errors="replace")

    def record(demo_dir: Path, db_path: Path, env_overrides: dict[str, str]) -> str:
        stdin = (demo_dir / "client_script.jsonl").read_bytes()
        proc = subprocess.run(
            [
                str(binary), "--db", str(db_path), "record", "--client", "demo",
                "--", sys.executable, str(demo_dir / "server.py"),
            ],
            input=stdin,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=demo_dir,
            timeout=60,
            check=False,
            env={**os.environ, **env_overrides},
        )
        assert proc.returncode == 0, proc.stderr.decode("utf-8", errors="replace")
        match = re.search(
            r"recording session (\S+) ->", proc.stderr.decode("utf-8", errors="replace")
        )
        assert match, proc.stderr.decode("utf-8", errors="replace")
        return match.group(1)

    def mcptracer_cmd(db_path: Path, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(binary), "--db", str(db_path), *args],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=repo,
            timeout=60,
            check=False,
        )

    # -- rug-pull-demo --------------------------------------------------
    rug_pull_dir = repo / "examples" / "rug-pull-demo"
    rug_pull_db = make_workdir(repo) / "rug-pull-demo.db"

    baseline_id = record(rug_pull_dir, rug_pull_db, {})
    pin_pass = mcptracer_cmd(
        rug_pull_db, "assert", baseline_id, "--spec", str(rug_pull_dir / "pin.toml")
    )
    assert pin_pass.returncode == 0, pin_pass.stdout.decode("utf-8", errors="replace")

    candidate_id = record(rug_pull_dir, rug_pull_db, {"MCPTRACER_DEMO_RUG_PULLED": "1"})
    diff_result = mcptracer_cmd(
        rug_pull_db, "diff", baseline_id, candidate_id, "--ignore-latency"
    )
    assert diff_result.returncode == 1, diff_result.stdout.decode("utf-8", errors="replace")
    assert b"SECURITY" in diff_result.stdout

    pin_fail = mcptracer_cmd(
        rug_pull_db, "assert", candidate_id, "--spec", str(rug_pull_dir / "pin.toml")
    )
    assert pin_fail.returncode == 1, pin_fail.stdout.decode("utf-8", errors="replace")
    assert b"tools_pinned" in pin_fail.stdout

    # -- latency-regression-demo -----------------------------------------
    latency_dir = repo / "examples" / "latency-regression-demo"
    latency_db = make_workdir(repo) / "latency-regression-demo.db"

    fast_id = record(latency_dir, latency_db, {})
    fast_check = mcptracer_cmd(
        latency_db, "assert", fast_id, "--spec", str(latency_dir / "checks.toml")
    )
    assert fast_check.returncode == 0, fast_check.stdout.decode("utf-8", errors="replace")

    slow_id = record(latency_dir, latency_db, {"MCPTRACER_DEMO_SLOW_MS": "250"})
    slow_check = mcptracer_cmd(
        latency_db, "assert", slow_id, "--spec", str(latency_dir / "checks.toml")
    )
    assert slow_check.returncode == 1, slow_check.stdout.decode("utf-8", errors="replace")
    assert b"search never exceeds 100ms" in slow_check.stdout






if __name__ == "__main__":
    test_proxy_records_session()
    test_sessions_show_calls_json_correlates_exchanges()
    test_replay_reproduces_methods_and_tools_in_order()
    test_serve_replays_recorded_responses_without_the_upstream_server()
    test_proxy_redacts_stored_secrets_but_forwards_them_unchanged()
    test_diff_detects_changes_and_security_findings()
    test_diff_batch_aggregates_pairs_and_gates_on_fail_on_breaking()
    test_optimize_mines_history_for_suggestions()
    test_index_rebuild_surfaces_tool_versions_and_supersession()
    test_assert_rule_and_snapshot_modes()
    test_assert_manifest_and_offline_verify()
    test_verify_rejects_hostile_manifests()
    test_baseline_lifecycle_and_ci_resolve_composition()
    test_json_output_conforms_to_published_schemas()
    test_junit_sarif_and_github_annotation_adapters()
    test_eval_scores_expected_and_forbidden_tool_calls()
    test_validate_and_gates_reject_incomplete_sessions()
    test_merge_combines_and_deduplicates_sessions()
    test_mtrace_export_import_round_trip()
    test_export_blocks_on_sensitive_content_lint_finding()
    test_mtrace_import_rejects_derived_fields_that_disagree_with_the_payload()
    test_export_otel_produces_deterministic_span_names_and_gates_on_redaction()
    test_vcr_cassette_import_produces_sane_correlated_exchanges()
    test_stats_and_search_surface_recorded_activity()
    test_bench_replays_recorded_session_as_load()
    test_replay_and_bench_warn_when_source_session_is_redacted()
    test_semantic_search_finds_drift_and_gates_on_redaction()
    test_oversized_unterminated_frame_is_capped()
    test_record_exits_when_the_server_exits_while_client_stdin_stays_open()
    test_streamable_http_proxy_records_json_and_sse()
    test_replay_http_reproduces_json_and_sse_sessions_and_fails_clearly_on_bad_envelope()
    test_replay_http_caps_oversized_json_response()
    test_modern_streamable_http_records_grouped_stateless_calls_and_replays_required_headers()
    test_streamable_http_partitions_recordings_by_logical_session()
    test_replay_and_bench_plan_mode()
    test_inspect_serves_read_only_session_ui()
    test_export_sensitive_content_lint_aborts_on_bearer_token()
    test_export_allow_sensitive_content_overrides_abort()
    test_canonical_demos_catch_their_scenarios()
    print("integration test passed")
