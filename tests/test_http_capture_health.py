"""Bounded real HTTP capture failures must agree in CLI and inspector."""
import http.client
import json
from pathlib import Path
import re
import sqlite3
import subprocess
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from test_proxy_integration import build_recorder_binary, free_port, make_workdir, stop_process, wait_for_port


BODY = b'{"jsonrpc":"2.0","id":1,"result":{"content":[]}}'
MALFORMED = b'{"jsonrpc":"2.0","id":1,"result":'


class Fixture(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        payload = MALFORMED if self.path == "/malformed" else BODY
        sse = self.path.startswith("/sse-")
        if sse:
            payload = b"data: " + BODY + (b"\n\n" if self.path == "/sse-healthy" else b"\n")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream" if sse else "application/json")
        self.send_header("X-Dummy", "unchanged")
        self.send_header("Content-Length", str(len(payload) + (12 if self.path == "/truncated" else 0)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)
        self.wfile.flush()
        if self.path == "/truncated":
            # A gate, not a timing sleep: prove the valid prefix forwards
            # while upstream remains open, then inject premature EOF.
            self.server.release_truncated_end.wait(timeout=3)
        self.close_connection = True


class HttpCaptureHealth(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.repo = Path(__file__).resolve().parent.parent
        cls.binary = build_recorder_binary(cls.repo)

    def test_malformed_and_truncated_capture_fail_cli_and_inspector_while_bytes_forward(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
        server.daemon_threads = True
        server.release_truncated_end = threading.Event()
        thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)
        thread.start()
        try:
            for case in ("healthy", "malformed", "truncated", "sse-healthy", "sse-undelimited"):
                with self.subTest(case=case):
                    self.check_capture(server, case)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)
            self.assertFalse(thread.is_alive())

    def check_capture(self, upstream, case):
        upstream_port = upstream.server_address[1]
        work = make_workdir(self.repo)
        db = work / "capture.db"
        proxy_port = free_port()
        with (work / "recorder.stderr").open("wb") as diagnostics:
            recorder = subprocess.Popen([
                str(self.binary), "--db", str(db), "record-http",
                "--listen", f"127.0.0.1:{proxy_port}",
                "--target", f"http://127.0.0.1:{upstream_port}/", "--redact", "default",
                "--max-recording-sessions", "2", "--max-capture-exchanges", "2",
            ], cwd=self.repo, stdout=subprocess.DEVNULL, stderr=diagnostics)
            try:
                wait_for_port(proxy_port, recorder)
                connection = http.client.HTTPConnection("127.0.0.1", proxy_port, timeout=3)
                try:
                    connection.request("POST", f"/{case}", body=b'{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"dummy"}}',
                                       headers={"Content-Type": "application/json"})
                    response = connection.getresponse()
                    self.assertEqual(response.status, 200)
                    self.assertEqual(response.getheader("X-Dummy"), "unchanged")
                    if case == "truncated":
                        prefix = response.read(len(BODY))
                        self.assertEqual(prefix, BODY)
                        upstream.release_truncated_end.set()
                        with self.assertRaises(http.client.IncompleteRead) as caught:
                            response.read()
                        self.assertEqual(prefix + caught.exception.partial, BODY)
                    else:
                        expected = MALFORMED if case == "malformed" else BODY
                        if case.startswith("sse-"):
                            expected = b"data: " + BODY + (b"\n\n" if case == "sse-healthy" else b"\n")
                        self.assertEqual(response.read(), expected)
                finally:
                    if case == "truncated":
                        upstream.release_truncated_end.set()
                    connection.close()
                deadline = time.monotonic() + 5
                row = None
                while time.monotonic() < deadline:
                    with sqlite3.connect(db) as connection:
                        row = connection.execute("SELECT id, ended_at, dropped_messages FROM sessions").fetchone()
                    if row and row[1] is not None:
                        break
                    time.sleep(0.01)
                self.assertIsNotNone(row)
                self.assertIsNotNone(row[1], "one-shot capture must finalize")
            finally:
                stop_process(recorder)
        session_id, _, drops = row
        healthy = case in ("healthy", "sse-healthy")
        self.assertEqual(drops, 0 if healthy else 1)
        with sqlite3.connect(db) as connection:
            count = connection.execute("SELECT COUNT(*) FROM messages WHERE session_id=?", (session_id,)).fetchone()[0]
        self.assertEqual(count, 2 if healthy else 1, "failed response prefixes must not become complete messages")
        result = subprocess.run([str(self.binary), "--db", str(db), "validate", session_id, "--json"],
                                cwd=self.repo, capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0 if healthy else 1, result.stderr.decode(errors="replace"))
        report = json.loads(result.stdout)
        self.assertEqual(report["healthy"], healthy)
        self.check_inspector(work, db, session_id, report)

    def check_inspector(self, work, db, session_id, report):
        port = free_port()
        stderr = work / "inspector.stderr"
        with stderr.open("wb") as diagnostics:
            inspector = subprocess.Popen([str(self.binary), "--db", str(db), "inspect", "--listen", f"127.0.0.1:{port}"],
                                         cwd=self.repo, stdout=subprocess.DEVNULL, stderr=diagnostics)
            try:
                wait_for_port(port, inspector)
                deadline = time.monotonic() + 3
                match = None
                while time.monotonic() < deadline:
                    match = re.search(r"/#token=([a-f0-9]{32})", stderr.read_text())
                    if match:
                        break
                    time.sleep(0.01)
                self.assertIsNotNone(match)

                def get(path):
                    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
                    try:
                        connection.request("GET", path, headers={"Authorization": f"Bearer {match.group(1)}"})
                        response = connection.getresponse()
                        return response.status, response.read()
                    finally:
                        connection.close()

                status, payload = get(f"/api/sessions/{session_id}")
                self.assertEqual(status, 200)
                detail = json.loads(payload)
                self.assertEqual(detail["capture_health"], {"healthy": report["healthy"], "issues": report["issues"]})
                status, _ = get(f"/api/diff/{session_id}/{session_id}")
                self.assertEqual(status, 200 if report["healthy"] else 400)
            finally:
                stop_process(inspector)


if __name__ == "__main__":
    unittest.main()
