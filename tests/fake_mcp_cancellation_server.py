"""Minimal stdio server that exits after receiving cancellation."""

import json
import sys


def send(message: dict) -> None:
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


pending_id = None
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "cancel-fixture", "version": "1"}}})
    elif method == "tools/call":
        pending_id = message["id"]
    elif method == "notifications/cancelled" and message.get("params", {}).get("requestId") == pending_id:
        break