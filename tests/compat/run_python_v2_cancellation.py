"""Record and replay a real Python SDK v2.2.0 stdio cancellation."""
from __future__ import annotations
import anyio
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

repo = Path(__file__).resolve().parents[2]
site = os.environ["MCPTRACER_COMPAT_PY_V2_SITE"]
sys.path.insert(0, site)
from mcp.client.session import ClientSession
from mcp.client.stdio import StdioServerParameters, stdio_client

binary = Path(os.environ["MCPTRACER_BIN"]).resolve()
python = os.environ.get("MCPTRACER_COMPAT_PY_V2") or sys.executable
server = repo / "tests" / "compat" / "servers" / "py-modern-stdio" / "cancellation_server.py"


def target_command() -> list[str]:
    marker = os.environ["MCPTRACER_CANCEL_MARKER"]
    source = (
        "import runpy,sys;"
        f"sys.path.insert(0,{site!r});"
        f"sys.argv=[{str(server)!r},{marker!r}];"
        f"runpy.run_path({str(server)!r},run_name='__main__')"
    )
    return [python, "-c", source]


def cli(db: Path, *args: str, timeout: float = 20) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(binary), "--db", str(db), *args], cwd=repo, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout, check=False,
    )


async def capture_cancel(db: Path) -> None:
    params = StdioServerParameters(
        command=str(binary),
        args=["--db", str(db), "record", "--client", "python-v2-cancellation",
              "--", *target_command()],
    )
    progress_seen = anyio.Event()
    cancel_scope: anyio.CancelScope | None = None
    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.discover()

            async def on_progress(progress: float, total: float | None, message: str | None) -> None:
                progress_seen.set()
                await anyio.sleep(0.05)
                assert cancel_scope is not None
                cancel_scope.cancel()

            with anyio.CancelScope() as scope:
                cancel_scope = scope
                await session.call_tool("wait_for_cancel", {}, progress_callback=on_progress)
                raise AssertionError("the long-running SDK call returned instead of cancelling")
    assert progress_seen.is_set(), "the real server did not report progress before cancellation"


def main() -> int:
    if not binary.is_file():
        raise SystemExit(f"MCPTRACER_BIN is not a file: {binary}")
    with tempfile.TemporaryDirectory(prefix="mcptracer-sdk-cancel-") as temp:
        root = Path(temp)
        db = root / "sessions.db"
        marker = root / "source-cancelled.txt"
        os.environ["MCPTRACER_CANCEL_MARKER"] = str(marker)
        anyio.run(capture_cancel, db)
        deadline = time.monotonic() + 3
        while not marker.exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        assert marker.exists(), "SDK cancellation did not interrupt the real server handler"

        import sqlite3
        conn = sqlite3.connect(db)
        (source_id,) = conn.execute("SELECT id FROM sessions").fetchone()
        conn.close()
        health = cli(db, "validate", source_id, "--json")
        assert health.returncode == 0, health.stderr
        assert json.loads(health.stdout)["healthy"] is True
        source = cli(db, "sessions", "show", source_id, "--calls", "--json")
        assert source.returncode == 0, source.stderr
        model = json.loads(source.stdout)
        exchange = next(item for item in model["exchanges"] if item["method"] == "tools/call")
        assert exchange["status"] == "cancelled", exchange
        assert model["stats"]["cancelled"] == 1, model["stats"]
        raw = cli(db, "sessions", "show", source_id, "--full", "--json")
        assert raw.returncode == 0, raw.stderr
        assert any(item.get("method") == "notifications/cancelled" for item in json.loads(raw.stdout))

        replay_marker = root / "replay-cancelled.txt"
        os.environ["MCPTRACER_CANCEL_MARKER"] = str(replay_marker)
        replay = cli(
            db, "replay", source_id, "--timing", "realtime", "--request-timeout", "3000",
            "--i-understand-side-effects", "--", *target_command(), timeout=15,
        )
        assert replay.returncode == 0, replay.stderr
        match = re.search(r"replaying session \S+ as (\S+) ->", replay.stderr)
        assert match, replay.stderr
        replay_id = match.group(1)
        replay_health = cli(db, "validate", replay_id, "--json")
        assert replay_health.returncode == 0, replay_health.stderr
        assert json.loads(replay_health.stdout)["healthy"] is True
        replay_source = cli(db, "sessions", "show", replay_id, "--calls", "--json")
        assert replay_source.returncode == 0, replay_source.stderr
        replay_model = json.loads(replay_source.stdout)
        replay_exchange = next(item for item in replay_model["exchanges"] if item["method"] == "tools/call")
        assert replay_exchange["status"] == "cancelled", replay_exchange
        assert replay_model["stats"]["cancelled"] == 1, replay_model["stats"]
        diff = cli(db, "diff", source_id, replay_id, "--ignore-latency")
        assert diff.returncode == 0, diff.stderr + diff.stdout
        print("PASS: Python SDK 2.2.0 sent stdio cancellation; capture and replay validate cancelled; diff is clean")
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
