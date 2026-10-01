import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, rmSync, statSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";
import { Client } from "@modelcontextprotocol/client";
import { StdioClientTransport } from "@modelcontextprotocol/client/stdio";
import { McpServer } from "@modelcontextprotocol/server";
import { serveStdio } from "@modelcontextprotocol/server/stdio";
import * as z from "zod/v4";

const script = fileURLToPath(import.meta.url);
const root = path.resolve(path.dirname(script), "../../../../");

async function runServer(marker) {
  const server = new McpServer({ name: "mcptracer-ts-cancellation", version: "1.0.0" });
  server.registerTool(
    "wait_for_cancel",
    { description: "Wait until the client cancels this call.", inputSchema: z.object({}) },
    async (_args, ctx) => {
      const progressToken = ctx.mcpReq._meta?.progressToken;
      assert.notEqual(progressToken, undefined, "client did not request progress");
      const cancelled = new Promise((resolve) => {
        const onAbort = () => {
          writeFileSync(marker, "cancelled", { mode: 0o600 });
          resolve();
        };
        if (ctx.mcpReq.signal.aborted) return onAbort();
        ctx.mcpReq.signal.addEventListener("abort", onAbort, { once: true });
      });
      await ctx.mcpReq.notify({
        method: "notifications/progress",
        params: { progressToken, progress: 1, total: 2, message: "handler-started" },
      });
      await cancelled;
      return { content: [{ type: "text", text: "cancelled" }] };
    },
  );
  await serveStdio(() => server);
}

function cli(binary, db, args, timeout = 20_000) {
  return execFileSync(binary, ["--db", db, ...args], {
    cwd: root,
    encoding: "utf8",
    timeout,
    stdio: ["ignore", "pipe", "pipe"],
  });
}

function latestSessionId(binary, db) {
  const sessions = JSON.parse(cli(binary, db, ["sessions", "list", "--json"]));
  assert.ok(sessions.length, "no captured sessions");
  return sessions.at(-1).id;
}

function verifyCancelled(binary, db, id) {
  const health = JSON.parse(cli(binary, db, ["validate", id, "--json"]));
  assert.equal(health.healthy, true);
  const calls = JSON.parse(cli(binary, db, ["sessions", "show", id, "--calls", "--json"]));
  const exchange = calls.exchanges.find((item) => item.method === "tools/call");
  assert.equal(exchange?.status, "cancelled");
  assert.equal(calls.stats.cancelled, 1);
  const messages = JSON.parse(cli(binary, db, ["sessions", "show", id, "--full", "--json"]));
  assert.ok(messages.some((item) => item.method === "notifications/cancelled"));
}

async function waitForFile(marker, timeout = 3_000) {
  const stop = Date.now() + timeout;
  while (Date.now() < stop) {
    try {
      statSync(marker);
      return;
    } catch {
      await delay(20);
    }
  }
  throw new Error(`server cancellation cleanup did not create marker: ${marker}`);
}

async function capture(binary, db, self, marker) {
  let resolveProgress;
  const progress = new Promise((resolve) => { resolveProgress = resolve; });
  const transport = new StdioClientTransport({
    command: binary,
    args: ["--db", db, "record", "--client", "typescript-v2-cancellation", "--", process.execPath, self, "server", marker],
    cwd: root,
    stderr: "pipe",
  });
  const client = new Client({ name: "mcptracer-ts-cancellation-client", version: "1.0.0" });
  await client.connect(transport);
  const controller = new AbortController();
  const call = client.callTool({ name: "wait_for_cancel", arguments: {} }, {
    signal: controller.signal,
    onprogress: () => resolveProgress(),
    timeout: 10_000,
  });
  let timer;
  const timeout = new Promise((_, reject) => { timer = setTimeout(() => reject(new Error("timed out waiting for progress")), 10_000); });
  await Promise.race([progress, timeout]);
  clearTimeout(timer);
  await delay(50);
  controller.abort();
  const outcome = await call.then((value) => ({ value }), (error) => ({ error }));
  if (!outcome.error) throw new Error(`call resolved instead of aborting: ${JSON.stringify(outcome.value)}`);
  assert.match(String(outcome.error), /AbortError|CanceledError/, `unexpected cancellation error: ${outcome.error}`);
  await client.ping();
  await client.close();
  return transport;
}

async function runSmoke() {
  const binary = process.env.MCPTRACER_BIN;
  assert.ok(binary, "MCPTRACER_BIN must name the MCPTracer executable");
  const dbDir = mkdtempSync(path.join(os.tmpdir(), "mcptracer-ts-sdk-cancel-"));
  try {
    const db = path.join(dbDir, "sessions.db");
    const sourceMarker = path.join(dbDir, "source-cancelled.txt");
    await capture(binary, db, script, sourceMarker);
    const sourceId = latestSessionId(binary, db);
    verifyCancelled(binary, db, sourceId);
    await waitForFile(sourceMarker);

    const replayMarker = path.join(dbDir, "replay-cancelled.txt");
    const replay = spawn(binary, ["--db", db, "replay", sourceId, "--timing", "realtime", "--request-timeout", "3000", "--i-understand-side-effects", "--", process.execPath, script, "server", replayMarker], { cwd: root, stdio: ["ignore", "pipe", "pipe"] });
    let replayStdout = "";
    let replayStderr = "";
    replay.stdout.setEncoding("utf8").on("data", (chunk) => { replayStdout += chunk; });
    replay.stderr.setEncoding("utf8").on("data", (chunk) => { replayStderr += chunk; });
    const exitCode = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => { replay.kill(); reject(new Error("replay timed out")); }, 20_000);
      replay.on("error", (error) => { clearTimeout(timer); reject(error); });
      replay.on("close", (code) => { clearTimeout(timer); resolve(code); });
    });
    assert.equal(exitCode, 0, `replay failed: ${replayStdout}\n${replayStderr}`);
    const match = replayStderr.match(/replaying session \S+ as (\S+) ->/);
    assert.ok(match, `could not parse replay ID: ${replayStderr}`);
    const replayId = match[1];
    try { await waitForFile(replayMarker); } catch (error) {
      const calls = cli(binary, db, ["sessions", "show", replayId, "--calls", "--json"]);
      throw new Error(`${error}; replay stderr=${replayStderr}; calls=${calls}`);
    }
    verifyCancelled(binary, db, replayId);
    execFileSync(binary, ["--db", db, "diff", sourceId, replayId, "--ignore-latency"], { cwd: root, stdio: "pipe", timeout: 20_000 });
    console.log("PASS: TypeScript SDK v2.2.0 sent stdio cancellation; capture and replay validate cancelled; diff is clean");
  } finally {
    try { rmSync(dbDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }); } catch { /* Windows may retain SQLite sidecars briefly after child shutdown. */ }
  }
}

if (process.argv[2] === "server") {
  await runServer(process.argv[3]);
} else {
  await runSmoke();
}
