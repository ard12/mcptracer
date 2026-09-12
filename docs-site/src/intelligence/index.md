# Intelligence Layer

A **derived** layer over recorded sessions: tool-version identity, drift
detection, and query surfaces built from `sessions`/`messages`, never
touching the protocol hot path. Every table here is rebuildable from the
source-of-truth session data via `mcptracer index rebuild`.

```bash
mcptracer index rebuild       # (re)build the derived index from recorded sessions
mcptracer index facts --json  # query derived facts
mcptracer route <session-id>  # reasoned next-step recommendations, fact-cited
mcptracer optimize            # mine history for latency/assertion/bench suggestions
```
