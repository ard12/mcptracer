# Bench

`mcptracer bench` turns a recorded MCP session into a small load/soak test.
Instead of writing synthetic scripts by hand, it reuses client-originated
traffic that was already captured by `record`. See
[`replay-plan.md`](replay-plan.md) for `--plan`, `--allow-tool`, and
`--deny-tool`, shared with `replay`.

## Research Inputs

- The MCP stdio transport is newline-delimited JSON-RPC and stdout must contain
  only valid MCP messages, so `bench` must not forward target-server protocol
  traffic to stdout. See the official transport spec:
  <https://modelcontextprotocol.io/specification/2025-06-18/basic/transports>.
- k6-style load testing shows the useful reporting surface: throughput,
  percentiles, error rate, and CI-failing thresholds. `bench` starts with
  throughput + p50/p90/p95/p99 + error/unanswered counts and can add thresholds
  later. See Grafana k6 thresholds:
  <https://grafana.com/docs/k6/latest/using-k6/thresholds/>.
- MCP benchmark papers such as MCP-Atlas and MCPMark focus on model/tool-use
  competence. `bench` is intentionally different: it tests server performance
  under real captured traffic, not model quality.

## CLI

```bash
mcptracer bench <session-id> --repeat 20 --concurrency 4 -- your-mcp-server
mcptracer bench <session-id> --repeat 100 --concurrency 16 --json -- your-mcp-server
```

## V1 Behavior

- Loads one recorded source session.
- Extracts client-originated requests and notifications, forcing `initialize`
  first like `replay`.
- Runs `N` session iterations with at most `M` concurrent workers.
- Each iteration starts a fresh server subprocess.
- Does not persist each run by default.
- Prints aggregate latency percentiles, throughput, errors, and unanswered
  requests.
- Exits non-zero if any server response is an error or any request goes
  unanswered.

## Intentional Limits

- V1 is stdio-only.
- V1 does not model long-lived shared server state because each iteration uses a
  fresh subprocess. A later `--shared-server` mode can exercise stateful servers.
- V1 has no threshold language. A future version should add k6-style gates such
  as `--p95-under-ms` and `--error-rate-under`.
