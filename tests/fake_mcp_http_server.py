"""Small Streamable HTTP MCP fixture for MCPTracer integration tests."""

import json
import queue
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    observed_headers: dict[str, str] = {}
    mrtr_initial_count = 0
    mrtr_expected_request_state = ""
    mrtr_live_retry_used_fresh_id = False
    subscription_queues: dict[str, tuple[queue.Queue[dict], dict]] = {}
    subscription_lock = threading.Lock()
    subscription_event_delivery_count = 0

    def log_message(self, _format: str, *_args: object) -> None:
        pass

    def do_GET(self) -> None:
        if self.path != "/observed":
            self.send_error(404)
            return
        self._send_json(
            {
                "headers": self.observed_headers,
                "mrtr_live_retry_used_fresh_id": self.mrtr_live_retry_used_fresh_id,
                "subscription_event_delivery_count": self.subscription_event_delivery_count,
            }
        )

    def do_DELETE(self) -> None:
        type(self).observed_headers = {
            name.lower(): value for name, value in self.headers.items()
        }
        self.send_response(204)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_POST(self) -> None:
        content_length = int(self.headers.get("Content-Length", "0"))
        request = json.loads(self.rfile.read(content_length))
        type(self).observed_headers = {
            name.lower(): value for name, value in self.headers.items()
        }

        if self.path == "/sse":
            self._send_sse()
            return

        if self.path == "/sse-echo":
            self._send_sse_echo(request.get("id"))
            return

        if self.path == "/plain":
            self._send_plain()
            return

        if self.path == "/big":
            self._send_big_json(request.get("id"))
            return

        if self.path == "/modern":
            self._send_modern_json(request)
            return

        self._send_json(
            {
                "jsonrpc": "2.0",
                "id": request.get("id"),
                "result": {"serverInfo": {"name": "fake-http-mcp"}},
            }
        )

    def _send_modern_json(self, request: dict) -> None:
        meta = request.get("params", {}).get("_meta", {})
        version = meta.get("io.modelcontextprotocol/protocolVersion")
        method = request.get("method")
        expected_name = None
        if method in ("tools/call", "prompts/get"):
            expected_name = request.get("params", {}).get("name")
        elif method == "resources/read":
            expected_name = request.get("params", {}).get("uri")

        valid = (
            version == "2026-07-28"
            and self.headers.get("MCP-Protocol-Version") == version
            and self.headers.get("Mcp-Method") == method
            and (expected_name is None or self.headers.get("Mcp-Name") == expected_name)
            and self.headers.get("Mcp-Session-Id") is None
        )
        if method == "tools/call" and expected_name == "echo":
            expected_text = request.get("params", {}).get("arguments", {}).get("text")
            valid = valid and isinstance(expected_text, str) and self.headers.get("Mcp-Param-Text") == expected_text
        if not valid:
            self._send_modern_error(request.get("id"))
            return

        if method == "subscriptions/listen":
            self._send_modern_subscription(request)
            return

        if method == "tools/call" and expected_name == "mrtr-echo":
            params = request.get("params", {})
            if "inputResponses" not in params:
                type(self).mrtr_initial_count += 1
                state = "source-state" if self.mrtr_initial_count == 1 else "live-state"
                type(self).mrtr_expected_request_state = state
                self._send_json_without_session(
                    {
                        "jsonrpc": "2.0",
                        "id": request.get("id"),
                        "result": {
                            "resultType": "input_required",
                            "inputRequests": {
                                "approval": {
                                    "method": "elicitation/create",
                                    "params": {"mode": "form", "message": "Approve replay"},
                                }
                            },
                            "requestState": state,
                        },
                    }
                )
                return

            responses = params.get("inputResponses")
            valid_retry = (
                isinstance(responses, dict)
                and set(responses) == {"approval"}
                and params.get("requestState") == self.mrtr_expected_request_state
            )
            if not valid_retry:
                self._send_modern_error(request.get("id"))
                return
            if self.mrtr_expected_request_state == "live-state" and request.get("id") != "mrtr-2":
                type(self).mrtr_live_retry_used_fresh_id = True
            self._send_json_without_session(
                {
                    "jsonrpc": "2.0",
                    "id": request.get("id"),
                    "result": {"resultType": "complete", "ok": True},
                }
            )
            return
        if method == "tools/list":
            self._send_json_without_session(
                {
                    "jsonrpc": "2.0",
                    "id": request.get("id"),
                    "result": {
                        "resultType": "complete",
                        "tools": [
                            {
                                "name": "echo",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "text": {"type": "string", "x-mcp-header": "Text"}
                                    },
                                },
                            },
                            {
                                "name": "mrtr-echo",
                                "inputSchema": {"type": "object", "properties": {}},
                            }
                        ],
                    },
                }
            )
            return

        if method == "tools/call" and expected_name == "echo":
            self._publish_tools_list_changed()
        self._send_json_without_session(
            {
                "jsonrpc": "2.0",
                "id": request.get("id"),
                "result": {"resultType": "complete", "ok": True},
            }
        )

    def _send_modern_subscription(self, request: dict) -> None:
        request_id = request.get("id")
        notifications = request.get("params", {}).get("notifications")
        if not isinstance(request_id, (str, int)) or not isinstance(notifications, dict):
            self._send_modern_error(request_id)
            return
        key = json.dumps(request_id, separators=(",", ":"))
        events: queue.Queue[dict] = queue.Queue()
        with self.subscription_lock:
            self.subscription_queues[key] = (events, notifications)

        acknowledgement = {
            "jsonrpc": "2.0",
            "method": "notifications/subscriptions/acknowledged",
            "params": {
                "notifications": {
                    name: value
                    for name, value in notifications.items()
                    if name == "toolsListChanged" and value is True
                },
                "_meta": {"io.modelcontextprotocol/subscriptionId": request_id},
            },
        }
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        try:
            self._write_sse_json(acknowledgement)
            while True:
                try:
                    event = events.get(timeout=0.05)
                except queue.Empty:
                    self.wfile.write(b": keepalive\n\n")
                    self.wfile.flush()
                    continue
                self._write_sse_json(event)
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            with self.subscription_lock:
                if self.subscription_queues.get(key, (None, None))[0] is events:
                    self.subscription_queues.pop(key, None)

    def _publish_tools_list_changed(self) -> None:
        with self.subscription_lock:
            subscribers = list(self.subscription_queues.items())
        for key, (events, notifications) in subscribers:
            if notifications.get("toolsListChanged") is not True:
                continue
            request_id = json.loads(key)
            events.put(
                {
                    "jsonrpc": "2.0",
                    "method": "notifications/tools/list_changed",
                    "params": {
                        "_meta": {
                            "io.modelcontextprotocol/subscriptionId": request_id,
                        }
                    },
                }
            )
            type(self).subscription_event_delivery_count += 1

    def _write_sse_json(self, payload: dict) -> None:
        event = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        self.wfile.write(b"data: " + event + b"\n\n")
        self.wfile.flush()

    def _send_modern_error(self, request_id: object) -> None:
        payload = {
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": -32020, "message": "Header mismatch"},
        }
        body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        self.send_response(400)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def _send_json_without_session(self, payload: dict) -> None:
        body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def _send_json(self, payload: dict) -> None:
        body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Mcp-Session-Id", "upstream-session")
        self.send_header("MCP-Protocol-Version", "2025-06-18")
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def _send_big_json(self, request_id: object) -> None:
        # One byte past MCPTracer's 8 MiB capture cap (`MAX_FRAME_BYTES` in
        # `crates/mcptracer-proxy/src/session_writer.rs`), so a caller
        # exercising this route proves the cap actually bites rather than
        # just staying comfortably under it.
        padding = "x" * (8 * 1024 * 1024 + 1)
        body = json.dumps(
            {"jsonrpc": "2.0", "id": request_id, "result": {"padding": padding}},
            separators=(",", ":"),
        ).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Mcp-Session-Id", "upstream-session")
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def _send_plain(self) -> None:
        body = b"not an MCP envelope"
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def _send_sse_echo(self, request_id: object) -> None:
        # Unlike `_send_sse` (fixed, request-independent event ids, used to
        # exercise multi-event SSE decoding), this echoes the request's own
        # id in a single response event so the resulting recording
        # correlates cleanly (no unanswered/orphan exchange) and is eligible
        # for `require_healthy_session`-gated commands like `replay-http`.
        #
        # The write is split with a short pause in the middle, like `_send_sse`
        # below, so a concurrent plain-JSON response on another connection has
        # a real window to complete while this one is still streaming - this
        # is what lets a concurrency test assert genuine interleaving rather
        # than two responses that merely happened to be requested close together.
        event = json.dumps(
            {"jsonrpc": "2.0", "id": request_id, "result": {"ok": True}},
            separators=(",", ":"),
        ).encode("utf-8")
        events = b"data: " + event + b"\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(events)))
        self.send_header("Mcp-Session-Id", "upstream-session")
        self.end_headers()
        midpoint = len(events) // 2
        self.wfile.write(events[:midpoint])
        self.wfile.flush()
        time.sleep(0.02)
        self.wfile.write(events[midpoint:])
        self.wfile.flush()

    def _send_sse(self) -> None:
        events = (
            b"id: 1\n"
            b"data: {\"jsonrpc\":\"2.0\",\"id\":\"sse-1\",\"result\":{\"ok\":true}}\n\n"
            b"data: {\"jsonrpc\":\"2.0\",\"id\":\"sse-2\",\"result\":{\"ok\":true}}\n\n"
        )
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(events)))
        self.send_header("Mcp-Session-Id", "upstream-session")
        self.end_headers()
        midpoint = len(events) // 2
        self.wfile.write(events[:midpoint])
        self.wfile.flush()
        time.sleep(0.02)
        self.wfile.write(events[midpoint:])
        self.wfile.flush()


def main() -> None:
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
    server.serve_forever()


if __name__ == "__main__":
    main()
