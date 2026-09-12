# MCPTracer in CI

For this source preview, build the reviewed checkout and invoke the CLI directly.
The bundled release-installing composite Action is not an advertised install
path until public release assets have been built and verified.

## Verify the bundled workflow

From a checkout on a runner with Rust, a C toolchain, Python, and Bash:

```bash
cargo install --path crates/mcptracer-proxy --locked
bash examples/rug-pull-demo/run.sh
```

This runs the synthetic demo: the script succeeds only when the tool-contract
change is caught. It validates the fixture workflow; it does not test your server.
See [the tutorial](../examples/rug-pull-demo/README.md) for its full sequence.

## Gate your own server

Record a trusted baseline, validate capture integrity, then replay or record a
candidate against your test server. Compare the two sessions and assert the
expected contract. Replace the placeholders below with actual session IDs:

```bash
mcptracer validate <baseline-id>
mcptracer validate <candidate-id>
mcptracer diff <baseline-id> <candidate-id>
mcptracer assert <candidate-id> --golden <baseline-id>
```

`diff` returns 1 on a meaningful difference. Run commands as separate CI steps,
or use a shell configured to fail on errors, so a later success cannot mask an
earlier failure. Use an assertion spec to pin the full tool catalog; see
[assertions](spec/assertions.md) and the tutorial's `pin.toml`.

Replay executes real tool calls. Use isolated test targets and synthetic data.
Keep recordings and database files out of public logs and unrestricted artifacts.
Review the [security policy](../SECURITY.md) before sharing evidence.

## Repository checks

The included CI runs Rust tests, integration tests, linting, documentation,
SDK compatibility fixtures, and Rust 1.85 checks. Inspect results for the exact
commit you plan to consume. Hosted runs remain a release verification step.
