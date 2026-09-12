"""Canonical MCPTracer demo: a latency regression caught before release.

A `search` tool responds quickly in the current release. Set
`MCPTRACER_DEMO_SLOW_MS=<n>` to simulate what an accidental N+1 query, a
newly-synchronous network call, or any other routine-looking change can do
to response time: the tool still returns the exact same result, just much
slower. This is what `mcptracer assert kind = "latency"` exists to catch as
an explicit, numeric release gate rather than something a human has to
notice by eye in a changelog.
"""

import json
import os
import sys
import time

SLOW_MS = float(os.environ.get("MCPTRACER_DEMO_SLOW_MS", "0"))


def send_message(message: dict) -> None:
    body = json.dumps(message, separators=(",", ":"))
    sys.stdout.buffer.write(body.encode("utf-8") + b"\n")
    sys.stdout.buffer.flush()


def read_message() -> dict | None:
    line = sys.stdin.buffer.readline()
    if not line:
        return None
    return json.loads(line)


def main() -> None:
    while True:
        message = read_message()
        if message is None:
            break

        method = message.get("method")
        msg_id = message.get("id")

        if method == "initialize":
            send_message(
                {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "result": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {"tools": {}},
                        "serverInfo": {
                            "name": "latency-regression-demo",
                            "version": "1.0.0",
                        },
                    },
                }
            )
        elif method == "notifications/initialized":
            continue
        elif method == "tools/list":
            send_message(
                {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "result": {
                        "tools": [
                            {
                                "name": "search",
                                "description": "Search the product catalog.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {"query": {"type": "string"}},
                                    "required": ["query"],
                                },
                            }
                        ]
                    },
                }
            )
        elif method == "tools/call":
            tool = message.get("params", {}).get("name")
            if tool == "search":
                if SLOW_MS > 0:
                    time.sleep(SLOW_MS / 1000.0)
                query = message.get("params", {}).get("arguments", {}).get("query", "")
                # The result is identical either way - only the time it took
                # to produce it changed.
                send_message(
                    {
                        "jsonrpc": "2.0",
                        "id": msg_id,
                        "result": {
                            "content": [
                                {
                                    "type": "text",
                                    "text": f"3 results for '{query}'.",
                                }
                            ],
                            "isError": False,
                        },
                    }
                )
            else:
                send_message(
                    {
                        "jsonrpc": "2.0",
                        "id": msg_id,
                        "error": {"code": -32601, "message": f"Unknown tool: {tool}"},
                    }
                )
        elif msg_id is not None:
            send_message(
                {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "error": {"code": -32601, "message": f"Method not found: {method}"},
                }
            )


if __name__ == "__main__":
    main()
