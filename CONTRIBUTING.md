# Contributing to MCPTracer

You do not need access to any private repository to contribute. Small fixes,
clear reproductions, documentation improvements, and compatibility cases are welcome.

## Choose a starting point

- **Found a bug?** Open an issue with the command, expected and actual behavior,
  operating system, and commit or version. Prefer a tiny fake MCP server that
  reproduces the problem. Remove credentials and real customer data.
- **Improving docs?** Fix the page and check its links. Small documentation fixes
  can go straight to a pull request.
- **Changing behavior?** Open an issue first for a larger change so the intended
  behavior and owning crate can be agreed before implementation.

## Set up a checkout

Fork the public repository, clone your fork, and create a branch for your change.
You need Rust 1.85+ and a working C toolchain for bundled SQLite; Windows users
can use MSVC Build Tools with the Windows SDK. Python 3.10+ runs the integration
suite. Documentation builds also use mdBook 0.5; generator scripts use Bash.

```bash
cargo build --workspace --all-features --locked
cargo test --workspace --all-features --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
python tests/test_proxy_integration.py
```

For documentation changes, run `mdbook build docs-site` and check relative links.
If CLI commands or flags change, regenerate their references:

```bash
bash scripts/generate-cli-reference.sh
bash scripts/generate-capability-reference.sh
```

Run checks appropriate to the change and list their results in the pull request.
CI performs the broader release checks; explain any checks you could not run.

## Find the right crate

| Area | Owning crate |
| --- | --- |
| JSON-RPC framing and transports | `mcptracer-protocol` |
| SQLite, migrations, and stored artifacts | `mcptracer-storage` |
| Redaction policy | `mcptracer-redact` |
| Correlation, diff, assertions, and matching | `mcptracer-model` |
| Derived local facts and indexes | `mcptracer-intel` |
| CLI, subprocesses, and IO orchestration | `mcptracer-proxy` |

Read the [architecture overview](docs/architecture/overview.md) and the relevant
behavior contract in `docs/spec/` before changing a data boundary.

## Ground rules

- stdout is protocol traffic during recording; diagnostics go to stderr.
- Forwarding must not depend on recording success or modify forwarded bytes.
- Keep SQL behind storage and framing inside the protocol crate.
- Do not share a SQLite connection across async tasks.
- Sharing and export paths must apply a redaction policy.
- Add regression tests at the boundary where behavior changes. Use synthetic
  fixtures, never private recordings, live credentials, or customer records.

## Send a pull request

1. Keep the patch focused on one problem.
2. Explain the trigger and the resulting behavior; include a reproduction where useful.
3. List the checks you ran and any limitations.
4. Update affected documentation and preserve existing copyright notices.

## How public patches are integrated

Public releases are generated from an authoritative source repository. Maintainers
review your public patch, preserve its author and originating commit in the
integration record, apply the accepted change to that source, and regenerate a
verified public snapshot. The snapshot is linked back to the issue or pull request.
A public pull request may therefore be closed in favor of the generated commit;
you do not need private access to participate.

## Contribution license

Submit only material you own or are authorized to contribute. Proposed patches
are offered under [LICENSE](LICENSE); preserve the attribution in [NOTICE](NOTICE).

The project also supports separately authorized commercial use. Before accepting
a contribution, maintainers must obtain a separate, explicit written contributor
grant allowing ard12 to include and sublicense that contribution in commercial
distributions, or exclude it from those distributions. A pull request alone is
not treated as a copyright assignment or an implied commercial relicensing grant.
Record that permission with the contribution and retain third-party notices.
The permission must expressly cover distribution and sublicensing by ard12
under separately negotiated commercial terms. Maintainers may decline or hold
patches where those rights are unavailable or unclear; contributors retain
ownership of their work unless a separate agreement says otherwise.
See [commercial permission](COMMERCIAL-LICENSE.md).

## Security and contact

Report vulnerabilities privately using [SECURITY.md](SECURITY.md), rather than
opening a public issue. Maintainer and contact fields are in [ABOUT.md](ABOUT.md).
