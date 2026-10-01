"""Cancellation target for the real Python SDK v2 stdio smoke."""
import sys
import anyio
from pathlib import Path
from mcp.server import MCPServer
from mcp.server.mcpserver import Context

server = MCPServer("mcptracer-python-v2-cancellation")

@server.tool()
async def wait_for_cancel(ctx: Context) -> str:
    await ctx.report_progress(1, 2, "handler-started")
    try:
        await anyio.sleep_forever()
    except anyio.get_cancelled_exc_class():
        marker = sys.argv[1] if len(sys.argv) > 1 else None
        if marker:
            Path(marker).write_text("cancelled", encoding="utf-8")
        raise

if __name__ == "__main__":
    import asyncio
    asyncio.run(server.run_stdio_async())
