# About MCPTracer

![MCPTracer — Record. Replay. Catch regressions.](docs/assets/mcptracer-banner.png)

**Think Wireshark for MCP—with replay and regression tests.**

MCPTracer helps you inspect communication between AI agents and MCP servers:
tool calls, arguments, responses, timing, and failures. It also turns recorded
sessions into repeatable comparisons and CI checks, with evidence kept locally.

## Who it helps

- MCP server authors checking releases for regressions.
- Agent developers debugging integrations with MCP tools.
- Maintainers building automated checks around recorded workflows.
- Security reviewers examining changes in tool contracts and exchanges.

It observes MCP traffic routed through its recorder or HTTP proxy. It does not
observe ordinary editor, terminal, or AI coding activity outside those exchanges.

## License and commercial use

MCPTracer is source-available for noncommercial use under [LICENSE](LICENSE).
Commercial use requires prior written permission from ard12. Public projects
that incorporate or materially rely on MCPTracer must provide the credit specified
in the license. See [commercial permission and attribution](COMMERCIAL-LICENSE.md).

## About the maintainer

Maintained by [ard12](https://github.com/ard12).

<!-- Add your preferred name, a short personal introduction, and any links you want public here. -->

## What this project values

- Useful regression reports that point to what changed.
- Local recordings with explicit data-handling boundaries.
- Small, reviewable changes with tests at the affected behavior boundary.
- Clear limits and reproducible examples.

## Contribute

You can help with a small bug reproduction, a documentation improvement,
a compatibility case, or a focused patch. See [the contribution guide](CONTRIBUTING.md).

## Contact

| Contact | Details |
| --- | --- |
| Commercial licensing | [Request permission](https://github.com/ard12/mcptracer/issues/new?template=licensing_request.md) |
| General contact | [Open a GitHub issue](https://github.com/ard12/mcptracer/issues/new) — no direct email is published |
| Website | |
| Other | |

Please use [private vulnerability reporting](SECURITY.md) for security issues.
