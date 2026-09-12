## What this changes

Briefly describe the change and the owning crate.

## Why

Link the issue or explain the motivation.

## Verification

- [ ] `cargo test --all`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all -- -D warnings`
- [ ] `python tests/test_proxy_integration.py`

## Checklist

- [ ] Change is scoped to the owning crate (protocol / storage / proxy / redact).
- [ ] No logs written to stdout during `record`.
- [ ] Forwarding still works even if recording fails.
- [ ] Tests added at the same boundary as the change.
- [ ] No sharing/export path added without redaction.
