use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use mcptracer_protocol::McpMessage;
use mcptracer_redact::{RedactionPolicy, Redactor, REDACTED_PLACEHOLDER};
use rusqlite::{params, Connection, ErrorCode, OptionalExtension, Row, TransactionBehavior};
use serde::Serialize;
use uuid::Uuid;

/// Harden the storage directory to owner-only access (0700) and warn if
/// it already has broader permissions. No-op on non-Unix platforms.
#[cfg(unix)]
fn harden_dir_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    // Attempt to set 0700; if that fails, emit a warning but don't abort.
    match fs::set_permissions(path, fs::Permissions::from_mode(0o700)) {
        Ok(()) => {}
        Err(err) => {
            eprintln!(
                "[mcptracer] WARNING: could not set 0700 permissions on {}: {err}",
                path.display()
            );
        }
    }
    // If the directory already exists and was wider, warn even if set_permissions
    // succeeded (it may have been wider before this open).
    if let Ok(meta) = fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            eprintln!(
                "[mcptracer] WARNING: {} has permissions {:04o}; expected 0700. \
                 Other users may be able to read your recorded MCP sessions. \
                 Run: chmod 700 '{}'",
                path.display(),
                mode,
                path.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn harden_dir_permissions(_path: &Path) {}
#[cfg(unix)]
fn warn_dir_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            eprintln!(
                "[mcptracer] WARNING: custom database parent {} has permissions {:04o}; MCPTracer will not modify caller-managed directory permissions",
                path.display(), mode
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_dir_permissions(_path: &Path) {}

/// Harden the database file to owner-only access (0600) and warn if it
/// already has broader permissions. No-op on non-Unix platforms.
#[cfg(unix)]
fn harden_file_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    match fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        Ok(()) => {}
        Err(err) => {
            eprintln!(
                "[mcptracer] WARNING: could not set 0600 permissions on {}: {err}",
                path.display()
            );
        }
    }
    if let Ok(meta) = fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            eprintln!(
                "[mcptracer] WARNING: {} has permissions {:04o}; expected 0600. \
                 Other users may be able to read your recorded MCP sessions. \
                 Run: chmod 600 '{}'",
                path.display(),
                mode,
                path.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn harden_file_permissions(_path: &Path) {}

/// Harden the WAL and shared-memory sidecar files SQLite creates alongside
/// the database in WAL mode (`<db>-wal`, `<db>-shm`). They hold
/// not-yet-checkpointed session data — the same sensitivity as the main
/// database file — but SQLite creates them independently, so they need
/// their own pass rather than inheriting whatever `harden_file_permissions`
/// already did to `db_path` itself. Only touches files that exist: on a
/// fresh open before any write, or on a filesystem where WAL silently falls
/// back to another journal mode, neither sidecar exists yet.
#[cfg(unix)]
fn harden_wal_sidecar_files(db_path: &Path) {
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = db_path.as_os_str().to_owned();
        sidecar.push(suffix);
        let sidecar_path = Path::new(&sidecar);
        if sidecar_path.exists() {
            harden_file_permissions(sidecar_path);
        }
    }
}

#[cfg(not(unix))]
fn harden_wal_sidecar_files(_db_path: &Path) {}

pub mod graph;
pub mod mtrace;
pub mod vcr;

const CURRENT_SCHEMA_VERSION: i64 = 5;

/// How long any statement waits on another connection's lock before failing.
/// Applied once the schema is ready; opening uses the shorter of this and
/// whatever remains of `SCHEMA_LOCK_RETRY_BUDGET`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const CONNECTION_PRAGMAS: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA synchronous = NORMAL;
"#;

/// Wall-clock ceiling on opening a database another connection holds locked,
/// enforced by [`Store::init_schema`]. A single attempt can block inside
/// SQLite's busy handler, so bounding retries by *count* let a database locked
/// by another process (not just the brief startup race the retries exist for)
/// multiply the busy timeout up to a hundredfold and hang `Store::open` for
/// minutes. Each attempt's busy wait is also capped at the time remaining, or
/// one that starts just before the deadline could still overrun it by a whole
/// busy timeout.
const SCHEMA_LOCK_RETRY_BUDGET: Duration = Duration::from_secs(5);

const SCHEMA_BOOTSTRAP: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER PRIMARY KEY
);
"#;

// Initial recorder schema. Keep this migration immutable so existing on-disk
// databases can advance through later versions deterministically.
const MIGRATION_V1: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    client TEXT NOT NULL,
    server_command TEXT NOT NULL,
    transport TEXT NOT NULL DEFAULT 'stdio',
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    total_messages INTEGER NOT NULL DEFAULT 0,
    dropped_messages INTEGER NOT NULL DEFAULT 0,
    redaction_policy TEXT NOT NULL DEFAULT 'none',
    tags TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    ts INTEGER NOT NULL,
    direction TEXT NOT NULL,
    message_kind TEXT NOT NULL,
    rpc_id TEXT,
    method TEXT,
    tool_name TEXT,
    payload TEXT NOT NULL,
    payload_bytes INTEGER NOT NULL,
    is_error INTEGER NOT NULL DEFAULT 0,
    error_code INTEGER
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_session_seq
ON messages(session_id, seq);

CREATE INDEX IF NOT EXISTS idx_messages_session
ON messages(session_id);

CREATE INDEX IF NOT EXISTS idx_messages_tool
ON messages(tool_name)
WHERE tool_name IS NOT NULL;
"#;

// Persistent derived-memory tables introduced after the initial recorder.
// They remain in storage, while their contents stay owned by mcptracer-intel.
const MIGRATION_V2: &str = r#"
CREATE TABLE IF NOT EXISTS memory_facts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    fact_type TEXT NOT NULL,
    subject_type TEXT NOT NULL,
    subject_key TEXT NOT NULL,
    object_type TEXT,
    object_key TEXT,
    value_json TEXT NOT NULL DEFAULT '{}',
    confidence REAL NOT NULL DEFAULT 1.0,
    session_id TEXT,
    seq_start INTEGER,
    seq_end INTEGER,
    source TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_memory_facts_session
ON memory_facts(session_id);

CREATE INDEX IF NOT EXISTS idx_memory_facts_type_subject
ON memory_facts(fact_type, subject_type, subject_key);

CREATE TABLE IF NOT EXISTS memory_edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    edge_type TEXT NOT NULL,
    from_type TEXT NOT NULL,
    from_key TEXT NOT NULL,
    to_type TEXT NOT NULL,
    to_key TEXT NOT NULL,
    value_json TEXT NOT NULL DEFAULT '{}',
    session_id TEXT,
    source TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_memory_edges_session
ON memory_edges(session_id);

CREATE INDEX IF NOT EXISTS idx_memory_edges_from
ON memory_edges(from_type, from_key);

CREATE INDEX IF NOT EXISTS idx_memory_edges_to
ON memory_edges(to_type, to_key);

CREATE TABLE IF NOT EXISTS tool_versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    server_key TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    description_hash TEXT,
    schema_hash TEXT,
    description_redacted TEXT,
    input_schema_redacted TEXT,
    first_session_id TEXT NOT NULL,
    first_seen_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    UNIQUE(server_key, tool_name, description_hash, schema_hash)
);

CREATE INDEX IF NOT EXISTS idx_tool_versions_tool
ON tool_versions(server_key, tool_name);

CREATE TABLE IF NOT EXISTS tool_version_observations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    server_key TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    version_key TEXT NOT NULL,
    description_hash TEXT,
    schema_hash TEXT,
    seq INTEGER,
    observed_at INTEGER NOT NULL,
    UNIQUE(session_id, server_key, tool_name, version_key)
);

CREATE INDEX IF NOT EXISTS idx_tvo_tool
ON tool_version_observations(server_key, tool_name);

CREATE INDEX IF NOT EXISTS idx_tvo_session
ON tool_version_observations(session_id);

CREATE TABLE IF NOT EXISTS route_definitions (
    id TEXT PRIMARY KEY,
    label TEXT NOT NULL,
    description TEXT NOT NULL,
    threshold REAL NOT NULL DEFAULT 1.0,
    enabled INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS route_decisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    route_id TEXT,
    score REAL NOT NULL,
    threshold REAL NOT NULL,
    abstained INTEGER NOT NULL,
    reasons_json TEXT NOT NULL,
    commands_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
"#;

// Custom redaction key names are provenance: without them, a session labeled
// `custom` cannot later be checked for an unmasked configured field.
const MIGRATION_V3: &str = r#"
ALTER TABLE sessions ADD COLUMN redaction_keys TEXT NOT NULL DEFAULT '[]';
"#;

// Adds contract_hash (covers title/outputSchema/annotations, the fields the
// tool-contract completeness fix added) to tool version identity. SQLite
// cannot add a column to an existing UNIQUE constraint in place, so this
// rebuilds both tables from scratch rather than backfilling a placeholder
// hash into old rows. Safe because tool_versions/tool_version_observations
// are a derived, rebuildable index (see `mcptracer index rebuild`), never
// the source of truth — recorded sessions/messages are untouched. Any
// version_supersedes edges or tool_has_version facts referencing the old
// key format become stale until the next `index rebuild`, the same
// "stale until rebuilt" characteristic this derived index always has.
const MIGRATION_V4: &str = r#"
DROP TABLE IF EXISTS tool_versions;
DROP TABLE IF EXISTS tool_version_observations;

CREATE TABLE tool_versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    server_key TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    description_hash TEXT,
    schema_hash TEXT,
    contract_hash TEXT,
    description_redacted TEXT,
    input_schema_redacted TEXT,
    first_session_id TEXT NOT NULL,
    first_seen_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    UNIQUE(server_key, tool_name, description_hash, schema_hash, contract_hash)
);

CREATE INDEX idx_tool_versions_tool
ON tool_versions(server_key, tool_name);

CREATE TABLE tool_version_observations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    server_key TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    version_key TEXT NOT NULL,
    description_hash TEXT,
    schema_hash TEXT,
    contract_hash TEXT,
    seq INTEGER,
    observed_at INTEGER NOT NULL,
    UNIQUE(session_id, server_key, tool_name, version_key)
);

CREATE INDEX idx_tvo_tool
ON tool_version_observations(server_key, tool_name);

CREATE INDEX idx_tvo_session
ON tool_version_observations(session_id);
"#;

// Named local baselines (T-74): addresses a baseline by
// (project, scenario, environment) rather than a raw session id, with an
// explicit candidate -> approved -> superseded/revoked lifecycle. A row's
// digest is captured only at promotion time (see `Store::promote_baseline`),
// not candidate registration, matching the accept criterion "promotion
// records actor label, artifact digest, and reason".
const MIGRATION_V5: &str = r#"
CREATE TABLE IF NOT EXISTS baselines (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project TEXT NOT NULL,
    scenario TEXT NOT NULL,
    environment TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    state TEXT NOT NULL,
    digest TEXT,
    promoted_by TEXT,
    promoted_at INTEGER,
    promotion_reason TEXT,
    revoked_at INTEGER,
    revoked_reason TEXT,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_baselines_lookup
ON baselines(project, scenario, environment, state);
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParentDirectoryOwnership {
    McpTracer,
    Caller,
}

fn parent_directory_ownership(path: &Path) -> ParentDirectoryOwnership {
    if path == default_db_path() {
        ParentDirectoryOwnership::McpTracer
    } else {
        ParentDirectoryOwnership::Caller
    }
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        Self::open_with_parent_ownership(path, parent_directory_ownership(path))
    }

    fn open_with_parent_ownership(
        path: &Path,
        parent_ownership: ParentDirectoryOwnership,
    ) -> Result<Self> {
        if let Some(parent) = path.parent() {
            // A custom `--db /shared/project/capture.db` path may deliberately
            // live under a directory owned by someone else. Harden directories
            // MCPTracer owns (including the default path) and directories this
            // open creates; never revoke access to an existing caller-managed
            // parent.
            let parent_existed = parent.exists();
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
            if !parent_existed || parent_ownership == ParentDirectoryOwnership::McpTracer {
                harden_dir_permissions(parent);
            } else {
                warn_dir_permissions(parent);
            }
        }

        let conn =
            Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        // T-61: restrict the database file itself to owner-only access.
        harden_file_permissions(path);
        let mut store = Self { conn };
        store.init_schema()?;
        // T-61: init_schema() enables WAL mode above, which creates the
        // -wal/-shm sidecar files; harden those too now that they exist.
        harden_wal_sidecar_files(path);
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let mut store = Self {
            conn: Connection::open_in_memory()?,
        };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&mut self) -> Result<()> {
        // SQLite can return BUSY/LOCKED immediately while another process is
        // switching a new database into WAL mode. Retrying the whole setup is
        // safe: every migration is transactional and schema bootstrap is
        // idempotent. Bounded by wall-clock time, not attempt count: see
        // `SCHEMA_LOCK_RETRY_BUDGET`.
        let deadline = std::time::Instant::now() + SCHEMA_LOCK_RETRY_BUDGET;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            // A zero busy timeout would disable the busy handler entirely.
            self.conn
                .busy_timeout(remaining.clamp(Duration::from_millis(1), BUSY_TIMEOUT))?;
            match self.init_schema_once() {
                Err(error)
                    if is_database_lock_error(&error) && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) if is_database_lock_error(&error) => {
                    return Err(error.context(
                        "database appears to be locked by another process; \
                         gave up waiting for it to become available",
                    ));
                }
                Err(error) => return Err(error),
                Ok(()) => {
                    self.conn.busy_timeout(BUSY_TIMEOUT)?;
                    return Ok(());
                }
            }
        }
    }

    fn init_schema_once(&mut self) -> Result<()> {
        self.conn.execute_batch(CONNECTION_PRAGMAS)?;
        self.conn.execute_batch(SCHEMA_BOOTSTRAP)?;

        // Acquire the write reservation before reading the version. Without
        // this, two processes can both observe the same old version and then
        // race a non-idempotent ALTER TABLE migration.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version = tx
            .query_row(
                "SELECT version FROM schema_version ORDER BY version DESC LIMIT 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0);

        if version < 0 {
            anyhow::bail!("invalid negative schema version: {version}");
        }
        if version > CURRENT_SCHEMA_VERSION {
            anyhow::bail!(
                "database schema version {version} is newer than this MCPTracer build supports ({CURRENT_SCHEMA_VERSION})"
            );
        }

        for target_version in (version + 1)..=CURRENT_SCHEMA_VERSION {
            let migration = match target_version {
                1 => MIGRATION_V1,
                2 => MIGRATION_V2,
                3 => MIGRATION_V3,
                4 => MIGRATION_V4,
                5 => MIGRATION_V5,
                _ => unreachable!("schema migration version is bounded by CURRENT_SCHEMA_VERSION"),
            };
            tx.execute_batch(migration)?;
            tx.execute("DELETE FROM schema_version", [])?;
            tx.execute(
                "INSERT INTO schema_version(version) VALUES(?1)",
                params![target_version],
            )?;
        }
        tx.commit()?;

        Ok(())
    }

    pub fn create_session(
        &self,
        client: &str,
        server_command: &str,
        transport: &str,
        started_at_ns: i64,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO sessions(id, client, server_command, transport, started_at)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![id, client, server_command, transport, started_at_ns],
        )?;
        Ok(id)
    }

    pub fn write_message(&self, session_id: &str, msg: &McpMessage) -> Result<()> {
        self.conn.execute(
            "INSERT INTO messages(
                session_id, seq, ts, direction, message_kind, rpc_id, method,
                tool_name, payload, payload_bytes, is_error, error_code
             )
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                session_id,
                msg.seq as i64,
                msg.timestamp_ns,
                msg.direction.as_db_str(),
                msg.kind().as_db_str(),
                msg.id_json(),
                msg.method(),
                msg.tool_name(),
                msg.payload.to_string(),
                msg.payload_bytes as i64,
                msg.is_error() as i64,
                msg.error_code(),
            ],
        )?;
        Ok(())
    }

    /// Persist a contiguous writer batch in one transaction. The proxy owns a
    /// single `Store` on its writer thread, so batching does not change the
    /// public ordering contract while avoiding one SQLite commit per frame.
    pub fn write_messages<'a>(
        &mut self,
        session_id: &str,
        messages: impl IntoIterator<Item = &'a McpMessage>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        for msg in messages {
            tx.execute(
                "INSERT INTO messages(
                    session_id, seq, ts, direction, message_kind, rpc_id, method,
                    tool_name, payload, payload_bytes, is_error, error_code
                 )
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    session_id,
                    msg.seq as i64,
                    msg.timestamp_ns,
                    msg.direction.as_db_str(),
                    msg.kind().as_db_str(),
                    msg.id_json(),
                    msg.method(),
                    msg.tool_name(),
                    msg.payload.to_string(),
                    msg.payload_bytes as i64,
                    msg.is_error() as i64,
                    msg.error_code(),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_redaction_policy(
        &self,
        session_id: &str,
        policy: &str,
        custom_keys: &[String],
    ) -> Result<()> {
        let custom_keys_json = serde_json::to_string(custom_keys)?;
        self.conn.execute(
            "UPDATE sessions SET redaction_policy = ?1, redaction_keys = ?2 WHERE id = ?3",
            params![policy, custom_keys_json, session_id],
        )?;
        Ok(())
    }

    /// Serialize one session as a validated, gzip-compressed `.mtrace`
    /// artifact. A source without a redaction policy requires either a default
    /// redaction upgrade or an explicit unredacted override.
    pub fn export_mtrace(
        &self,
        session_id_or_prefix: &str,
        options: mtrace::ExportOptions,
    ) -> Result<Vec<u8>> {
        let document = self.export_mtrace_document(session_id_or_prefix, options)?;
        mtrace::encode(&document)
    }

    /// Build the portable document before compression. Exposed for callers that
    /// need to inspect an artifact in memory; it still applies export redaction.
    pub fn export_mtrace_document(
        &self,
        session_id_or_prefix: &str,
        options: mtrace::ExportOptions,
    ) -> Result<mtrace::MtraceDocument> {
        if options.force_default_redaction && options.allow_unredacted {
            anyhow::bail!("--redact default cannot be combined with --allow-unredacted");
        }

        let summary = self.get_session_summary(session_id_or_prefix)?;
        let source_policy = redaction_policy_for_summary(&summary)?;
        let effective_policy = match source_policy {
            RedactionPolicy::None if options.force_default_redaction => RedactionPolicy::Default,
            RedactionPolicy::None if options.allow_unredacted => RedactionPolicy::None,
            RedactionPolicy::None => anyhow::bail!(
                "refusing to export an unredacted session; use --redact default or --allow-unredacted"
            ),
            policy => policy,
        };
        let redactor = Redactor::new(effective_policy.clone());
        let tags = self.get_session_tags(&summary.id)?;
        let messages = self.get_messages(&summary.id)?;
        let messages = messages
            .into_iter()
            .map(|message| {
                let mut payload: serde_json::Value = serde_json::from_str(&message.payload)
                    .with_context(|| format!("stored message {} is not valid JSON", message.seq))?;
                if !payload.is_object() {
                    anyhow::bail!(
                        "stored message {} payload is not a JSON object",
                        message.seq
                    );
                }
                redactor.redact(&mut payload);
                Ok(mtrace::MtraceMessage {
                    seq: message.seq,
                    ts_ns: message.ts_ns,
                    direction: message.direction,
                    message_kind: message.message_kind,
                    rpc_id: message.rpc_id,
                    method: message.method,
                    tool_name: message.tool_name,
                    payload,
                    payload_bytes: message.payload_bytes,
                    is_error: message.is_error,
                    error_code: message.error_code,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(mtrace::MtraceDocument {
            format: mtrace::FORMAT.to_string(),
            version: mtrace::VERSION,
            exported_at_ns: now_ns(),
            exporter: format!("mcptracer/{}", env!("CARGO_PKG_VERSION")),
            session: mtrace::MtraceSession {
                client: summary.client,
                server_command: summary.server_command,
                transport: summary.transport,
                started_at_ns: summary.started_at_ns,
                ended_at_ns: summary.ended_at_ns,
                redaction_policy: effective_policy.as_str().to_string(),
                redaction_keys: effective_policy.custom_keys().to_vec(),
                dropped_messages: summary.dropped_messages,
                tags,
            },
            messages,
        })
    }

    /// Decode, validate, and import a portable session locally. This never
    /// executes imported commands or payloads.
    pub fn import_mtrace(&mut self, bytes: &[u8], strict: bool) -> Result<MtraceImportResult> {
        let document = mtrace::decode(bytes, strict)?;
        self.import_mtrace_document(document)
    }

    /// Persist an already validated document in one transaction.
    pub fn import_mtrace_document(
        &mut self,
        document: mtrace::MtraceDocument,
    ) -> Result<MtraceImportResult> {
        mtrace::validate(&document)?;

        let session_id = Uuid::new_v4().to_string();
        let total_messages =
            i64::try_from(document.messages.len()).context("mtrace message count exceeds i64")?;
        let redaction_keys_json = serde_json::to_string(&document.session.redaction_keys)?;
        let tags_json = serde_json::to_string(&document.session.tags)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO sessions(
                id, client, server_command, transport, started_at, ended_at,
                total_messages, dropped_messages, redaction_policy, redaction_keys, tags
             )
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                session_id,
                document.session.client,
                document.session.server_command,
                document.session.transport,
                document.session.started_at_ns,
                document.session.ended_at_ns,
                total_messages,
                document.session.dropped_messages,
                document.session.redaction_policy,
                redaction_keys_json,
                tags_json,
            ],
        )?;
        for message in &document.messages {
            tx.execute(
                "INSERT INTO messages(
                    session_id, seq, ts, direction, message_kind, rpc_id, method,
                    tool_name, payload, payload_bytes, is_error, error_code
                 )
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    session_id,
                    i64::try_from(message.seq).context("mtrace sequence exceeds i64")?,
                    message.ts_ns,
                    message.direction,
                    message.message_kind,
                    message.rpc_id,
                    message.method,
                    message.tool_name,
                    serde_json::to_string(&message.payload)?,
                    i64::try_from(message.payload_bytes)
                        .context("mtrace payload size exceeds i64")?,
                    message.is_error as i64,
                    message.error_code,
                ],
            )?;
        }
        tx.commit()?;

        Ok(MtraceImportResult {
            session_id,
            total_messages: document.messages.len(),
        })
    }

    /// Import a parsed `.vcr` cassette as a new session. Marked redaction
    /// policy `none`: the cassette carries no redaction metadata.
    pub fn import_vcr(
        &mut self,
        cassette: &vcr::VcrCassette,
        client: &str,
    ) -> Result<vcr::VcrImportResult> {
        let messages = vcr::cassette_to_messages(cassette)?;
        let session_id = self.create_session(client, "imported from .vcr cassette", "stdio", 0)?;
        self.write_messages(&session_id, &messages)?;
        let ended_at_ns = messages.iter().map(|m| m.timestamp_ns).max().unwrap_or(0);
        self.close_session(&session_id, ended_at_ns)?;
        Ok(vcr::VcrImportResult {
            session_id,
            total_messages: messages.len(),
        })
    }

    /// Read parsed tags without exposing raw storage internals to commands.
    pub fn get_session_tags(&self, session_id_or_prefix: &str) -> Result<Vec<String>> {
        let session_id = self.resolve_session_id(session_id_or_prefix)?;
        let tags_json: String = self.conn.query_row(
            "SELECT tags FROM sessions WHERE id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        serde_json::from_str(&tags_json).context("stored session tags are not a JSON string array")
    }

    /// Combine complete stored artifacts in the supplied order. Payloads are
    /// copied verbatim from storage, so this never reintroduces unredacted
    /// content. Sources must use the same persisted redaction configuration.
    pub fn merge_sessions(
        &mut self,
        source_session_ids: &[String],
        deduplicate: bool,
    ) -> Result<MergeResult> {
        if source_session_ids.len() < 2 {
            anyhow::bail!("merge requires at least two source sessions");
        }

        let mut source_ids = HashSet::new();
        let mut sources = Vec::with_capacity(source_session_ids.len());
        for source_session_id in source_session_ids {
            let summary = self.get_session_summary(source_session_id)?;
            if !source_ids.insert(summary.id.clone()) {
                anyhow::bail!("source session was provided more than once: {}", summary.id);
            }
            let messages = self.get_messages(&summary.id)?;
            sources.push((summary, messages));
        }

        let (transport, redaction_policy, redaction_keys_json) = {
            let reference = &sources[0].0;
            if sources.iter().any(|(summary, _)| {
                summary.transport != reference.transport
                    || summary.redaction_policy != reference.redaction_policy
                    || summary.redaction_keys_json != reference.redaction_keys_json
            }) {
                anyhow::bail!(
                    "cannot merge sessions with different transports or redaction configurations"
                );
            }
            (
                reference.transport.clone(),
                reference.redaction_policy.clone(),
                reference.redaction_keys_json.clone(),
            )
        };

        let resolved_source_ids: Vec<String> = sources
            .iter()
            .map(|(summary, _)| summary.id.clone())
            .collect();
        let started_at_ns = sources
            .iter()
            .map(|(summary, _)| summary.started_at_ns)
            .min()
            .context("merge sources unexpectedly had no start time")?;
        let ended_at_ns = if sources
            .iter()
            .all(|(summary, _)| summary.ended_at_ns.is_some())
        {
            sources
                .iter()
                .filter_map(|(summary, _)| summary.ended_at_ns)
                .max()
        } else {
            None
        };
        let dropped_messages = sources.iter().try_fold(0_i64, |total, (summary, _)| {
            total
                .checked_add(summary.dropped_messages)
                .context("merged dropped-message count exceeds i64")
        })?;

        let mut merged_messages = Vec::new();
        let mut deduplicated_calls = 0_usize;
        let mut seen_calls = HashSet::new();
        for (_, messages) in sources {
            if deduplicate {
                let (messages, removed_calls) =
                    deduplicate_source_messages(messages, &mut seen_calls);
                merged_messages.extend(messages);
                deduplicated_calls += removed_calls;
            } else {
                merged_messages.extend(messages);
            }
        }

        let session_id = Uuid::new_v4().to_string();
        let mut tags = vec!["merged".to_string()];
        tags.extend(
            resolved_source_ids
                .iter()
                .map(|source_id| format!("source:{source_id}")),
        );
        let tags_json = serde_json::to_string(&tags)?;
        let total_messages =
            i64::try_from(merged_messages.len()).context("merged message count exceeds i64")?;

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO sessions(
                id, client, server_command, transport, started_at, ended_at,
                total_messages, dropped_messages, redaction_policy, redaction_keys, tags
             )
             VALUES(?1, 'mcptracer merge', 'merged sessions', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                session_id,
                transport,
                started_at_ns,
                ended_at_ns,
                total_messages,
                dropped_messages,
                redaction_policy,
                redaction_keys_json,
                tags_json,
            ],
        )?;
        for (seq, message) in merged_messages.iter().enumerate() {
            tx.execute(
                "INSERT INTO messages(
                    session_id, seq, ts, direction, message_kind, rpc_id, method,
                    tool_name, payload, payload_bytes, is_error, error_code
                 )
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    session_id,
                    i64::try_from(seq).context("merged sequence exceeds i64")?,
                    message.ts_ns,
                    message.direction,
                    message.message_kind,
                    message.rpc_id,
                    message.method,
                    message.tool_name,
                    message.payload,
                    i64::try_from(message.payload_bytes)
                        .context("merged payload size exceeds i64")?,
                    message.is_error as i64,
                    message.error_code,
                ],
            )?;
        }
        tx.commit()?;

        Ok(MergeResult {
            session_id,
            source_session_ids: resolved_source_ids,
            total_messages: merged_messages.len(),
            deduplicated_calls,
        })
    }

    pub fn increment_dropped_messages(&self, session_id: &str, count: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET dropped_messages = dropped_messages + ?1 WHERE id = ?2",
            params![count as i64, session_id],
        )?;
        Ok(())
    }

    pub fn close_session(&self, session_id: &str, ended_at_ns: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET
                ended_at = ?1,
                total_messages = (SELECT COUNT(*) FROM messages WHERE session_id = ?2)
             WHERE id = ?2",
            params![ended_at_ns, session_id],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<SessionSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, client, server_command, transport, started_at, ended_at,
                    total_messages, dropped_messages, redaction_policy, redaction_keys
             FROM sessions
             ORDER BY started_at DESC
             LIMIT ?1",
        )?;

        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                client: row.get(1)?,
                server_command: row.get(2)?,
                transport: row.get(3)?,
                started_at_ns: row.get(4)?,
                ended_at_ns: row.get(5)?,
                total_messages: row.get(6)?,
                dropped_messages: row.get(7)?,
                redaction_policy: row.get(8)?,
                redaction_keys_json: row.get(9)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Load the summary for a full session id or unique prefix.
    pub fn get_session_summary(&self, session_id_or_prefix: &str) -> Result<SessionSummary> {
        let session_id = self.resolve_session_id(session_id_or_prefix)?;
        self.conn
            .query_row(
                "SELECT id, client, server_command, transport, started_at, ended_at,
                        total_messages, dropped_messages, redaction_policy, redaction_keys
                 FROM sessions
                 WHERE id = ?1",
                params![session_id],
                |row| {
                    Ok(SessionSummary {
                        id: row.get(0)?,
                        client: row.get(1)?,
                        server_command: row.get(2)?,
                        transport: row.get(3)?,
                        started_at_ns: row.get(4)?,
                        ended_at_ns: row.get(5)?,
                        total_messages: row.get(6)?,
                        dropped_messages: row.get(7)?,
                        redaction_policy: row.get(8)?,
                        redaction_keys_json: row.get(9)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn get_messages(&self, session_id_or_prefix: &str) -> Result<Vec<StoredMessage>> {
        let session_id = self.resolve_session_id(session_id_or_prefix)?;
        let mut stmt = self.conn.prepare(
            "SELECT seq, ts, direction, message_kind, rpc_id, method, tool_name,
                    payload, payload_bytes, is_error, error_code
             FROM messages
             WHERE session_id = ?1
             ORDER BY seq ASC",
        )?;

        let rows = stmt.query_map(params![session_id], |row| {
            Ok(StoredMessage {
                seq: row.get::<_, i64>(0)? as u64,
                ts_ns: row.get(1)?,
                direction: row.get(2)?,
                message_kind: row.get(3)?,
                rpc_id: row.get(4)?,
                method: row.get(5)?,
                tool_name: row.get(6)?,
                payload: row.get(7)?,
                payload_bytes: row.get::<_, i64>(8)? as usize,
                is_error: row.get::<_, i64>(9)? != 0,
                error_code: row.get(10)?,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn resolve_session_id(&self, session_id_or_prefix: &str) -> Result<String> {
        if session_id_or_prefix.len() == 36 {
            let exact = self
                .conn
                .query_row(
                    "SELECT id FROM sessions WHERE id = ?1",
                    params![session_id_or_prefix],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            return exact
                .ok_or_else(|| anyhow::anyhow!("session not found: {}", session_id_or_prefix));
        }

        let like = format!("{session_id_or_prefix}%");
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM sessions WHERE id LIKE ?1 ORDER BY started_at DESC LIMIT 2")?;
        let ids = stmt
            .query_map(params![like], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        match ids.as_slice() {
            [id] => Ok(id.clone()),
            [] => anyhow::bail!("session not found: {}", session_id_or_prefix),
            _ => anyhow::bail!("session prefix is ambiguous: {}", session_id_or_prefix),
        }
    }

    pub fn get_redaction_policy(&self, session_id_or_prefix: &str) -> Result<String> {
        let session_id = self.resolve_session_id(session_id_or_prefix)?;
        Ok(self.conn.query_row(
            "SELECT redaction_policy FROM sessions WHERE id = ?1",
            params![session_id],
            |row| row.get(0),
        )?)
    }

    /// Search messages across all recorded sessions. Filters combine with AND;
    /// results are newest-first, capped at `query.limit`.
    pub fn search_messages(&self, query: &SearchQuery) -> Result<Vec<MessageHit>> {
        let mut sql = String::from(
            "SELECT session_id, seq, ts, direction, method, tool_name, is_error \
             FROM messages",
        );
        let mut conditions: Vec<String> = Vec::new();
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(tool) = &query.tool {
            bind.push(Box::new(tool.clone()));
            conditions.push(format!("tool_name = ?{}", bind.len()));
        }
        if let Some(method) = &query.method {
            bind.push(Box::new(method.clone()));
            conditions.push(format!("method = ?{}", bind.len()));
        }
        if query.errors_only {
            conditions.push("is_error = 1".to_string());
        }
        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        bind.push(Box::new(query.limit as i64));
        sql.push_str(&format!(" ORDER BY ts DESC LIMIT ?{}", bind.len()));

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(bind.iter()), |row| {
            Ok(MessageHit {
                session_id: row.get(0)?,
                seq: row.get::<_, i64>(1)? as u64,
                ts_ns: row.get(2)?,
                direction: row.get(3)?,
                method: row.get(4)?,
                tool_name: row.get(5)?,
                is_error: row.get::<_, i64>(6)? != 0,
            })
        })?;

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn session_exists(&self, session_id: &str) -> Result<bool> {
        let exists = self
            .conn
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                params![session_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(exists)
    }

    /// Replace all derived data owned by `session_id` in a single transaction:
    /// per-session facts, per-session edges, and per-session tool-version
    /// observations are deleted and re-inserted, and observed tool versions are
    /// upserted into the global `tool_versions` table. Cross-session
    /// `version_supersedes` edges are managed separately by
    /// [`Store::replace_supersession_edges`]; run that after rebuilding.
    ///
    /// Idempotent: rebuilding the same session with the same extraction yields
    /// the same rows and never duplicates.
    pub fn rebuild_memory_for_session(
        &mut self,
        session_id: &str,
        facts: &[MemoryFactRecord],
        edges: &[MemoryEdgeRecord],
        tool_versions: &[ToolVersionRecord],
        observations: &[ToolVersionObservationRecord],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;

        tx.execute(
            "DELETE FROM memory_facts WHERE session_id = ?1",
            params![session_id],
        )?;
        // Only per-session edges are owned by this session; cross-session
        // supersession edges (session_id IS NULL) are left untouched here.
        tx.execute(
            "DELETE FROM memory_edges WHERE session_id = ?1",
            params![session_id],
        )?;
        tx.execute(
            "DELETE FROM tool_version_observations WHERE session_id = ?1",
            params![session_id],
        )?;

        for fact in facts {
            let seq_start = fact.seq_start.map(|seq| seq as i64);
            let seq_end = fact.seq_end.map(|seq| seq as i64);
            tx.execute(
                "INSERT INTO memory_facts(
                    fact_type, subject_type, subject_key, object_type, object_key,
                    value_json, confidence, session_id, seq_start, seq_end,
                    source, observed_at, created_at
                 )
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    fact.fact_type.as_str(),
                    fact.subject_type.as_str(),
                    fact.subject_key.as_str(),
                    fact.object_type.as_deref(),
                    fact.object_key.as_deref(),
                    fact.value_json.as_str(),
                    fact.confidence,
                    fact.session_id.as_deref(),
                    seq_start,
                    seq_end,
                    fact.source.as_str(),
                    fact.observed_at_ns,
                    fact.created_at_ns,
                ],
            )?;
        }

        for edge in edges {
            tx.execute(
                "INSERT INTO memory_edges(
                    edge_type, from_type, from_key, to_type, to_key,
                    value_json, session_id, source, observed_at, created_at
                 )
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    edge.edge_type.as_str(),
                    edge.from_type.as_str(),
                    edge.from_key.as_str(),
                    edge.to_type.as_str(),
                    edge.to_key.as_str(),
                    edge.value_json.as_str(),
                    edge.session_id.as_deref(),
                    edge.source.as_str(),
                    edge.observed_at_ns,
                    edge.created_at_ns,
                ],
            )?;
        }

        for version in tool_versions {
            tx.execute(
                "INSERT INTO tool_versions(
                    server_key, tool_name, description_hash, schema_hash, contract_hash,
                    description_redacted, input_schema_redacted, first_session_id,
                    first_seen_at, last_seen_at
                 )
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(server_key, tool_name, description_hash, schema_hash, contract_hash)
                 DO UPDATE SET
                    first_session_id = CASE
                        WHEN excluded.first_seen_at < tool_versions.first_seen_at
                        THEN excluded.first_session_id
                        ELSE tool_versions.first_session_id
                    END,
                    first_seen_at = MIN(tool_versions.first_seen_at, excluded.first_seen_at),
                    last_seen_at = MAX(tool_versions.last_seen_at, excluded.last_seen_at)",
                params![
                    version.server_key.as_str(),
                    version.tool_name.as_str(),
                    version.description_hash.as_deref(),
                    version.schema_hash.as_deref(),
                    version.contract_hash.as_deref(),
                    version.description_redacted.as_deref(),
                    version.input_schema_redacted.as_deref(),
                    version.first_session_id.as_str(),
                    version.first_seen_at_ns,
                    version.last_seen_at_ns,
                ],
            )?;
        }

        for observation in observations {
            tx.execute(
                "INSERT INTO tool_version_observations(
                    session_id, server_key, tool_name, version_key,
                    description_hash, schema_hash, contract_hash, seq, observed_at
                 )
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(session_id, server_key, tool_name, version_key)
                 DO UPDATE SET
                    description_hash = excluded.description_hash,
                    schema_hash = excluded.schema_hash,
                    contract_hash = excluded.contract_hash,
                    seq = excluded.seq,
                    observed_at = excluded.observed_at",
                params![
                    observation.session_id.as_str(),
                    observation.server_key.as_str(),
                    observation.tool_name.as_str(),
                    observation.version_key.as_str(),
                    observation.description_hash.as_deref(),
                    observation.schema_hash.as_deref(),
                    observation.contract_hash.as_deref(),
                    observation.seq.map(|seq| seq as i64),
                    observation.observed_at_ns,
                ],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Replace the cross-session `version_supersedes` edge set atomically. These
    /// edges have `session_id = NULL`, so they are not owned by any single
    /// session's rebuild. Delete-all-then-insert keeps the derived supersession
    /// graph free of duplicates when recomputed.
    pub fn replace_supersession_edges(&mut self, edges: &[MemoryEdgeRecord]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM memory_edges WHERE edge_type = 'version_supersedes'",
            [],
        )?;
        for edge in edges {
            tx.execute(
                "INSERT INTO memory_edges(
                    edge_type, from_type, from_key, to_type, to_key,
                    value_json, session_id, source, observed_at, created_at
                 )
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    edge.edge_type.as_str(),
                    edge.from_type.as_str(),
                    edge.from_key.as_str(),
                    edge.to_type.as_str(),
                    edge.to_key.as_str(),
                    edge.value_json.as_str(),
                    edge.session_id.as_deref(),
                    edge.source.as_str(),
                    edge.observed_at_ns,
                    edge.created_at_ns,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Resolve a full session id or unique prefix to its canonical id.
    pub fn resolve_session(&self, session_id_or_prefix: &str) -> Result<String> {
        self.resolve_session_id(session_id_or_prefix)
    }

    /// All recorded session ids, oldest first (deterministic rebuild order).
    pub fn all_session_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM sessions ORDER BY started_at ASC, id ASC")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// The stored `server_command` for a session (raw, joined args). Callers
    /// that persist derived memory must hash this into an opaque server key
    /// rather than copying it into facts.
    pub fn get_server_command(&self, session_id_or_prefix: &str) -> Result<String> {
        let session_id = self.resolve_session_id(session_id_or_prefix)?;
        Ok(self.conn.query_row(
            "SELECT server_command FROM sessions WHERE id = ?1",
            params![session_id],
            |row| row.get(0),
        )?)
    }

    pub fn list_tool_version_observations(
        &self,
        session_id_or_prefix: Option<&str>,
    ) -> Result<Vec<ToolVersionObservationRow>> {
        if let Some(session) = session_id_or_prefix {
            let session_id = self.resolve_session_id(session)?;
            let mut stmt = self.conn.prepare(
                "SELECT id, session_id, server_key, tool_name, version_key,
                        description_hash, schema_hash, contract_hash, seq, observed_at
                 FROM tool_version_observations
                 WHERE session_id = ?1
                 ORDER BY id ASC",
            )?;
            let rows = stmt.query_map(params![session_id], tool_version_observation_from_row)?;
            return rows
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Into::into);
        }

        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, server_key, tool_name, version_key,
                    description_hash, schema_hash, contract_hash, seq, observed_at
             FROM tool_version_observations
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], tool_version_observation_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn list_memory_facts(
        &self,
        session_id_or_prefix: Option<&str>,
    ) -> Result<Vec<MemoryFactRow>> {
        if let Some(session) = session_id_or_prefix {
            let session_id = self.resolve_session_id(session)?;
            let mut stmt = self.conn.prepare(
                "SELECT id, fact_type, subject_type, subject_key, object_type,
                        object_key, value_json, confidence, session_id, seq_start,
                        seq_end, source, observed_at, created_at
                 FROM memory_facts
                 WHERE session_id = ?1
                 ORDER BY id ASC",
            )?;
            let rows = stmt.query_map(params![session_id], memory_fact_from_row)?;
            return rows
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Into::into);
        }

        let mut stmt = self.conn.prepare(
            "SELECT id, fact_type, subject_type, subject_key, object_type,
                    object_key, value_json, confidence, session_id, seq_start,
                    seq_end, source, observed_at, created_at
             FROM memory_facts
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], memory_fact_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn list_memory_edges(
        &self,
        session_id_or_prefix: Option<&str>,
    ) -> Result<Vec<MemoryEdgeRow>> {
        if let Some(session) = session_id_or_prefix {
            let session_id = self.resolve_session_id(session)?;
            let mut stmt = self.conn.prepare(
                "SELECT id, edge_type, from_type, from_key, to_type, to_key,
                        value_json, session_id, source, observed_at, created_at
                 FROM memory_edges
                 WHERE session_id = ?1
                 ORDER BY id ASC",
            )?;
            let rows = stmt.query_map(params![session_id], memory_edge_from_row)?;
            return rows
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Into::into);
        }

        let mut stmt = self.conn.prepare(
            "SELECT id, edge_type, from_type, from_key, to_type, to_key,
                    value_json, session_id, source, observed_at, created_at
             FROM memory_edges
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], memory_edge_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn list_tool_versions(&self) -> Result<Vec<ToolVersionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, server_key, tool_name, description_hash, schema_hash, contract_hash,
                    description_redacted, input_schema_redacted, first_session_id,
                    first_seen_at, last_seen_at
             FROM tool_versions
             ORDER BY server_key ASC, tool_name ASC, first_seen_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], tool_version_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Register `session_id` as a candidate baseline for
    /// `(project, scenario, environment)`. Multiple simultaneous candidates
    /// for the same triple are allowed (e.g. comparing two approaches before
    /// picking one); nothing about this call affects which baseline, if any,
    /// is currently approved for that triple.
    pub fn create_baseline_candidate(
        &self,
        project: &str,
        scenario: &str,
        environment: &str,
        session_id: &str,
        created_at_ns: i64,
    ) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO baselines(project, scenario, environment, session_id, state, created_at)
                 VALUES(?1, ?2, ?3, ?4, 'candidate', ?5)",
                params![project, scenario, environment, session_id, created_at_ns],
            )
            .context("failed to register baseline candidate")?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Promote the candidate row matching `(project, scenario, environment,
    /// session_id)` to approved, superseding any baseline already approved
    /// for the same `(project, scenario, environment)`. `digest` should be
    /// `mtrace::canonical_digest` computed over the same session — callers
    /// pass it in rather than this function computing it, since that
    /// requires `mtrace::ExportOptions` policy decisions this storage-layer
    /// function has no business making. Atomic: either both the supersession
    /// and the new approval land, or neither does.
    #[allow(clippy::too_many_arguments)]
    pub fn promote_baseline(
        &mut self,
        project: &str,
        scenario: &str,
        environment: &str,
        session_id: &str,
        digest: &str,
        promoted_by: &str,
        reason: &str,
        promoted_at_ns: i64,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        let candidate_id: i64 = tx
            .query_row(
                "SELECT id FROM baselines
                 WHERE project = ?1 AND scenario = ?2 AND environment = ?3
                   AND session_id = ?4 AND state = 'candidate'",
                params![project, scenario, environment, session_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no candidate baseline for project={project} scenario={scenario} \
                     environment={environment} session={session_id}; register one first \
                     with `mcptracer baseline candidate`"
                )
            })?;

        tx.execute(
            "UPDATE baselines SET state = 'superseded'
             WHERE project = ?1 AND scenario = ?2 AND environment = ?3 AND state = 'approved'",
            params![project, scenario, environment],
        )?;
        tx.execute(
            "UPDATE baselines
             SET state = 'approved', digest = ?1, promoted_by = ?2, promoted_at = ?3, promotion_reason = ?4
             WHERE id = ?5",
            params![digest, promoted_by, promoted_at_ns, reason, candidate_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Revoke the currently approved baseline for
    /// `(project, scenario, environment)`, if any. After this,
    /// `resolve_approved_baseline` finds nothing for that triple until a new
    /// baseline is promoted.
    pub fn revoke_baseline(
        &self,
        project: &str,
        scenario: &str,
        environment: &str,
        reason: &str,
        revoked_at_ns: i64,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE baselines SET state = 'revoked', revoked_at = ?1, revoked_reason = ?2
             WHERE project = ?3 AND scenario = ?4 AND environment = ?5 AND state = 'approved'",
            params![revoked_at_ns, reason, project, scenario, environment],
        )?;
        if changed == 0 {
            anyhow::bail!(
                "no approved baseline for project={project} scenario={scenario} environment={environment}"
            );
        }
        Ok(())
    }

    /// Resolve the currently approved baseline for
    /// `(project, scenario, environment)`. This is the one lookup a CI
    /// workflow should use instead of hardcoding a transient session id.
    pub fn resolve_approved_baseline(
        &self,
        project: &str,
        scenario: &str,
        environment: &str,
    ) -> Result<Baseline> {
        self.conn
            .query_row(
                "SELECT id, project, scenario, environment, session_id, state, digest,
                        promoted_by, promoted_at, promotion_reason, revoked_at, revoked_reason,
                        created_at
                 FROM baselines
                 WHERE project = ?1 AND scenario = ?2 AND environment = ?3 AND state = 'approved'",
                params![project, scenario, environment],
                baseline_from_row,
            )
            .optional()?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no approved baseline for project={project} scenario={scenario} environment={environment}"
                )
            })
    }

    /// List baselines in every state, most recently created first, optionally
    /// filtered to one project.
    pub fn list_baselines(&self, project: Option<&str>) -> Result<Vec<Baseline>> {
        let sql = "SELECT id, project, scenario, environment, session_id, state, digest,
                          promoted_by, promoted_at, promotion_reason, revoked_at, revoked_reason,
                          created_at
                   FROM baselines
                   WHERE ?1 IS NULL OR project = ?1
                   ORDER BY created_at DESC, id DESC";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![project], baseline_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

fn baseline_from_row(row: &Row<'_>) -> rusqlite::Result<Baseline> {
    Ok(Baseline {
        id: row.get(0)?,
        project: row.get(1)?,
        scenario: row.get(2)?,
        environment: row.get(3)?,
        session_id: row.get(4)?,
        state: row.get(5)?,
        digest: row.get(6)?,
        promoted_by: row.get(7)?,
        promoted_at: row.get(8)?,
        promotion_reason: row.get(9)?,
        revoked_at: row.get(10)?,
        revoked_reason: row.get(11)?,
        created_at: row.get(12)?,
    })
}

fn memory_fact_from_row(row: &Row<'_>) -> rusqlite::Result<MemoryFactRow> {
    Ok(MemoryFactRow {
        id: row.get(0)?,
        fact_type: row.get(1)?,
        subject_type: row.get(2)?,
        subject_key: row.get(3)?,
        object_type: row.get(4)?,
        object_key: row.get(5)?,
        value_json: row.get(6)?,
        confidence: row.get(7)?,
        session_id: row.get(8)?,
        seq_start: row.get::<_, Option<i64>>(9)?.map(|seq| seq as u64),
        seq_end: row.get::<_, Option<i64>>(10)?.map(|seq| seq as u64),
        source: row.get(11)?,
        observed_at_ns: row.get(12)?,
        created_at_ns: row.get(13)?,
    })
}

fn memory_edge_from_row(row: &Row<'_>) -> rusqlite::Result<MemoryEdgeRow> {
    Ok(MemoryEdgeRow {
        id: row.get(0)?,
        edge_type: row.get(1)?,
        from_type: row.get(2)?,
        from_key: row.get(3)?,
        to_type: row.get(4)?,
        to_key: row.get(5)?,
        value_json: row.get(6)?,
        session_id: row.get(7)?,
        source: row.get(8)?,
        observed_at_ns: row.get(9)?,
        created_at_ns: row.get(10)?,
    })
}

fn tool_version_from_row(row: &Row<'_>) -> rusqlite::Result<ToolVersionRow> {
    Ok(ToolVersionRow {
        id: row.get(0)?,
        server_key: row.get(1)?,
        tool_name: row.get(2)?,
        description_hash: row.get(3)?,
        schema_hash: row.get(4)?,
        contract_hash: row.get(5)?,
        description_redacted: row.get(6)?,
        input_schema_redacted: row.get(7)?,
        first_session_id: row.get(8)?,
        first_seen_at_ns: row.get(9)?,
        last_seen_at_ns: row.get(10)?,
    })
}

fn tool_version_observation_from_row(row: &Row<'_>) -> rusqlite::Result<ToolVersionObservationRow> {
    Ok(ToolVersionObservationRow {
        id: row.get(0)?,
        session_id: row.get(1)?,
        server_key: row.get(2)?,
        tool_name: row.get(3)?,
        version_key: row.get(4)?,
        description_hash: row.get(5)?,
        schema_hash: row.get(6)?,
        contract_hash: row.get(7)?,
        seq: row.get::<_, Option<i64>>(8)?.map(|seq| seq as u64),
        observed_at_ns: row.get(9)?,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryFactRecord {
    pub fact_type: String,
    pub subject_type: String,
    pub subject_key: String,
    pub object_type: Option<String>,
    pub object_key: Option<String>,
    pub value_json: String,
    pub confidence: f64,
    pub session_id: Option<String>,
    pub seq_start: Option<u64>,
    pub seq_end: Option<u64>,
    pub source: String,
    pub observed_at_ns: i64,
    pub created_at_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryFactRow {
    pub id: i64,
    pub fact_type: String,
    pub subject_type: String,
    pub subject_key: String,
    pub object_type: Option<String>,
    pub object_key: Option<String>,
    pub value_json: String,
    pub confidence: f64,
    pub session_id: Option<String>,
    pub seq_start: Option<u64>,
    pub seq_end: Option<u64>,
    pub source: String,
    pub observed_at_ns: i64,
    pub created_at_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryEdgeRecord {
    pub edge_type: String,
    pub from_type: String,
    pub from_key: String,
    pub to_type: String,
    pub to_key: String,
    pub value_json: String,
    pub session_id: Option<String>,
    pub source: String,
    pub observed_at_ns: i64,
    pub created_at_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryEdgeRow {
    pub id: i64,
    pub edge_type: String,
    pub from_type: String,
    pub from_key: String,
    pub to_type: String,
    pub to_key: String,
    pub value_json: String,
    pub session_id: Option<String>,
    pub source: String,
    pub observed_at_ns: i64,
    pub created_at_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolVersionRecord {
    pub server_key: String,
    pub tool_name: String,
    pub description_hash: Option<String>,
    pub schema_hash: Option<String>,
    pub contract_hash: Option<String>,
    pub description_redacted: Option<String>,
    pub input_schema_redacted: Option<String>,
    pub first_session_id: String,
    pub first_seen_at_ns: i64,
    pub last_seen_at_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolVersionRow {
    pub id: i64,
    pub server_key: String,
    pub tool_name: String,
    pub description_hash: Option<String>,
    pub schema_hash: Option<String>,
    pub contract_hash: Option<String>,
    pub description_redacted: Option<String>,
    pub input_schema_redacted: Option<String>,
    pub first_session_id: String,
    pub first_seen_at_ns: i64,
    pub last_seen_at_ns: i64,
}

/// One session's observation of a specific tool version. Provenance for which
/// session saw which version; the basis for deterministic supersession.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolVersionObservationRecord {
    pub session_id: String,
    pub server_key: String,
    pub tool_name: String,
    pub version_key: String,
    pub description_hash: Option<String>,
    pub schema_hash: Option<String>,
    pub contract_hash: Option<String>,
    pub seq: Option<u64>,
    pub observed_at_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolVersionObservationRow {
    pub id: i64,
    pub session_id: String,
    pub server_key: String,
    pub tool_name: String,
    pub version_key: String,
    pub description_hash: Option<String>,
    pub schema_hash: Option<String>,
    pub contract_hash: Option<String>,
    pub seq: Option<u64>,
    pub observed_at_ns: i64,
}

/// Filter for [`Store::search_messages`]. All set filters combine with AND.
#[derive(Debug, Default)]
pub struct SearchQuery {
    pub tool: Option<String>,
    pub method: Option<String>,
    pub errors_only: bool,
    pub limit: usize,
}

/// One message match from [`Store::search_messages`], carrying the owning
/// session id so callers can jump to `sessions show`.
#[derive(Debug)]
pub struct MessageHit {
    pub session_id: String,
    pub seq: u64,
    pub ts_ns: i64,
    pub direction: String,
    pub method: Option<String>,
    pub tool_name: Option<String>,
    pub is_error: bool,
}

/// A named local baseline (T-74): one row in its
/// candidate/approved/superseded/revoked lifecycle for one
/// `(project, scenario, environment)` triple. See
/// `docs/spec/baselines.md`.
#[derive(Debug, Clone, Serialize)]
pub struct Baseline {
    pub id: i64,
    pub project: String,
    pub scenario: String,
    pub environment: String,
    pub session_id: String,
    pub state: String,
    pub digest: Option<String>,
    pub promoted_by: Option<String>,
    pub promoted_at: Option<i64>,
    pub promotion_reason: Option<String>,
    pub revoked_at: Option<i64>,
    pub revoked_reason: Option<String>,
    pub created_at: i64,
}

fn is_database_lock_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(sqlite_error, _))
                if matches!(sqlite_error.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
        )
    })
}

#[derive(Debug)]
pub struct SessionSummary {
    pub id: String,
    pub client: String,
    pub server_command: String,
    pub transport: String,
    pub started_at_ns: i64,
    pub ended_at_ns: Option<i64>,
    pub total_messages: i64,
    pub dropped_messages: i64,
    pub redaction_policy: String,
    pub redaction_keys_json: String,
}

#[derive(Debug)]
pub struct StoredMessage {
    pub seq: u64,
    pub ts_ns: i64,
    pub direction: String,
    pub message_kind: String,
    pub rpc_id: Option<String>,
    pub method: Option<String>,
    pub tool_name: Option<String>,
    pub payload: String,
    pub payload_bytes: usize,
    pub is_error: bool,
    pub error_code: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct MtraceImportResult {
    pub session_id: String,
    pub total_messages: usize,
}

/// The durable result of combining sessions. Source ids are retained in both
/// this result and the merged session's tags for later provenance inspection.
#[derive(Debug, Serialize)]
pub struct MergeResult {
    pub session_id: String,
    pub source_session_ids: Vec<String>,
    pub total_messages: usize,
    pub deduplicated_calls: usize,
}

fn redaction_policy_for_summary(summary: &SessionSummary) -> Result<RedactionPolicy> {
    let custom_keys: Vec<String> = serde_json::from_str(&summary.redaction_keys_json)
        .context("stored custom redaction keys are not a JSON string array")?;
    RedactionPolicy::from_storage(&summary.redaction_policy, &custom_keys)
        .map_err(|detail| anyhow::anyhow!("invalid stored redaction policy: {detail}"))
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct DeduplicationKey {
    method: String,
    tool_name: Option<String>,
    normalized_params: String,
}

fn deduplicate_source_messages(
    messages: Vec<StoredMessage>,
    seen_calls: &mut HashSet<DeduplicationKey>,
) -> (Vec<StoredMessage>, usize) {
    let mut removed_sequences = HashSet::new();
    let mut pending_requests = HashMap::new();
    let mut removed_calls = 0;

    for message in &messages {
        if message.direction == "c2s" && message.message_kind == "request" {
            let remove_request = deduplication_key(message)
                .map(|key| !seen_calls.insert(key))
                .unwrap_or(false);
            if remove_request {
                removed_sequences.insert(message.seq);
                removed_calls += 1;
            }
            if let Some(rpc_id) = &message.rpc_id {
                pending_requests.insert(rpc_id.clone(), remove_request);
            }
        } else if message.direction == "s2c" && message.message_kind == "response" {
            if let Some(rpc_id) = &message.rpc_id {
                if pending_requests.remove(rpc_id).unwrap_or(false) {
                    removed_sequences.insert(message.seq);
                }
            }
        }
    }

    (
        messages
            .into_iter()
            .filter(|message| !removed_sequences.contains(&message.seq))
            .collect(),
        removed_calls,
    )
}

fn deduplication_key(message: &StoredMessage) -> Option<DeduplicationKey> {
    let method = message.method.clone()?;
    let payload: serde_json::Value = serde_json::from_str(&message.payload).ok()?;
    let params = payload
        .as_object()?
        .get("params")
        .cloned()
        .unwrap_or_default();
    if contains_redacted_value(&params) {
        return None;
    }

    Some(DeduplicationKey {
        method,
        tool_name: message.tool_name.clone(),
        normalized_params: canonical_json(&params),
    })
}

fn contains_redacted_value(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => text == REDACTED_PLACEHOLDER,
        serde_json::Value::Array(values) => values.iter().any(contains_redacted_value),
        serde_json::Value::Object(values) => values.values().any(contains_redacted_value),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            false
        }
    }
}

fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::String(value) => serde_json::to_string(value).unwrap_or_default(),
        serde_json::Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        serde_json::Value::Object(values) => {
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by_key(|(key, _)| *key);
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_json(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

pub fn default_db_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mcptracer")
        .join("sessions.db")
}

#[cfg(test)]
mod tests {
    use mcptracer_protocol::Direction;
    use rusqlite::Connection;
    use serde_json::json;

    use super::*;

    fn make_message(seq: u64, method: &str, tool: Option<&str>) -> McpMessage {
        let payload = if let Some(tool_name) = tool {
            json!({
                "jsonrpc": "2.0",
                "id": seq,
                "method": method,
                "params": {"name": tool_name, "arguments": {}}
            })
        } else {
            json!({"jsonrpc": "2.0", "id": seq, "method": method, "params": {}})
        };

        McpMessage {
            seq,
            timestamp_ns: 1_000_000_000 * seq as i64,
            direction: Direction::ClientToServer,
            payload,
            payload_bytes: 64,
        }
    }

    fn write_tool_exchange(
        store: &Store,
        session_id: &str,
        request_seq: u64,
        response_seq: u64,
        rpc_id: i64,
        arguments: serde_json::Value,
    ) {
        store
            .write_message(
                session_id,
                &McpMessage {
                    seq: request_seq,
                    timestamp_ns: request_seq as i64,
                    direction: Direction::ClientToServer,
                    payload: json!({
                        "jsonrpc": "2.0",
                        "id": rpc_id,
                        "method": "tools/call",
                        "params": {"name": "echo", "arguments": arguments},
                    }),
                    payload_bytes: 64,
                },
            )
            .unwrap();
        store
            .write_message(
                session_id,
                &McpMessage {
                    seq: response_seq,
                    timestamp_ns: response_seq as i64,
                    direction: Direction::ServerToClient,
                    payload: json!({
                        "jsonrpc": "2.0",
                        "id": rpc_id,
                        "result": {"content": []},
                    }),
                    payload_bytes: 32,
                },
            )
            .unwrap();
    }

    #[test]
    fn creates_and_lists_session() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .create_session("codex", "fake-server", "stdio", 100)
            .unwrap();

        let sessions = store.list_sessions(10).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, id);
        assert!(store.session_exists(&id).unwrap());
    }

    #[test]
    fn rejects_unknown_full_session_ids() {
        let store = Store::open_in_memory().unwrap();
        let missing = "00000000-0000-0000-0000-000000000000";

        assert!(store.get_messages(missing).is_err());
        assert!(store.get_session_summary(missing).is_err());
    }

    #[test]
    fn memory_schema_initializes_empty() {
        let store = Store::open_in_memory().unwrap();

        assert!(store.list_memory_facts(None).unwrap().is_empty());
        assert!(store.list_memory_edges(None).unwrap().is_empty());
        assert!(store.list_tool_versions().unwrap().is_empty());
        assert!(store
            .list_tool_version_observations(None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn supersession_edges_replace_without_duplicates() {
        let mut store = Store::open_in_memory().unwrap();
        let edge = MemoryEdgeRecord {
            edge_type: "version_supersedes".to_string(),
            from_type: "tool_version".to_string(),
            from_key: "srv::echo::v2".to_string(),
            to_type: "tool_version".to_string(),
            to_key: "srv::echo::v1".to_string(),
            value_json: "{}".to_string(),
            session_id: None,
            source: "test".to_string(),
            observed_at_ns: 10,
            created_at_ns: 10,
        };

        store
            .replace_supersession_edges(std::slice::from_ref(&edge))
            .unwrap();
        store.replace_supersession_edges(&[edge]).unwrap();

        let edges = store.list_memory_edges(None).unwrap();
        assert_eq!(
            edges.len(),
            1,
            "replace must not duplicate supersession edges"
        );
        assert_eq!(edges[0].edge_type, "version_supersedes");
        assert!(edges[0].session_id.is_none());
    }

    #[test]
    fn migrates_a_v1_database_to_the_current_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_BOOTSTRAP).unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute("INSERT INTO schema_version(version) VALUES(1)", [])
            .unwrap();

        let mut store = Store { conn };
        store.init_schema().unwrap();

        let version: i64 = store
            .conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        let has_memory_facts: bool = store
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'memory_facts')",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        assert!(has_memory_facts);
        assert!(store.list_memory_facts(None).unwrap().is_empty());

        let session_id = store.create_session("test", "server", "stdio", 0).unwrap();
        assert_eq!(
            store
                .get_session_summary(&session_id)
                .unwrap()
                .redaction_keys_json,
            "[]"
        );
    }

    #[test]
    fn migrates_a_v2_database_with_sessions_to_the_current_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_BOOTSTRAP).unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute_batch(MIGRATION_V2).unwrap();
        conn.execute("INSERT INTO schema_version(version) VALUES(2)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO sessions(id, client, server_command, transport, started_at)
             VALUES('legacy-session', 'test', 'server', 'stdio', 0)",
            [],
        )
        .unwrap();

        let mut store = Store { conn };
        store.init_schema().unwrap();

        let summary = store.get_session_summary("legacy-session").unwrap();
        assert_eq!(summary.redaction_policy, "none");
        assert_eq!(summary.redaction_keys_json, "[]");
    }

    #[test]
    fn migrates_a_v3_database_with_tool_versions_to_the_current_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_BOOTSTRAP).unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute_batch(MIGRATION_V2).unwrap();
        conn.execute_batch(MIGRATION_V3).unwrap();
        conn.execute("INSERT INTO schema_version(version) VALUES(3)", [])
            .unwrap();
        // Pre-V4 tool_versions row using the old 4-part unique key, with no
        // contract_hash column at all.
        conn.execute(
            "INSERT INTO tool_versions(
                server_key, tool_name, description_hash, schema_hash,
                first_session_id, first_seen_at, last_seen_at
             )
             VALUES('srv', 'echo', 'old-desc', 'old-schema', 'legacy-session', 0, 0)",
            [],
        )
        .unwrap();

        let mut store = Store { conn };
        store.init_schema().unwrap();

        let version: i64 = store
            .conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        // The pre-V4 row is gone: tool_versions/tool_version_observations are
        // a derived, rebuildable index, not source-of-truth data, so this
        // migration drops and recreates them rather than backfilling.
        assert!(store.list_tool_versions().unwrap().is_empty());

        let has_contract_hash_column: bool = store
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tool_versions') WHERE name = 'contract_hash')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(has_contract_hash_column);

        let session_id = store.create_session("test", "server", "stdio", 0).unwrap();
        store
            .rebuild_memory_for_session(
                &session_id,
                &[],
                &[],
                &[ToolVersionRecord {
                    server_key: "srv".to_string(),
                    tool_name: "echo".to_string(),
                    description_hash: Some("new-desc".to_string()),
                    schema_hash: Some("new-schema".to_string()),
                    contract_hash: Some("new-contract".to_string()),
                    description_redacted: None,
                    input_schema_redacted: None,
                    first_session_id: session_id.clone(),
                    first_seen_at_ns: 0,
                    last_seen_at_ns: 0,
                }],
                &[ToolVersionObservationRecord {
                    session_id: session_id.clone(),
                    server_key: "srv".to_string(),
                    tool_name: "echo".to_string(),
                    version_key: "srv::echo::new-desc::new-schema::new-contract".to_string(),
                    description_hash: Some("new-desc".to_string()),
                    schema_hash: Some("new-schema".to_string()),
                    contract_hash: Some("new-contract".to_string()),
                    seq: Some(0),
                    observed_at_ns: 0,
                }],
            )
            .unwrap();

        let versions = store.list_tool_versions().unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].contract_hash.as_deref(), Some("new-contract"));
    }

    #[test]
    fn concurrent_opens_serialize_schema_migrations() {
        let path =
            std::env::temp_dir().join(format!("mcptracer-schema-race-{}.db", Uuid::new_v4()));
        let path = std::sync::Arc::new(path);
        let start = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut handles = Vec::new();

        for _ in 0..2 {
            let path = std::sync::Arc::clone(&path);
            let start = std::sync::Arc::clone(&start);
            handles.push(std::thread::spawn(move || {
                start.wait();
                let store = Store::open(path.as_ref())?;
                let version =
                    store
                        .conn
                        .query_row("SELECT version FROM schema_version", [], |row| {
                            row.get::<_, i64>(0)
                        })?;
                Ok::<_, anyhow::Error>(version)
            }));
        }

        start.wait();
        for handle in handles {
            assert_eq!(
                handle.join().expect("migration thread panicked").unwrap(),
                CURRENT_SCHEMA_VERSION
            );
        }

        for suffix in ["", "-wal", "-shm"] {
            let mut candidate = path.as_os_str().to_owned();
            candidate.push(suffix);
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn rejects_databases_newer_than_this_build() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_BOOTSTRAP).unwrap();
        conn.execute(
            "INSERT INTO schema_version(version) VALUES(?1)",
            params![CURRENT_SCHEMA_VERSION + 1],
        )
        .unwrap();
        let mut store = Store { conn };

        let err = store.init_schema().unwrap_err();

        assert!(err.to_string().contains("newer than this MCPTracer build"));
    }

    #[test]
    fn writes_and_reads_messages() {
        let store = Store::open_in_memory().unwrap();
        let session_id = store
            .create_session("test", "fake-server", "stdio", 0)
            .unwrap();

        store
            .write_message(&session_id, &make_message(1, "initialize", None))
            .unwrap();
        store
            .write_message(&session_id, &make_message(2, "tools/call", Some("echo")))
            .unwrap();

        let messages = store.get_messages(&session_id[..8]).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].method.as_deref(), Some("initialize"));
        assert_eq!(messages[1].tool_name.as_deref(), Some("echo"));
    }

    #[test]
    fn message_batches_are_atomic() {
        let mut store = Store::open_in_memory().unwrap();
        let session_id = store
            .create_session("test", "fake-server", "stdio", 0)
            .unwrap();
        let first = make_message(1, "initialize", None);
        let duplicate = make_message(1, "tools/list", None);

        assert!(store
            .write_messages(&session_id, [&first, &duplicate])
            .is_err());

        assert!(store.get_messages(&session_id).unwrap().is_empty());
    }

    #[test]
    fn merge_resequences_messages_in_source_order() {
        let mut store = Store::open_in_memory().unwrap();
        let first = store.create_session("one", "server", "stdio", 10).unwrap();
        let second = store.create_session("two", "server", "stdio", 20).unwrap();
        write_tool_exchange(&store, &first, 4, 9, 1, json!({"message": "first"}));
        write_tool_exchange(&store, &second, 7, 12, 1, json!({"message": "second"}));
        store.close_session(&first, 15).unwrap();
        store.close_session(&second, 25).unwrap();

        let result = store
            .merge_sessions(&[first.clone(), second.clone()], false)
            .unwrap();
        let messages = store.get_messages(&result.session_id).unwrap();
        let summary = store.get_session_summary(&result.session_id).unwrap();

        assert_eq!(result.source_session_ids, vec![first, second]);
        assert_eq!(result.total_messages, 4);
        assert_eq!(result.deduplicated_calls, 0);
        assert_eq!(
            messages
                .iter()
                .map(|message| message.seq)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(messages[0].ts_ns, 4);
        assert_eq!(messages[2].ts_ns, 7);
        assert_eq!(summary.client, "mcptracer merge");
        assert_eq!(summary.started_at_ns, 10);
        assert_eq!(summary.ended_at_ns, Some(25));
    }

    #[test]
    fn merge_deduplicates_identical_client_request_response_pairs() {
        let mut store = Store::open_in_memory().unwrap();
        let first = store.create_session("one", "server", "stdio", 0).unwrap();
        let second = store.create_session("two", "server", "stdio", 10).unwrap();
        write_tool_exchange(&store, &first, 0, 1, 1, json!({"message": "same"}));
        write_tool_exchange(&store, &second, 0, 1, 1, json!({"message": "same"}));
        store.close_session(&first, 2).unwrap();
        store.close_session(&second, 12).unwrap();

        let result = store.merge_sessions(&[first, second], true).unwrap();
        let messages = store.get_messages(&result.session_id).unwrap();

        assert_eq!(result.total_messages, 2);
        assert_eq!(result.deduplicated_calls, 1);
        assert_eq!(
            messages
                .iter()
                .map(|message| message.message_kind.as_str())
                .collect::<Vec<_>>(),
            vec!["request", "response"]
        );
    }

    #[test]
    fn merge_refuses_mixed_redaction_configurations() {
        let mut store = Store::open_in_memory().unwrap();
        let first = store.create_session("one", "server", "stdio", 0).unwrap();
        let second = store.create_session("two", "server", "stdio", 1).unwrap();
        store.set_redaction_policy(&second, "default", &[]).unwrap();

        let err = store.merge_sessions(&[first, second], false).unwrap_err();

        assert!(err.to_string().contains("redaction configurations"));
    }

    #[test]
    fn merge_keeps_calls_with_redacted_parameters_distinct() {
        let mut store = Store::open_in_memory().unwrap();
        let first = store.create_session("one", "server", "stdio", 0).unwrap();
        let second = store.create_session("two", "server", "stdio", 10).unwrap();
        let custom_keys = vec!["tenantid".to_string()];
        for session_id in [&first, &second] {
            store
                .set_redaction_policy(session_id, "custom", &custom_keys)
                .unwrap();
        }
        write_tool_exchange(
            &store,
            &first,
            0,
            1,
            1,
            json!({"api_key": REDACTED_PLACEHOLDER}),
        );
        write_tool_exchange(
            &store,
            &second,
            0,
            1,
            1,
            json!({"api_key": REDACTED_PLACEHOLDER}),
        );

        let result = store.merge_sessions(&[first, second], true).unwrap();
        let summary = store.get_session_summary(&result.session_id).unwrap();

        assert_eq!(result.total_messages, 4);
        assert_eq!(result.deduplicated_calls, 0);
        assert_eq!(summary.redaction_policy, "custom");
        assert_eq!(summary.redaction_keys_json, "[\"tenantid\"]");
    }

    #[test]
    fn mtrace_export_import_round_trip_preserves_a_redacted_artifact() {
        let source = Store::open_in_memory().unwrap();
        let source_id = source.create_session("test", "server", "stdio", 0).unwrap();
        source
            .set_redaction_policy(&source_id, "default", &[])
            .unwrap();
        source
            .write_message(
                &source_id,
                &McpMessage {
                    seq: 0,
                    timestamp_ns: 0,
                    direction: Direction::ClientToServer,
                    payload: json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "tools/call",
                        "params": {"name": "echo", "arguments": {"api_key": "secret"}},
                    }),
                    payload_bytes: 96,
                },
            )
            .unwrap();
        source.close_session(&source_id, 1).unwrap();

        let bytes = source
            .export_mtrace(&source_id, mtrace::ExportOptions::default())
            .unwrap();
        let mut exported = mtrace::decode(&bytes, true).unwrap();
        assert_eq!(exported.session.redaction_policy, "default");
        assert_eq!(
            exported.messages[0].payload["params"]["arguments"]["api_key"],
            REDACTED_PLACEHOLDER
        );

        let mut imported_store = Store::open_in_memory().unwrap();
        let imported = imported_store.import_mtrace(&bytes, true).unwrap();
        let imported_message = imported_store.get_messages(&imported.session_id).unwrap();
        assert_eq!(imported.total_messages, 1);
        assert!(!imported_message[0].payload.contains("secret"));

        let reexported_bytes = imported_store
            .export_mtrace(&imported.session_id, mtrace::ExportOptions::default())
            .unwrap();
        let mut reexported = mtrace::decode(&reexported_bytes, true).unwrap();
        exported.exported_at_ns = 0;
        reexported.exported_at_ns = 0;
        assert_eq!(reexported, exported);
    }

    #[test]
    fn mtrace_export_requires_an_explicit_policy_for_unredacted_sessions() {
        let store = Store::open_in_memory().unwrap();
        let session_id = store.create_session("test", "server", "stdio", 0).unwrap();
        store
            .write_message(&session_id, &make_message(0, "tools/list", None))
            .unwrap();

        let err = store
            .export_mtrace(&session_id, mtrace::ExportOptions::default())
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("refusing to export an unredacted session"));

        let document = store
            .export_mtrace_document(
                &session_id,
                mtrace::ExportOptions {
                    force_default_redaction: true,
                    allow_unredacted: false,
                },
            )
            .unwrap();
        assert_eq!(document.session.redaction_policy, "default");
    }

    #[test]
    fn search_messages_filters_across_sessions() {
        let store = Store::open_in_memory().unwrap();
        let a = store.create_session("a", "srv", "stdio", 0).unwrap();
        let b = store.create_session("b", "srv", "stdio", 0).unwrap();

        store
            .write_message(&a, &make_message(1, "tools/call", Some("echo")))
            .unwrap();
        store
            .write_message(&b, &make_message(2, "tools/call", Some("read")))
            .unwrap();

        // An errored echo call in session b: keep method/params (so tool_name
        // still extracts) and add an error object (so is_error flips).
        let mut err = make_message(3, "tools/call", Some("echo"));
        err.payload
            .as_object_mut()
            .unwrap()
            .insert("error".to_string(), json!({"code": -32000}));
        store.write_message(&b, &err).unwrap();

        let by_tool = store
            .search_messages(&SearchQuery {
                tool: Some("echo".to_string()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_tool.len(), 2);
        assert!(by_tool
            .iter()
            .all(|h| h.tool_name.as_deref() == Some("echo")));

        let errors = store
            .search_messages(&SearchQuery {
                errors_only: true,
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].session_id, b);

        let scoped = store
            .search_messages(&SearchQuery {
                tool: Some("echo".to_string()),
                errors_only: true,
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(scoped.len(), 1);
    }

    #[test]
    fn defaults_to_none_redaction_policy_and_reflects_updates() {
        let store = Store::open_in_memory().unwrap();
        let session_id = store.create_session("test", "cmd", "stdio", 0).unwrap();

        assert_eq!(store.get_redaction_policy(&session_id).unwrap(), "none");

        store
            .set_redaction_policy(&session_id, "default", &[])
            .unwrap();
        assert_eq!(store.get_redaction_policy(&session_id).unwrap(), "default");
        assert_eq!(
            store
                .get_session_summary(&session_id)
                .unwrap()
                .redaction_keys_json,
            "[]"
        );

        store
            .set_redaction_policy(
                &session_id,
                "custom",
                &["tenantid".to_string(), "customercode".to_string()],
            )
            .unwrap();
        let summary = store.get_session_summary(&session_id).unwrap();
        assert_eq!(summary.redaction_policy, "custom");
        assert_eq!(
            summary.redaction_keys_json,
            "[\"tenantid\",\"customercode\"]"
        );

        // Resolves by prefix too, same as get_messages.
        assert_eq!(
            store.get_redaction_policy(&session_id[..8]).unwrap(),
            "custom"
        );
    }

    #[test]
    fn closes_session_and_counts_messages() {
        let store = Store::open_in_memory().unwrap();
        let session_id = store.create_session("test", "cmd", "stdio", 100).unwrap();

        store
            .write_message(&session_id, &make_message(1, "tools/list", None))
            .unwrap();
        store.close_session(&session_id, 200).unwrap();

        let sessions = store.list_sessions(1).unwrap();
        assert_eq!(sessions[0].ended_at_ns, Some(200));
        assert_eq!(sessions[0].total_messages, 1);
    }

    #[cfg(unix)]
    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("mcptracer-test-{label}-{pid}-{n}"))
    }

    #[cfg(unix)]
    #[test]
    fn store_open_hardens_directory_and_database_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_temp_dir("perms-new");
        let db_path = dir.join("sessions.db");

        let store = Store::open(&db_path).unwrap();
        // Force a real write so WAL mode actually materializes -wal/-shm.
        store.create_session("test", "server", "stdio", 0).unwrap();
        drop(store);

        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "storage directory must be owner-only");

        let db_mode = fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(db_mode, 0o600, "database file must be owner-only");

        for suffix in ["-wal", "-shm"] {
            let mut sidecar = db_path.as_os_str().to_owned();
            sidecar.push(suffix);
            let sidecar_path = std::path::PathBuf::from(sidecar);
            if sidecar_path.exists() {
                let mode = fs::metadata(&sidecar_path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{suffix} sidecar must be owner-only");
            }
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn store_open_hardens_preexisting_app_owned_directory_and_database_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_temp_dir("perms-fix");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let db_path = dir.join("sessions.db");

        // A pre-existing database file with wide permissions, as if left
        // behind by an older MCPTracer version or a misconfigured deploy.
        fs::write(&db_path, b"").unwrap();
        fs::set_permissions(&db_path, fs::Permissions::from_mode(0o644)).unwrap();

        let _store =
            Store::open_with_parent_ownership(&db_path, ParentDirectoryOwnership::McpTracer)
                .unwrap();

        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "a pre-existing MCPTracer-owned directory must be corrected on open"
        );

        let db_mode = fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            db_mode, 0o600,
            "a pre-existing wide-open database file must be corrected on open"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // ── named local baselines (T-74) ──────────────────────────────────────

    fn store_with_session() -> (Store, String) {
        let store = Store::open_in_memory().unwrap();
        let session_id = store.create_session("test", "cmd", "stdio", 0).unwrap();
        (store, session_id)
    }

    #[test]
    fn fresh_database_has_a_baselines_table() {
        let store = Store::open_in_memory().unwrap();
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM baselines", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn candidate_then_promote_records_digest_actor_and_reason() {
        let (mut store, session_id) = store_with_session();
        store
            .create_baseline_candidate("proj", "scenario", "env", &session_id, 100)
            .unwrap();

        store
            .promote_baseline(
                "proj",
                "scenario",
                "env",
                &session_id,
                "digest-abc",
                "alice",
                "looks good",
                200,
            )
            .unwrap();

        let baseline = store
            .resolve_approved_baseline("proj", "scenario", "env")
            .unwrap();
        assert_eq!(baseline.state, "approved");
        assert_eq!(baseline.session_id, session_id);
        assert_eq!(baseline.digest.as_deref(), Some("digest-abc"));
        assert_eq!(baseline.promoted_by.as_deref(), Some("alice"));
        assert_eq!(baseline.promotion_reason.as_deref(), Some("looks good"));
        assert_eq!(baseline.promoted_at, Some(200));
    }

    #[test]
    fn promote_without_a_registered_candidate_fails_clearly() {
        let (mut store, session_id) = store_with_session();
        let err = store
            .promote_baseline(
                "proj",
                "scenario",
                "env",
                &session_id,
                "d",
                "alice",
                "r",
                100,
            )
            .unwrap_err();
        assert!(err.to_string().contains("no candidate baseline"), "{err}");
    }

    #[test]
    fn promoting_a_new_baseline_supersedes_the_previous_approved_one() {
        let mut store = Store::open_in_memory().unwrap();
        let first_session = store.create_session("test", "cmd1", "stdio", 0).unwrap();
        let second_session = store.create_session("test", "cmd2", "stdio", 0).unwrap();

        store
            .create_baseline_candidate("proj", "scenario", "env", &first_session, 100)
            .unwrap();
        store
            .promote_baseline(
                "proj",
                "scenario",
                "env",
                &first_session,
                "digest-1",
                "alice",
                "first",
                200,
            )
            .unwrap();

        store
            .create_baseline_candidate("proj", "scenario", "env", &second_session, 300)
            .unwrap();
        store
            .promote_baseline(
                "proj",
                "scenario",
                "env",
                &second_session,
                "digest-2",
                "bob",
                "second",
                400,
            )
            .unwrap();

        let approved = store
            .resolve_approved_baseline("proj", "scenario", "env")
            .unwrap();
        assert_eq!(approved.session_id, second_session);
        assert_eq!(approved.digest.as_deref(), Some("digest-2"));

        let all = store.list_baselines(Some("proj")).unwrap();
        let first = all.iter().find(|b| b.session_id == first_session).unwrap();
        assert_eq!(first.state, "superseded");
        let second = all.iter().find(|b| b.session_id == second_session).unwrap();
        assert_eq!(second.state, "approved");
    }

    #[test]
    fn resolve_approved_baseline_fails_when_none_is_approved() {
        let store = Store::open_in_memory().unwrap();
        let err = store
            .resolve_approved_baseline("proj", "scenario", "env")
            .unwrap_err();
        assert!(err.to_string().contains("no approved baseline"), "{err}");
    }

    #[test]
    fn revoke_clears_the_approved_baseline() {
        let (mut store, session_id) = store_with_session();
        store
            .create_baseline_candidate("proj", "scenario", "env", &session_id, 100)
            .unwrap();
        store
            .promote_baseline(
                "proj",
                "scenario",
                "env",
                &session_id,
                "digest",
                "alice",
                "reason",
                200,
            )
            .unwrap();

        store
            .revoke_baseline("proj", "scenario", "env", "no longer valid", 300)
            .unwrap();

        let err = store
            .resolve_approved_baseline("proj", "scenario", "env")
            .unwrap_err();
        assert!(err.to_string().contains("no approved baseline"), "{err}");

        let all = store.list_baselines(None).unwrap();
        let row = all.iter().find(|b| b.session_id == session_id).unwrap();
        assert_eq!(row.state, "revoked");
        assert_eq!(row.revoked_reason.as_deref(), Some("no longer valid"));
        assert_eq!(row.revoked_at, Some(300));
    }

    #[test]
    fn revoke_without_an_approved_baseline_fails_clearly() {
        let store = Store::open_in_memory().unwrap();
        let err = store
            .revoke_baseline("proj", "scenario", "env", "reason", 100)
            .unwrap_err();
        assert!(err.to_string().contains("no approved baseline"), "{err}");
    }

    #[test]
    fn list_baselines_filters_by_project() {
        let (store, session_id) = store_with_session();
        store
            .create_baseline_candidate("proj-a", "scenario", "env", &session_id, 100)
            .unwrap();
        store
            .create_baseline_candidate("proj-b", "scenario", "env", &session_id, 100)
            .unwrap();

        let all = store.list_baselines(None).unwrap();
        assert_eq!(all.len(), 2);

        let only_a = store.list_baselines(Some("proj-a")).unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].project, "proj-a");
    }
    #[test]
    fn only_the_default_database_parent_is_classified_as_mcptracer_owned() {
        assert_eq!(
            parent_directory_ownership(&default_db_path()),
            ParentDirectoryOwnership::McpTracer
        );
        assert_eq!(
            parent_directory_ownership(Path::new("custom.db")),
            ParentDirectoryOwnership::Caller
        );
    }

    #[cfg(unix)]
    #[test]
    fn custom_database_parent_permissions_are_not_modified() {
        use std::os::unix::fs::PermissionsExt;

        let parent = std::env::temp_dir().join(format!(
            "mcptracer-custom-parent-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        let db = parent.join("capture.db");
        Store::open(&db).unwrap();
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(&db).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(parent);
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_database_parent_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "mcptracer-owned-parent-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let parent = root.join("owned");
        let db = parent.join("capture.db");
        Store::open(&db).unwrap();
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&db).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(root);
    }

    /// A corrupt/garbage file at `--db` must fail fast and clearly, and must
    /// never be modified: there could be a real (if unreadable) database
    /// behind it that a "helpful" overwrite would destroy.
    #[test]
    fn open_rejects_a_corrupt_database_file_without_modifying_it() {
        let dir = std::env::temp_dir().join(format!(
            "mcptracer-corrupt-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("corrupt.db");
        fs::write(&db, b"not a sqlite database, just garbage bytes here").unwrap();
        let before = fs::read(&db).unwrap();

        let start = std::time::Instant::now();
        let result = Store::open(&db);
        let elapsed = start.elapsed();

        let error = result.err().expect("opening a corrupt file must fail");
        assert!(
            error.to_string().to_lowercase().contains("not a database"),
            "unexpected error: {error}"
        );
        assert_eq!(
            fs::read(&db).unwrap(),
            before,
            "a corrupt database file must not be modified"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "must fail fast: {elapsed:?}"
        );

        let _ = fs::remove_dir_all(dir);
    }

    /// `--db` pointing at a directory must fail clearly instead of hanging
    /// or panicking.
    #[test]
    fn open_rejects_a_directory_path() {
        let dir = std::env::temp_dir().join(format!(
            "mcptracer-dirpath-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("iam_a_dir");
        fs::create_dir_all(&db).unwrap();

        let start = std::time::Instant::now();
        let result = Store::open(&db);
        let elapsed = start.elapsed();

        let error = result.err().expect("opening a directory must fail");
        let message = error.to_string().to_lowercase();
        assert!(
            message.contains("unable to open") || message.contains("failed to open"),
            "unexpected error: {error}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "must fail fast: {elapsed:?}"
        );

        let _ = fs::remove_dir_all(dir);
    }

    /// Regression test: `init_schema`'s retry loop used to be bounded only by
    /// a retry *count* (100), and every retry re-armed SQLite's 5-second
    /// `busy_timeout` from scratch — a database locked by another process
    /// the whole time (not just the brief startup race the loop exists for)
    /// could hang `Store::open` for minutes. It must now give up within
    /// roughly one `SCHEMA_LOCK_RETRY_BUDGET` window and say clearly why.
    #[test]
    fn open_gives_up_promptly_on_a_database_locked_by_another_connection() {
        let dir = std::env::temp_dir().join(format!(
            "mcptracer-lock-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("locked.db");
        Store::open(&db).unwrap();

        let locker = Connection::open(&db).unwrap();
        locker.execute_batch("BEGIN EXCLUSIVE").unwrap();

        let start = std::time::Instant::now();
        let result = Store::open(&db);
        let elapsed = start.elapsed();

        let error = result.err().expect("opening a locked database must fail");
        assert!(
            error.to_string().to_lowercase().contains("locked"),
            "unexpected error: {error}"
        );
        assert!(
            // Each attempt's busy wait is capped at the remaining budget, so
            // the overrun is one 50 ms retry sleep plus scheduling, not a
            // whole busy timeout (which reached 9.5s on a macOS runner).
            elapsed < SCHEMA_LOCK_RETRY_BUDGET + Duration::from_secs(2),
            "took {elapsed:?} to give up on a permanently locked database; \
             the retry loop must be bounded by wall-clock time"
        );

        drop(locker);
        let _ = fs::remove_dir_all(dir);
    }
}
