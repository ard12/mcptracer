# Onboarding: your first recording

This walks you from a clean checkout to a recorded MCP tool call and one
useful comparison between two sessions. Each phase has a time estimate so you
can tell where you are; they add up to roughly ten minutes. That ten minutes
is a target for how long this *should* take, not a measurement — nobody has
timed a run of this exact guide yet, so treat the estimates as a guide to
pacing, not a promise.

If you get stuck, skip to [Troubleshooting](#troubleshooting) before asking
the maintainer — most of what goes wrong here has a one-line fix.

## 1. Install (a few minutes, mostly compiling)

No release has been published yet. npm, PyPI, crates.io, and Homebrew
packages are all unpublished, so **building from source is the only path
that works today.** Follow the "Build from source" section of
[Installation](../docs-site/src/installation.md) — skip the parts of that
page about downloading a release archive or verifying a checksum, since
there is nothing published to download. You'll need Rust 1.85+ and a working
C toolchain (MSVC Build Tools and the Windows SDK, on Windows) because a
bundled SQLite gets compiled in.

Confirm it worked before moving on:

```bash
mcptracer --version
```

If that fails with a "command not found" error, see
[Troubleshooting](#mcptracer-command-not-found).

## 2. Wrap your client (2 minutes) — the step people get wrong

`mcptracer setup` edits an already-configured MCP client so that its stdio
servers get launched through MCPTracer's recording proxy instead of directly.
Pick your client and run one of:

```bash
mcptracer setup claude-desktop
mcptracer setup cursor
mcptracer setup codex
mcptracer setup vscode
```

Here is exactly what each one edits, verified against the client-setup code
rather than guessed:

| Client | Windows | macOS | Linux | Servers key |
| --- | --- | --- | --- | --- |
| `claude-desktop` | `%APPDATA%\Claude\claude_desktop_config.json` | `~/Library/Application Support/Claude/claude_desktop_config.json` | `$XDG_CONFIG_HOME/Claude/claude_desktop_config.json` (falls back to `~/.config/...`) | `mcpServers` |
| `cursor` | `~/.cursor/mcp.json` (same path on every OS) | same | same | `mcpServers` |
| `codex` | `~/.codex/config.toml` (same path on every OS) | same | same | `[mcp_servers]` TOML table |
| `vscode` | `%APPDATA%\Code\User\mcp.json` | `~/Library/Application Support/Code/User/mcp.json` | `$XDG_CONFIG_HOME/Code/User/mcp.json` (falls back to `~/.config/...`) | `servers` |

A few things worth knowing before you run it:

- **Only stdio servers are touched.** An entry that's already HTTP or SSE is
  left exactly as it was — there's nothing for `setup` to wrap, and no
  recording will happen for it. If everything in your config is HTTP/SSE,
  `setup` will report that it found no unwrapped stdio servers and change
  nothing.
- **The `command` it writes is an absolute path**, not the bare word
  `mcptracer`. Desktop apps are launched by the OS with a minimal `PATH` that
  usually excludes `~/.local/bin` or your cargo bin directory, so a bare
  `mcptracer` would silently fail to start every wrapped server. `setup`
  resolves the actual running binary's path so this doesn't happen to you.
- **A backup is written next to the config**, named
  `<original-filename>.mcptracer.bak`, before anything is changed.
  `mcptracer setup <client> --undo` restores it byte-for-byte and deletes the
  backup. Run `mcptracer setup --undo` with no client to restore every
  supported client that currently has a backup.
- **Rewriting normalizes formatting.** VS Code's `mcp.json` may contain `//`
  comments and trailing commas; MCPTracer reads that correctly but rewrites
  the file as strict JSON, so comments don't survive the rewrite. Codex's
  `config.toml` is reformatted the same way, and comments are dropped there
  too. In both cases your original, comments included, is preserved
  untouched in the `.mcptracer.bak` backup.
- **`--config <path>`** points `setup` at a config file other than the
  default for that client — useful for a non-default profile or a test
  fixture.

After a successful wrap, `setup` prints the paths it touched: the config
file, the `.mcptracer.bak` backup, and the recordings database it will write
sessions to, along with a reminder to restart your client and the exact
`--undo` command to reverse everything it just did. Read that output — it's
the fastest way to confirm the wrap did what you expect, and this guide
deliberately doesn't reproduce it verbatim since the exact wording is the
tool's to show you, not paraphrased text that might drift out of sync with it.

**Now fully quit and reopen your client.** Not "reload window" — a full
restart. MCP clients read their server config once, at launch; a client
that's already running has no reason to notice the file changed under it.
This is the single most common thing to get wrong in this whole guide.

## 3. Do something in your editor (2 minutes)

Wrapping a server records nothing by itself — MCPTracer only sees traffic
that actually crosses the wire. Open your client's chat or agent panel and
make it call one of the tools from the server you just wrapped. Concretely:
ask it to do something that requires that specific server — list a directory
if you wrapped a filesystem server, fetch a page if you wrapped a web-fetch
server, and so on. If you're not sure what tools are available, most clients
have a tool or MCP-server picker in their settings UI that lists them.

Any one successful tool call is enough to produce something to look at in
the next step. This step depends on your specific client's UI, which this
guide has not been run against end-to-end — treat it as unverified, and
fall back to [Troubleshooting](#sessions-list-is-empty) if nothing shows up
afterward.

## 4. See your first recording (2 minutes)

List recorded sessions:

```bash
mcptracer sessions list
```

You should see one row per client run, with a session ID, the `--client`
label `setup` wrote (e.g. `claude-desktop`), a message count, and a start
time. If the list is empty, see
[Troubleshooting](#sessions-list-is-empty).

Take the session ID from that list and look inside it:

```bash
mcptracer sessions show <session-id> --calls
```

`--calls` prints the **correlated request/response view**: instead of raw
JSON-RPC messages that you'd have to cross-reference by id yourself, each
row pairs one client request with the response that answered it — so you can
read straight down the list and see which tool was called, with what
arguments, and what came back, in the order it happened. Add `--json` to get
the same correlated view as structured data instead of a table. (`sessions
show <session-id>` without `--calls` prints the raw message log instead,
which is closer to what actually went over the wire.)

## 5. Make one useful comparison (3 minutes) — the payoff

A single recording is a transcript. The reason to keep more than one is to
compare them. Repeat steps 3 and 4 — exercise the same tool again (it
doesn't need to be a different call) — so you have a second session ID, then
run:

```bash
mcptracer diff <first-session-id> <second-session-id>
```

`diff` aligns the two sessions' calls by method, tool, and position, so it
doesn't get confused by request-id renumbering, and reports what actually
changed: content that differs, calls that were added or removed, and latency
deltas past a threshold. Comparing two recordings of genuinely identical
usage should report no meaningful differences (exit code 0) — that's a
useful result too, since it's what "nothing changed" looks like.

What makes `diff` more than a transcript comparison is that it separately
watches the tool's **declared contract** — its `tools/list` name,
description, input/output schema, and annotations — and reports a drift
there as a `SECURITY` finding, distinct from an ordinary behavioral change.
That distinction matters: an ordinary diff line means a response or timing
changed; a `SECURITY` finding means the tool itself now claims to do
something different than what you approved, even if the call and response
you're looking at are byte-for-byte identical to before. Two recordings of
your own everyday usage are unlikely to produce one on their own, since
nothing is actually rug-pulling the tool's contract between your two runs.

To see a guaranteed `SECURITY` finding without needing a live compromised
server, run the self-contained walkthrough at
[`examples/rug-pull-demo/`](../examples/rug-pull-demo/README.md). It records
a trusted baseline against a local fake server, then re-records the same
scripted client traffic against a version of that server whose tool
description quietly changed — same requests, same responses, different
contract — and shows `diff` and `assert --spec pin.toml` both catching it.

## 6. Undo (1 minute)

Reverse the wrap for one client:

```bash
mcptracer setup <client> --undo
```

This restores the config from its `.mcptracer.bak` byte-for-byte and removes
the backup. Restart the client again afterward, for the same reason as step
2. Run `mcptracer setup --undo` with no client to restore every supported
client configuration that currently has a backup.

Undoing the config change does not delete anything you recorded. Recordings
live in a local SQLite database at `~/.mcptracer/sessions.db` (this path is
under your home directory the same way on Windows, macOS, and Linux — there
is no separate per-OS location). If you want to remove recorded sessions
too, delete or move that file; there is currently no per-session delete
command.

## Troubleshooting

#### Client shows no servers after setup

You almost certainly didn't fully restart the client — see the note at the
end of [step 2](#2-wrap-your-client-2-minutes--the-step-people-get-wrong).
Quit it completely (not just "reload window") and reopen it.

#### `mcptracer: command not found`

`cargo install` puts the binary in `$CARGO_HOME/bin` (usually
`~/.cargo/bin`), which needs to be on your shell's `PATH`. This is a
different problem from the one `setup` solves for desktop clients (which
write an absolute path instead of relying on `PATH` at all) — it only
affects you running `mcptracer` yourself, from a terminal.

#### `setup` refuses to touch the config

`setup` validates the existing configuration before writing anything, and
would rather fail loudly than guess at a broken file. Two common causes:

- The config has a genuine syntax error (not just VS Code/Codex-style
  comments, which are tolerated) — fix whatever the printed error points at.
- A `.mcptracer.bak` already exists next to it, usually because `setup` was
  run before without `--undo` in between. Run `mcptracer setup <client>
  --undo` first, then re-run `setup`.

#### `sessions list` is empty

In order, the usual causes:

1. You didn't restart the client after `setup` (see step 2) — it's still
   launching the server directly, so nothing is being recorded.
2. You restarted, but haven't actually made a tool call yet (step 3) —
   wrapping alone records nothing.
3. The server you're exercising is configured as HTTP or SSE, not stdio —
   `setup` only wraps stdio servers, so an HTTP/SSE entry is never recorded
   this way. (Streamable HTTP has its own path, `mcptracer record-http`, not
   covered by this guide.)
4. You're querying a different database than the one `setup` printed — pass
   `--db <path>` to point at the right one if you're using a non-default
   profile.

#### Recovering from a bad state

`mcptracer setup <client> --undo` is the answer for anything involving the
config file — it restores the exact pre-`setup` content from the backup.
Since both the config and its `.mcptracer.bak` are plain JSON or TOML, you
can also open the backup and restore it by hand if you ever need to.
