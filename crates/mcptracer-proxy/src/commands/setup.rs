use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Args, ValueEnum};
use serde_json::{Map, Value};

const BACKUP_SUFFIX: &str = ".mcptracer.bak";

#[derive(Args)]
pub struct SetupArgs {
    /// Client configuration to update.
    pub client: Option<SetupClient>,

    /// Restore the pre-MCPTracer backup. Without a client, restores every
    /// supported default configuration that has an MCPTracer backup.
    #[arg(long)]
    pub undo: bool,

    /// Override the discovered client configuration path.
    #[arg(long)]
    pub config: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SetupClient {
    ClaudeDesktop,
    Cursor,
    Codex,
    Vscode,
}

impl SetupClient {
    fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeDesktop => "claude-desktop",
            Self::Cursor => "cursor",
            Self::Codex => "codex",
            Self::Vscode => "vscode",
        }
    }

    fn config_path(self) -> Result<PathBuf> {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow!("could not determine the user home directory"))?;

        match self {
            Self::ClaudeDesktop => {
                #[cfg(target_os = "windows")]
                {
                    let appdata = std::env::var_os("APPDATA")
                        .map(PathBuf::from)
                        .ok_or_else(|| anyhow!("APPDATA is not set"))?;
                    Ok(appdata.join("Claude").join("claude_desktop_config.json"))
                }
                #[cfg(target_os = "macos")]
                {
                    Ok(home
                        .join("Library")
                        .join("Application Support")
                        .join("Claude")
                        .join("claude_desktop_config.json"))
                }
                #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
                {
                    let config = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
                    Ok(config.join("Claude").join("claude_desktop_config.json"))
                }
            }
            Self::Cursor => Ok(home.join(".cursor").join("mcp.json")),
            Self::Codex => Ok(home.join(".codex").join("config.toml")),
            Self::Vscode => {
                #[cfg(target_os = "windows")]
                {
                    let appdata = std::env::var_os("APPDATA")
                        .map(PathBuf::from)
                        .ok_or_else(|| anyhow!("APPDATA is not set"))?;
                    Ok(appdata.join("Code").join("User").join("mcp.json"))
                }
                #[cfg(target_os = "macos")]
                {
                    Ok(home
                        .join("Library")
                        .join("Application Support")
                        .join("Code")
                        .join("User")
                        .join("mcp.json"))
                }
                #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
                {
                    let config = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
                    Ok(config.join("Code").join("User").join("mcp.json"))
                }
            }
        }
    }

    fn is_toml(self) -> bool {
        matches!(self, Self::Codex)
    }

    fn json_server_key(self) -> Option<&'static str> {
        match self {
            Self::ClaudeDesktop | Self::Cursor => Some("mcpServers"),
            Self::Vscode => Some("servers"),
            Self::Codex => None,
        }
    }
}

struct SetupResult {
    wrapped_servers: usize,
    // Distinguishes "this config has no stdio servers at all" from "every
    // stdio server here is already wrapped" - both produce wrapped_servers
    // == 0, but only the latter is a successful idempotent re-run.
    total_stdio_servers: usize,
    backup_path: Option<PathBuf>,
}

pub async fn run(args: SetupArgs) -> Result<()> {
    if args.undo {
        return run_undo(args);
    }

    let client = args.client.ok_or_else(|| {
        anyhow!("a client is required; use claude-desktop, cursor, codex, or vscode")
    })?;
    let path = args.config.unwrap_or(client.config_path()?);
    let result = inject_config(client, &path)?;

    if result.wrapped_servers == 0 {
        if result.total_stdio_servers == 0 {
            println!(
                "[mcptracer] no stdio MCP servers found in {} (nothing to wrap)",
                path.display()
            );
        } else {
            // Not an error: re-running `setup` on an already-wrapped config
            // is expected to be a no-op, and the user should be told it
            // succeeded rather than left wondering whether anything ran.
            println!(
                "[mcptracer] all {} stdio MCP server(s) in {} are already wrapped; nothing to do",
                result.total_stdio_servers,
                path.display()
            );
        }
    } else {
        let backup = result
            .backup_path
            .expect("a changed configuration must have a backup");
        println!(
            "[mcptracer] wrapped {} stdio MCP server(s) for {} in {} (backup: {})",
            result.wrapped_servers,
            client.display_name(),
            path.display(),
            backup.display()
        );
        println!(
            "[mcptracer] restart {} completely for the change to take effect - most MCP \
             clients only read their server config at startup, so nothing is recorded until \
             you do",
            client.display_name()
        );
        println!(
            "[mcptracer] recordings will be stored at {}",
            mcptracer_storage::default_db_path().display()
        );
        println!("[mcptracer] see them with: mcptracer sessions list");
        println!(
            "[mcptracer] undo with: mcptracer setup {} --undo",
            client.display_name()
        );
    }
    Ok(())
}

fn run_undo(args: SetupArgs) -> Result<()> {
    if let Some(client) = args.client {
        let path = args.config.unwrap_or(client.config_path()?);
        restore_config(client, &path)?;
        println!(
            "[mcptracer] restored {} configuration from its backup: {}",
            client.display_name(),
            path.display()
        );
        return Ok(());
    }
    if args.config.is_some() {
        return Err(anyhow!("--config requires a client"));
    }

    let mut restored = 0;
    for client in [
        SetupClient::ClaudeDesktop,
        SetupClient::Cursor,
        SetupClient::Codex,
        SetupClient::Vscode,
    ] {
        let path = client.config_path()?;
        if backup_path(&path).exists() {
            restore_config(client, &path)?;
            println!(
                "[mcptracer] restored {} configuration from its backup: {}",
                client.display_name(),
                path.display()
            );
            restored += 1;
        }
    }
    if restored == 0 {
        return Err(anyhow!("no MCPTracer setup backups were found"));
    }
    Ok(())
}

fn inject_config(client: SetupClient, path: &Path) -> Result<SetupResult> {
    let original = read_config(path)?;
    // Resolved once, here, rather than inside wrap_json_server/wrap_toml_server:
    // those stay pure functions that a unit test can call with an arbitrary
    // command string, instead of every test depending on the real
    // current_exe() of whatever binary happens to be running the test suite.
    let mcptracer_command = resolve_mcptracer_command();
    let (updated, total_stdio_servers, wrapped_servers) = if client.is_toml() {
        inject_toml(&original, client, &mcptracer_command)?
    } else {
        inject_json(&original, client, &mcptracer_command)?
    };

    if wrapped_servers == 0 {
        return Ok(SetupResult {
            wrapped_servers,
            total_stdio_servers,
            backup_path: None,
        });
    }

    let backup_path = backup_path(path);
    if backup_path.exists() {
        return Err(anyhow!(
            "refusing to overwrite existing MCPTracer backup {}; run `mcptracer setup --undo` first",
            backup_path.display()
        ));
    }
    write_with_backup(path, &original, updated.as_bytes(), &backup_path)?;
    Ok(SetupResult {
        wrapped_servers,
        total_stdio_servers,
        backup_path: Some(backup_path),
    })
}

fn restore_config(client: SetupClient, path: &Path) -> Result<()> {
    let backup_path = backup_path(path);
    let backup = read_config(&backup_path).with_context(|| {
        format!(
            "no MCPTracer backup exists for {}; cannot undo",
            path.display()
        )
    })?;

    if client.is_toml() {
        toml::from_str::<toml::Value>(&backup)
            .context("refusing to restore an invalid TOML backup")?;
    } else {
        serde_json::from_str::<Value>(&backup)
            .context("refusing to restore an invalid JSON backup")?;
    }
    replace_file(path, backup.as_bytes())?;
    fs::remove_file(&backup_path)
        .with_context(|| format!("failed to remove restored backup {}", backup_path.display()))?;
    Ok(())
}

/// Returns `(rewritten config, stdio servers seen, stdio servers newly
/// wrapped)`. The "seen" count includes servers that were already wrapped,
/// so callers can tell "no stdio servers here" apart from "already wrapped"
/// even when the "newly wrapped" count is zero for both.
fn inject_json(
    original: &str,
    client: SetupClient,
    mcptracer_command: &str,
) -> Result<(String, usize, usize)> {
    // VS Code's `mcp.json` is documented to accept JSONC: `//` and `/* */`
    // comments, plus trailing commas before `}`/`]`. Try strict JSON first -
    // it's the common case for the other clients and gives the most precise
    // serde_json error - and only fall back to the comment/trailing-comma
    // stripper (warning that it discards them) when strict parsing actually
    // fails. This keeps the original error message intact for configs that
    // are genuinely broken rather than merely JSONC.
    let mut root = match serde_json::from_str::<Value>(original) {
        Ok(root) => root,
        Err(_) => {
            let stripped = strip_jsonc(original);
            let root = serde_json::from_str::<Value>(&stripped)
                .context("refusing to modify an invalid JSON MCP configuration")?;
            eprintln!(
                "[mcptracer] warning: this configuration uses JSONC syntax (comments and/or \
                 trailing commas) that strict JSON does not allow; mcptracer parsed it by \
                 stripping those out. The file will be rewritten as strict JSON, so the \
                 comments will NOT be preserved - your original is saved in the \
                 `{BACKUP_SUFFIX}` backup."
            );
            root
        }
    };
    let root_object = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("JSON MCP configuration must be an object"))?;
    let server_key = client
        .json_server_key()
        .expect("non-Codex clients have a JSON server key");
    let servers = root_object
        .get_mut(server_key)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("JSON MCP configuration must contain an object `{server_key}`"))?;

    let mut total_stdio_servers = 0;
    let mut wrapped_servers = 0;
    for (server_name, server) in servers {
        let server_object = server
            .as_object_mut()
            .ok_or_else(|| anyhow!("MCP server `{server_name}` must be an object"))?;
        if !server_object.contains_key("command") {
            continue;
        }
        total_stdio_servers += 1;
        if wrap_json_server(server_object, client, mcptracer_command)? {
            wrapped_servers += 1;
        }
    }
    Ok((
        serde_json::to_string_pretty(&root)? + "\n",
        total_stdio_servers,
        wrapped_servers,
    ))
}

/// Best-effort conversion of JSONC into strict JSON that `serde_json`
/// accepts: strips `//` and `/* */` comments and drops trailing commas
/// before a closing `}`/`]`. Only ever invoked after a strict
/// `serde_json::from_str` attempt has already failed - see `inject_json`.
fn strip_jsonc(text: &str) -> String {
    match strip_jsonc_comments(text) {
        Some(without_comments) => strip_trailing_commas(&without_comments),
        // An unterminated string literal means this isn't a
        // comments/trailing-comma problem at all - it's malformed JSON.
        // Return the input unchanged so the caller's real
        // `serde_json::from_str` retry produces that error itself instead
        // of us guessing at one from a half-mangled rewrite.
        None => text.to_string(),
    }
}

/// Strips `//` line comments and `/* */` block comments from `text`.
/// String-literal and escape aware: text inside a JSON string - including
/// `\"`, `\\`, and other escape sequences - is copied through unchanged and
/// never mistaken for a comment delimiter, so `"a // b"`, `"a \" // b"`,
/// and `"C:\\path"` all survive untouched. Returns `None` if a string
/// literal is left unterminated, so the caller can bail out rather than
/// silently mangling input that is malformed for an unrelated reason.
fn strip_jsonc_comments(text: &str) -> Option<String> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Normal,
        InString,
        StringEscape,
        LineComment,
        BlockComment,
        // Seen a `*` inside a block comment; a `/` next closes it, another
        // `*` stays here (so `**/` still closes), anything else resumes
        // the plain block-comment state.
        BlockCommentMaybeEnd,
    }

    let mut out = String::with_capacity(text.len());
    let mut state = State::Normal;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match state {
            State::Normal => match c {
                '"' => {
                    out.push(c);
                    state = State::InString;
                }
                '/' if chars.peek() == Some(&'/') => {
                    chars.next();
                    state = State::LineComment;
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    state = State::BlockComment;
                }
                _ => out.push(c),
            },
            State::InString => match c {
                '\\' => {
                    out.push(c);
                    state = State::StringEscape;
                }
                '"' => {
                    out.push(c);
                    state = State::Normal;
                }
                // A raw newline can never appear in a valid JSON string -
                // treat it as an unterminated literal and bail out rather
                // than keep scanning past it.
                '\n' => return None,
                _ => out.push(c),
            },
            // Whatever follows a backslash is part of the escape sequence
            // (`\"`, `\\`, `\n`, `\uXXXX`, ...), never a delimiter - copy it
            // through untouched and resume normal string scanning.
            State::StringEscape => {
                out.push(c);
                state = State::InString;
            }
            State::LineComment => {
                if c == '\n' {
                    out.push(c);
                    state = State::Normal;
                }
            }
            State::BlockComment => {
                if c == '*' {
                    state = State::BlockCommentMaybeEnd;
                }
            }
            State::BlockCommentMaybeEnd => {
                state = if c == '/' {
                    State::Normal
                } else if c == '*' {
                    State::BlockCommentMaybeEnd
                } else {
                    State::BlockComment
                };
            }
        }
    }

    match state {
        // EOF while still inside a string (or right after its escaping
        // backslash) is the same "unterminated literal" case as hitting a
        // raw newline above.
        State::InString | State::StringEscape => None,
        _ => Some(out),
    }
}

/// Drops commas that appear directly before a closing `}` or `]` (ignoring
/// intervening whitespace) - the trailing-comma half of JSONC. Assumes
/// comments have already been stripped (via `strip_jsonc_comments`) but
/// stays string-aware regardless, so a comma that is itself string content
/// is never touched.
fn strip_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }

        if c == ',' {
            let mut lookahead = i + 1;
            while lookahead < chars.len() && chars[lookahead].is_whitespace() {
                lookahead += 1;
            }
            if lookahead < chars.len() && matches!(chars[lookahead], '}' | ']') {
                // Drop the comma; the whitespace and the closer itself are
                // copied through normally on the following iterations.
                i += 1;
                continue;
            }
        }

        out.push(c);
        i += 1;
    }

    out
}

fn wrap_json_server(
    server: &mut Map<String, Value>,
    client: SetupClient,
    mcptracer_command: &str,
) -> Result<bool> {
    let command = server
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("MCP server `command` must be a string"))?;
    let args = json_args(server.get("args"))?;
    if is_mcptracer_record_wrapper(command, &args) {
        return Ok(false);
    }
    let wrapped_args = wrapped_args(client, command, &args);
    server.insert(
        "command".to_string(),
        Value::String(mcptracer_command.to_string()),
    );
    server.insert(
        "args".to_string(),
        Value::Array(wrapped_args.into_iter().map(Value::String).collect()),
    );
    Ok(true)
}

/// Returns `(rewritten config, stdio servers seen, stdio servers newly
/// wrapped)` - see `inject_json` for why both counts matter.
fn inject_toml(
    original: &str,
    client: SetupClient,
    mcptracer_command: &str,
) -> Result<(String, usize, usize)> {
    let mut root = toml::from_str::<toml::Value>(original)
        .context("refusing to modify an invalid TOML MCP configuration")?;
    let servers = root
        .get_mut("mcp_servers")
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| anyhow!("Codex configuration must contain a `[mcp_servers]` table"))?;

    let mut total_stdio_servers = 0;
    let mut wrapped_servers = 0;
    for (server_name, server) in servers {
        let server_table = server
            .as_table_mut()
            .ok_or_else(|| anyhow!("Codex MCP server `{server_name}` must be a table"))?;
        if !server_table.contains_key("command") {
            continue;
        }
        total_stdio_servers += 1;
        if wrap_toml_server(server_table, client, mcptracer_command)? {
            wrapped_servers += 1;
        }
    }
    if wrapped_servers > 0 {
        // toml::to_string_pretty re-serializes from the parsed toml::Value,
        // which - unlike a format-preserving editor such as toml_edit, a
        // dependency this project deliberately avoids adding - keeps none
        // of the user's original comments or formatting. Only warn when a
        // rewrite is actually about to happen, not on read-only/no-op runs.
        eprintln!(
            "[mcptracer] warning: rewriting this TOML configuration normalizes its formatting \
             and discards any comments - mcptracer does not use a comment-preserving TOML \
             writer. Your original is saved in the `{BACKUP_SUFFIX}` backup."
        );
    }
    Ok((
        toml::to_string_pretty(&root)?,
        total_stdio_servers,
        wrapped_servers,
    ))
}

fn wrap_toml_server(
    server: &mut toml::map::Map<String, toml::Value>,
    client: SetupClient,
    mcptracer_command: &str,
) -> Result<bool> {
    let command = server
        .get("command")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| anyhow!("Codex MCP server `command` must be a string"))?;
    let args = toml_args(server.get("args"))?;
    if is_mcptracer_record_wrapper(command, &args) {
        return Ok(false);
    }
    let wrapped_args = wrapped_args(client, command, &args);
    server.insert(
        "command".to_string(),
        toml::Value::String(mcptracer_command.to_string()),
    );
    server.insert(
        "args".to_string(),
        toml::Value::Array(wrapped_args.into_iter().map(toml::Value::String).collect()),
    );
    Ok(true)
}

fn json_args(value: Option<&Value>) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or_else(|| anyhow!("MCP server `args` must be an array of strings"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("MCP server `args` must be an array of strings"))
        })
        .collect()
}

fn toml_args(value: Option<&toml::Value>) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or_else(|| anyhow!("Codex MCP server `args` must be an array of strings"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("Codex MCP server `args` must be an array of strings"))
        })
        .collect()
}

fn wrapped_args(client: SetupClient, command: &str, args: &[String]) -> Vec<String> {
    let mut wrapped = vec![
        "record".to_string(),
        "--client".to_string(),
        client.display_name().to_string(),
        "--".to_string(),
        command.to_string(),
    ];
    wrapped.extend(args.iter().cloned());
    wrapped
}

fn is_mcptracer_record_wrapper(command: &str, args: &[String]) -> bool {
    let executable = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
        .to_ascii_lowercase();
    matches!(executable.as_str(), "mcptracer" | "mcptracer.exe")
        && args.first().is_some_and(|arg| arg == "record")
        && args.get(1).is_some_and(|arg| arg == "--client")
}

/// Resolves the absolute path to the currently running `mcptracer` binary,
/// so wrapped MCP server entries invoke it directly instead of relying on a
/// PATH lookup. `scripts/install.sh` installs to `$HOME/.local/bin`, which
/// is not on the minimal PATH that launchd (macOS) or Explorer (Windows)
/// hand to GUI-launched MCP clients like Claude Desktop, Cursor, or VS
/// Code - writing the bare string `"mcptracer"` as the command therefore
/// silently breaks every server `setup` wraps under those clients.
///
/// Falls back conservatively at every step, rather than erroring the whole
/// `setup` command out: an unresolvable or non-UTF-8 path is still better
/// served by the old PATH-relative behavior than by refusing to wrap
/// servers at all.
fn resolve_mcptracer_command() -> String {
    let Ok(current_exe) = std::env::current_exe() else {
        // Documented to fail in exotic/sandboxed environments - fall back
        // to relying on PATH exactly like before this fix.
        return "mcptracer".to_string();
    };

    let resolved = canonicalize_for_config(&current_exe).unwrap_or(current_exe);

    match resolved.to_str() {
        Some(path) => path.to_string(),
        // MCP client configs are JSON/TOML strings; a non-UTF-8 install
        // path can't be represented in them at all, so fall back to the
        // bare name and let PATH resolution take over as before.
        None => "mcptracer".to_string(),
    }
}

/// Canonicalizes `path`, stripping the `\\?\` "verbatim" prefix Windows'
/// `canonicalize` adds (it opts out of the legacy MAX_PATH limit, but MCP
/// clients that shell out to the resulting path don't expect that form and
/// choke on it). Returns `None` if canonicalization fails so the caller can
/// fall back to the non-canonicalized `current_exe()` result instead of
/// erroring out.
fn canonicalize_for_config(path: &Path) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    Some(strip_windows_verbatim_prefix(canonical))
}

#[cfg(target_os = "windows")]
fn strip_windows_verbatim_prefix(path: PathBuf) -> PathBuf {
    const VERBATIM_UNC_PREFIX: &str = r"\\?\UNC\";
    const VERBATIM_PREFIX: &str = r"\\?\";

    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(rest) = text.strip_prefix(VERBATIM_UNC_PREFIX) {
        // `\\?\UNC\server\share\...` is the verbatim form of a network
        // path; its non-verbatim equivalent is `\\server\share\...`, not
        // `UNC\server\share\...`, so restore the leading `\\` too.
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(VERBATIM_PREFIX) {
        PathBuf::from(rest)
    } else {
        path
    }
}

#[cfg(not(target_os = "windows"))]
fn strip_windows_verbatim_prefix(path: PathBuf) -> PathBuf {
    // Only Windows' `canonicalize` produces the verbatim `\\?\` form.
    path
}

fn backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("mcp-config");
    path.with_file_name(format!("{name}{BACKUP_SUFFIX}"))
}

fn read_config(path: &Path) -> Result<String> {
    fs::read_to_string(path)
        .with_context(|| format!("failed to read configuration {}", path.display()))
}

fn write_with_backup(
    path: &Path,
    original: &str,
    updated: &[u8],
    backup_path: &Path,
) -> Result<()> {
    fs::write(backup_path, original).with_context(|| {
        format!(
            "failed to write configuration backup {}",
            backup_path.display()
        )
    })?;
    replace_file(path, updated).with_context(|| {
        format!(
            "configuration replacement failed; original is retained at {}",
            backup_path.display()
        )
    })
}

fn replace_file(path: &Path, contents: &[u8]) -> Result<()> {
    let temp_path = path.with_file_name(format!(
        ".{}.mcptracer.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config")
    ));
    if temp_path.exists() {
        return Err(anyhow!(
            "refusing to overwrite temporary configuration file {}",
            temp_path.display()
        ));
    }
    fs::write(&temp_path, contents).with_context(|| {
        format!(
            "failed to write temporary configuration {}",
            temp_path.display()
        )
    })?;
    // Atomic on both POSIX (rename(2)) and Windows (MoveFileExW with
    // MOVEFILE_REPLACE_EXISTING) - unlike the copy-then-remove this
    // replaced, there is no window where a crash or disk-full event
    // between the two steps could leave `path` half-written. temp_path is
    // always in the same directory as `path` (see above), so this never
    // crosses a filesystem boundary, which is the one case `rename` can't
    // handle atomically.
    fs::rename(&temp_path, path)
        .with_context(|| format!("failed to replace configuration {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use serde_json::Value;

    use super::{
        backup_path, inject_config, inject_json, inject_toml, is_mcptracer_record_wrapper,
        resolve_mcptracer_command, restore_config, strip_jsonc, SetupClient,
    };

    struct TestDir(PathBuf);

    impl TestDir {
        /// Structurally unique per call, not just probabilistically so: a
        /// timestamp alone collides when two of this module's tests enter
        /// `new()` within the same clock tick (observed on Windows, whose
        /// `SystemTime` granularity is ~15.6ms), leaving both tests writing
        /// the same `mcp.json` fixture path into one shared directory. The
        /// process id plus a monotonically increasing counter guarantees a
        /// fresh directory on every call regardless of clock resolution.
        fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("mcptracer-setup-{}-{unique}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn json_setup_wraps_stdio_servers_preserves_http_and_undoes_exactly() {
        let dir = TestDir::new();
        let path = dir.path("mcp.json");
        let original = r#"{
  "mcpServers": {
    "local": {"command":"npx","args":["-y","server"],"env":{"TOKEN":"keep"}},
    "remote": {"type":"http","url":"https://example.test/mcp"}
  }
}
"#;
        fs::write(&path, original).unwrap();

        let result = inject_config(SetupClient::ClaudeDesktop, &path).unwrap();
        assert_eq!(result.wrapped_servers, 1);
        assert!(backup_path(&path).exists());

        let updated: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let local = &updated["mcpServers"]["local"];
        // inject_config resolves the real running test binary's absolute
        // path rather than the literal "mcptracer" (BUG 1) - compare
        // against that same resolution instead of a hardcoded string.
        assert_eq!(local["command"], resolve_mcptracer_command());
        assert_eq!(
            local["args"],
            serde_json::json!([
                "record",
                "--client",
                "claude-desktop",
                "--",
                "npx",
                "-y",
                "server"
            ])
        );
        assert_eq!(
            updated["mcpServers"]["remote"]["url"],
            "https://example.test/mcp"
        );

        restore_config(SetupClient::ClaudeDesktop, &path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn vscode_uses_servers_key_and_wrapping_is_idempotent() {
        // Exercised at the inject_json layer with a fixed command string,
        // rather than by calling inject_config twice on a real file: the
        // second inject_config call would re-resolve resolve_mcptracer_command(),
        // and under `cargo test` that resolves to the test harness
        // binary's own hashed filename (e.g. `mcptracer-<hash>.exe`) rather
        // than literally `mcptracer`/`mcptracer.exe`, which is the only
        // thing is_mcptracer_record_wrapper recognizes as already-wrapped
        // (see below). That mismatch is purely a `cargo test` artifact -
        // the real installed binary from scripts/install.sh is always
        // named exactly `mcptracer`/`mcptracer.exe` - and the dedicated
        // `already_wrapped_entry_using_absolute_path_is_skipped` test above
        // already covers idempotency for a real resolved absolute path.
        let original = r#"{"servers":{"local":{"command":"python","args":["server.py"]}}}"#;
        let (wrapped_once, total_once, wrapped_once_count) =
            inject_json(original, SetupClient::Vscode, "mcptracer").unwrap();
        assert_eq!(total_once, 1);
        assert_eq!(wrapped_once_count, 1);
        let updated: Value = serde_json::from_str(&wrapped_once).unwrap();
        assert_eq!(updated["servers"]["local"]["args"][0], "record");

        let (_wrapped_twice, total_twice, wrapped_twice_count) =
            inject_json(&wrapped_once, SetupClient::Vscode, "mcptracer").unwrap();
        // The server is still there and still stdio - it's just already
        // wrapped - so the "seen" count stays 1 while "newly wrapped" drops
        // to 0. That distinction is what lets `run()` report "already
        // wrapped" instead of "no stdio servers found".
        assert_eq!(total_twice, 1);
        assert_eq!(wrapped_twice_count, 0);
    }

    #[test]
    fn codex_toml_setup_wraps_stdio_servers_and_undoes_exactly() {
        let dir = TestDir::new();
        let path = dir.path("config.toml");
        let original = r#"model = "gpt-5"

[mcp_servers.local]
command = "uvx"
args = ["server"]

[mcp_servers.remote]
url = "https://example.test/mcp"
"#;
        fs::write(&path, original).unwrap();

        let result = inject_config(SetupClient::Codex, &path).unwrap();
        assert_eq!(result.wrapped_servers, 1);
        let updated: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let local = updated["mcp_servers"]["local"].as_table().unwrap();
        let expected_command = resolve_mcptracer_command();
        assert_eq!(local["command"].as_str(), Some(expected_command.as_str()));
        assert_eq!(
            local["args"].as_array().unwrap(),
            &vec![
                toml::Value::String("record".to_string()),
                toml::Value::String("--client".to_string()),
                toml::Value::String("codex".to_string()),
                toml::Value::String("--".to_string()),
                toml::Value::String("uvx".to_string()),
                toml::Value::String("server".to_string()),
            ]
        );
        assert_eq!(
            updated["mcp_servers"]["remote"]["url"].as_str(),
            Some("https://example.test/mcp")
        );

        restore_config(SetupClient::Codex, &path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn unparseable_or_invalid_server_configs_are_left_untouched() {
        let dir = TestDir::new();
        let invalid_json = dir.path("invalid.json");
        fs::write(&invalid_json, "{not json").unwrap();
        assert!(inject_config(SetupClient::Cursor, &invalid_json).is_err());
        assert_eq!(fs::read_to_string(&invalid_json).unwrap(), "{not json");
        assert!(!backup_path(&invalid_json).exists());

        let invalid_args = dir.path("invalid-args.json");
        let original = r#"{"mcpServers":{"local":{"command":"node","args":[1]}}}"#;
        fs::write(&invalid_args, original).unwrap();
        assert!(inject_config(SetupClient::Cursor, &invalid_args).is_err());
        assert_eq!(fs::read_to_string(&invalid_args).unwrap(), original);
        assert!(!backup_path(&invalid_args).exists());
    }

    #[test]
    fn resolve_mcptracer_command_returns_a_nonempty_string() {
        // Can't assert an exact value - it depends on where the test
        // binary itself happens to be installed - but every fallback in
        // resolve_mcptracer_command() ends in a usable, non-empty string,
        // even in unusual sandboxes where current_exe() fails.
        assert!(!resolve_mcptracer_command().is_empty());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_verbatim_prefix_is_stripped() {
        // canonicalize() on Windows prefixes results with `\\?\`, which MCP
        // clients choke on (see BUG 1) - both the plain-drive and UNC forms
        // must come back out looking like an ordinary path.
        assert_eq!(
            super::strip_windows_verbatim_prefix(PathBuf::from(r"\\?\C:\Users\test\mcptracer.exe")),
            PathBuf::from(r"C:\Users\test\mcptracer.exe")
        );
        assert_eq!(
            super::strip_windows_verbatim_prefix(PathBuf::from(
                r"\\?\UNC\server\share\mcptracer.exe"
            )),
            PathBuf::from(r"\\server\share\mcptracer.exe")
        );
    }

    #[test]
    fn absolute_path_command_is_written_for_json_and_toml_clients() {
        let mcptracer_command = if cfg!(target_os = "windows") {
            r"C:\Users\test\.local\bin\mcptracer.exe"
        } else {
            "/home/test/.local/bin/mcptracer"
        };

        let (json_updated, json_total, json_wrapped) = inject_json(
            r#"{"mcpServers":{"local":{"command":"npx","args":["-y","server"]}}}"#,
            SetupClient::ClaudeDesktop,
            mcptracer_command,
        )
        .unwrap();
        assert_eq!(json_total, 1);
        assert_eq!(json_wrapped, 1);
        let json_value: Value = serde_json::from_str(&json_updated).unwrap();
        assert_eq!(
            json_value["mcpServers"]["local"]["command"],
            mcptracer_command
        );

        let (toml_updated, toml_total, toml_wrapped) = inject_toml(
            "[mcp_servers.local]\ncommand = \"uvx\"\nargs = [\"server\"]\n",
            SetupClient::Codex,
            mcptracer_command,
        )
        .unwrap();
        assert_eq!(toml_total, 1);
        assert_eq!(toml_wrapped, 1);
        let toml_value: toml::Value = toml::from_str(&toml_updated).unwrap();
        assert_eq!(
            toml_value["mcp_servers"]["local"]["command"].as_str(),
            Some(mcptracer_command)
        );
    }

    #[test]
    fn already_wrapped_entry_using_absolute_path_is_skipped() {
        let absolute_path = if cfg!(target_os = "windows") {
            r"C:\Users\test\.local\bin\mcptracer.exe"
        } else {
            "/home/test/.local/bin/mcptracer"
        };

        // is_mcptracer_record_wrapper matches on the basename, so an
        // absolute path must still be recognized as "already wrapped".
        assert!(is_mcptracer_record_wrapper(
            absolute_path,
            &[
                "record".to_string(),
                "--client".to_string(),
                "claude-desktop".to_string(),
                "--".to_string(),
                "npx".to_string(),
            ]
        ));

        // End-to-end: re-running inject_json over an entry that is already
        // wrapped with an absolute mcptracer path must be a no-op, not a
        // double-wrap.
        let already_wrapped = format!(
            r#"{{"mcpServers":{{"local":{{"command":{:?},"args":["record","--client","claude-desktop","--","npx","-y","server"]}}}}}}"#,
            absolute_path
        );
        let (_updated, total, wrapped) =
            inject_json(&already_wrapped, SetupClient::ClaudeDesktop, absolute_path).unwrap();
        assert_eq!(total, 1);
        assert_eq!(wrapped, 0);
    }

    #[test]
    fn jsonc_comments_and_trailing_comma_are_tolerated() {
        let original = r#"{
  // top-level comment
  "mcpServers": {
    /* block comment
       spanning lines */
    "local": {
      "command": "npx", // inline comment
      "args": ["-y", "server",],
    },
  },
}
"#;
        let (updated, total, wrapped) = inject_json(
            original,
            SetupClient::ClaudeDesktop,
            "/opt/mcptracer/bin/mcptracer",
        )
        .unwrap();
        assert_eq!(total, 1);
        assert_eq!(wrapped, 1);
        let value: Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(
            value["mcpServers"]["local"]["command"],
            "/opt/mcptracer/bin/mcptracer"
        );
        assert_eq!(
            value["mcpServers"]["local"]["args"],
            serde_json::json!([
                "record",
                "--client",
                "claude-desktop",
                "--",
                "npx",
                "-y",
                "server"
            ])
        );
    }

    #[test]
    fn comment_stripper_preserves_string_contents_and_escapes() {
        // `//` and `/*` sequences that are themselves string content, and
        // escaped quotes/backslashes, must survive the stripper untouched -
        // only real comments (outside any string) may be removed.
        let input = r#"{"a": "a // b", "b": "a \" // b", "c": "C:\\path", "d": 1} // trailing"#;
        let stripped = strip_jsonc(input);
        let value: Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(value["a"], "a // b");
        assert_eq!(value["b"], "a \" // b");
        assert_eq!(value["c"], "C:\\path");
        assert_eq!(value["d"], 1);

        // A `/*` inside a string is not a block-comment opener either.
        let block_like = r#"{"note": "see /* not a comment */ here"}"#;
        let stripped_block = strip_jsonc(block_like);
        let value: Value = serde_json::from_str(&stripped_block).unwrap();
        assert_eq!(value["note"], "see /* not a comment */ here");
    }

    #[test]
    fn malformed_json_that_is_not_jsonc_still_fails_with_existing_error() {
        // Unterminated string plus missing closing braces: not fixable by
        // stripping comments or trailing commas, so the original "refusing
        // to modify" error must still surface, not a confusing one caused
        // by the JSONC preprocessing itself.
        let err = inject_json(
            r#"{"mcpServers": {"local": {"command": "npx"#,
            SetupClient::ClaudeDesktop,
            "/opt/mcptracer/bin/mcptracer",
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("refusing to modify an invalid JSON MCP configuration"));
    }

    #[test]
    fn wrapping_a_json_server_preserves_every_other_field_on_it() {
        // wrap_json_server only ever calls `.insert("command", ...)` and
        // `.insert("args", ...)` on the server's Map - this pins that down
        // end-to-end for the field shapes a real config actually carries
        // (a nested `env` object, `cwd`, a numeric `timeout`, and one field
        // mcptracer has never heard of) rather than trusting that by
        // reading the implementation.
        let original = r#"{
  "mcpServers": {
    "local": {
      "command": "npx",
      "args": ["-y", "server"],
      "env": {"TOKEN": "keep", "NESTED": {"a": 1}},
      "cwd": "/srv/app",
      "timeout": 30,
      "experimentalFlag": true
    }
  }
}
"#;
        let (updated, total, wrapped) = inject_json(
            original,
            SetupClient::ClaudeDesktop,
            "/opt/mcptracer/bin/mcptracer",
        )
        .unwrap();
        assert_eq!(total, 1);
        assert_eq!(wrapped, 1);

        let value: Value = serde_json::from_str(&updated).unwrap();
        let local = &value["mcpServers"]["local"];
        assert_eq!(
            local["env"],
            serde_json::json!({"TOKEN": "keep", "NESTED": {"a": 1}})
        );
        assert_eq!(local["cwd"], "/srv/app");
        assert_eq!(local["timeout"], 30);
        assert_eq!(local["experimentalFlag"], true);
        // Only command/args were actually meant to change.
        assert_eq!(local["command"], "/opt/mcptracer/bin/mcptracer");
        assert_eq!(local["args"][0], "record");
    }

    #[test]
    fn json_wrapping_leaves_unrelated_servers_and_top_level_keys_untouched() {
        // Three things sharing one file that `setup` must never conflate:
        // a server already wrapped (must be a pure no-op), an HTTP/SSE
        // server (has no `command`, so it's not even stdio), and top-level
        // keys that live alongside `mcpServers` rather than inside it.
        let original = r#"{
  "version": 2,
  "logging": {"level": "debug"},
  "mcpServers": {
    "already_wrapped": {
      "command": "/opt/mcptracer/bin/mcptracer",
      "args": ["record", "--client", "claude-desktop", "--", "npx", "-y", "server"]
    },
    "http_server": {"type": "http", "url": "https://example.test/mcp"},
    "local": {"command": "npx", "args": ["-y", "server"]}
  }
}
"#;
        let before: Value = serde_json::from_str(original).unwrap();
        let (updated, total, wrapped) = inject_json(
            original,
            SetupClient::ClaudeDesktop,
            "/opt/mcptracer/bin/mcptracer",
        )
        .unwrap();
        // `already_wrapped` and `local` both have `command`; `http_server`
        // does not, so it's excluded from the stdio count entirely.
        assert_eq!(total, 2);
        assert_eq!(wrapped, 1);

        let after: Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(
            after["mcpServers"]["already_wrapped"],
            before["mcpServers"]["already_wrapped"]
        );
        assert_eq!(
            after["mcpServers"]["http_server"],
            before["mcpServers"]["http_server"]
        );
        assert_eq!(after["version"], 2);
        assert_eq!(after["logging"], serde_json::json!({"level": "debug"}));
    }

    #[test]
    fn json_wrapping_preserves_paths_and_args_containing_spaces_in_order() {
        // Nothing in this module ever tokenizes `command`/`args` through a
        // shell - they are JSON strings copied around as opaque values -
        // but that guarantee is worth pinning down explicitly for the
        // paths-with-spaces case Windows users hit constantly (Program
        // Files installs) plus a positional arg with embedded spaces, and
        // for the exact order everything must come back in after `--`.
        let original = serde_json::json!({
            "mcpServers": {
                "local": {
                    "command": r"C:\Program Files\My Tool\server.exe",
                    "args": [
                        "--data-dir",
                        r"C:\Program Files\Data Dir\",
                        "positional arg with spaces",
                        "--flag"
                    ]
                }
            }
        })
        .to_string();

        let (updated, total, wrapped) = inject_json(
            &original,
            SetupClient::ClaudeDesktop,
            "/opt/mcptracer/bin/mcptracer",
        )
        .unwrap();
        assert_eq!(total, 1);
        assert_eq!(wrapped, 1);

        let value: Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(
            value["mcpServers"]["local"]["args"],
            serde_json::json!([
                "record",
                "--client",
                "claude-desktop",
                "--",
                r"C:\Program Files\My Tool\server.exe",
                "--data-dir",
                r"C:\Program Files\Data Dir\",
                "positional arg with spaces",
                "--flag"
            ])
        );
    }

    #[test]
    fn wrapping_a_toml_server_preserves_every_other_field_on_it() {
        // TOML equivalent of wrapping_a_json_server_preserves_every_other_field_on_it -
        // wrap_toml_server is separate code from wrap_json_server, so the
        // same guarantee needs its own proof rather than an inference from
        // the JSON path.
        let original = r#"[mcp_servers.local]
command = "uvx"
args = ["server"]
env = { TOKEN = "keep" }
cwd = "/srv/app"
timeout = 30
experimental_flag = true
"#;
        let (updated, total, wrapped) =
            inject_toml(original, SetupClient::Codex, "/opt/mcptracer/bin/mcptracer").unwrap();
        assert_eq!(total, 1);
        assert_eq!(wrapped, 1);

        let value: toml::Value = toml::from_str(&updated).unwrap();
        let local = value["mcp_servers"]["local"].as_table().unwrap();
        assert_eq!(local["env"]["TOKEN"].as_str(), Some("keep"));
        assert_eq!(local["cwd"].as_str(), Some("/srv/app"));
        assert_eq!(local["timeout"].as_integer(), Some(30));
        assert_eq!(local["experimental_flag"].as_bool(), Some(true));
        assert_eq!(
            local["command"].as_str(),
            Some("/opt/mcptracer/bin/mcptracer")
        );
    }

    #[test]
    fn toml_wrapping_leaves_unrelated_servers_and_top_level_keys_untouched() {
        // TOML equivalent of json_wrapping_leaves_unrelated_servers_and_top_level_keys_untouched.
        let original = r#"model = "gpt-5"

[logging]
level = "debug"

[mcp_servers.already_wrapped]
command = "/opt/mcptracer/bin/mcptracer"
args = ["record", "--client", "codex", "--", "uvx", "server"]

[mcp_servers.remote]
url = "https://example.test/mcp"

[mcp_servers.local]
command = "uvx"
args = ["server"]
"#;
        let before: toml::Value = toml::from_str(original).unwrap();
        let (updated, total, wrapped) =
            inject_toml(original, SetupClient::Codex, "/opt/mcptracer/bin/mcptracer").unwrap();
        // `already_wrapped` and `local` have `command`; `remote` (URL-only)
        // does not, so it never enters the stdio count.
        assert_eq!(total, 2);
        assert_eq!(wrapped, 1);

        let after: toml::Value = toml::from_str(&updated).unwrap();
        assert_eq!(
            after["mcp_servers"]["already_wrapped"],
            before["mcp_servers"]["already_wrapped"]
        );
        assert_eq!(
            after["mcp_servers"]["remote"],
            before["mcp_servers"]["remote"]
        );
        assert_eq!(after["model"].as_str(), Some("gpt-5"));
        assert_eq!(after["logging"]["level"].as_str(), Some("debug"));
    }

    #[test]
    fn toml_wrapping_preserves_paths_and_args_containing_spaces_in_order() {
        // TOML equivalent of json_wrapping_preserves_paths_and_args_containing_spaces_in_order.
        // TOML literal strings (single-quoted) are used here specifically
        // because they, like the JSON path, take backslashes and spaces
        // completely literally - no escaping to fight with in the fixture.
        let original = r#"[mcp_servers.local]
command = 'C:\Program Files\My Tool\server.exe'
args = ['--data-dir', 'C:\Program Files\Data Dir\', 'positional arg with spaces', '--flag']
"#;
        let (updated, total, wrapped) =
            inject_toml(original, SetupClient::Codex, "/opt/mcptracer/bin/mcptracer").unwrap();
        assert_eq!(total, 1);
        assert_eq!(wrapped, 1);

        let value: toml::Value = toml::from_str(&updated).unwrap();
        let args = value["mcp_servers"]["local"]["args"].as_array().unwrap();
        let expected = [
            "record",
            "--client",
            "codex",
            "--",
            r"C:\Program Files\My Tool\server.exe",
            "--data-dir",
            r"C:\Program Files\Data Dir\",
            "positional arg with spaces",
            "--flag",
        ];
        assert_eq!(args.len(), expected.len());
        for (actual, expected) in args.iter().zip(expected.iter()) {
            assert_eq!(actual.as_str(), Some(*expected));
        }
    }
}
