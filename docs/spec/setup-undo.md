# Client setup and undo

`mcptracer setup` can rewrite supported JSON client configuration files and
Codex TOML configuration. JSON setup accepts strict JSON and the supported
JSONC forms (line/block comments and trailing commas). The adjacent
`.mcptracer.bak` file contains the original configuration bytes; setup may
rewrite a JSONC config as strict JSON while it is wrapped.

`mcptracer setup <client> --undo --config <path>` validates the backup before
replacing the active config. Strict JSON and valid JSONC backups are accepted
for JSON clients; TOML backups must parse as TOML. Undo restores the backup's
original bytes without normalizing comments or formatting. Replacement uses a
temporary sibling and an atomic rename. A malformed backup is rejected before
the active config changes, and the backup remains available for recovery.

New setups also write `<config>.mcptracer.bak.meta`, containing a format version,
client identity and SHA-256 of the exact wrapped config. Undo refuses to replace
the active config if its bytes no longer match that digest. It also refuses
legacy backups without metadata and invalid or mismatched metadata when the active config differs from the backup. In those cases the active config and backup remain for inspection. `--undo --force` explicitly overrides missing/stale ownership information after the user reviews the active config and backup; it does not bypass backup syntax validation.

If the active config already equals the backup bytes, undo skips replacement and safely completes cleanup even when ownership metadata is missing. It removes metadata before the backup. If metadata removal fails, the backup stays available; if backup removal fails after metadata removal, a retry recognizes the already-restored bytes and finishes cleanup. A metadata write failure leaves the active file untouched; if config replacement fails after backup creation, the backup is retained for recovery.
