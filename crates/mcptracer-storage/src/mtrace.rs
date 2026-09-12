//! Versioned, local `.mtrace` session artifacts.
//!
//! The format is gzip-compressed JSON. This module only serializes and
//! validates artifacts; callers decide whether to write a file or persist an
//! imported document.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use mcptracer_protocol::{Direction, McpMessage};
use mcptracer_redact::{
    sensitive_content_lint, unredacted_sensitive_keys, RedactionPolicy, SensitiveContentFinding,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const FORMAT: &str = "mtrace";
pub const VERSION: u32 = 1;
pub const MAX_COMPRESSED_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_UNCOMPRESSED_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_MESSAGES: usize = 1_000_000;

const TOP_LEVEL_FIELDS: &[&str] = &[
    "format",
    "version",
    "exported_at_ns",
    "exporter",
    "session",
    "messages",
];

/// Export controls that retain the hard rule against accidental unredacted
/// sharing. The only policy upgrade supported by v1 is `none` to `default`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExportOptions {
    pub force_default_redaction: bool,
    pub allow_unredacted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MtraceDocument {
    pub format: String,
    pub version: u32,
    pub exported_at_ns: i64,
    pub exporter: String,
    pub session: MtraceSession,
    pub messages: Vec<MtraceMessage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MtraceSession {
    pub client: String,
    pub server_command: String,
    pub transport: String,
    pub started_at_ns: i64,
    pub ended_at_ns: Option<i64>,
    pub redaction_policy: String,
    /// Normalized custom field names, retained so custom redaction can be
    /// checked after import.
    #[serde(default)]
    pub redaction_keys: Vec<String>,
    /// Recording loss must survive export/import so a partial artifact cannot
    /// become a passing integrity gate after import.
    #[serde(default)]
    pub dropped_messages: i64,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MtraceMessage {
    pub seq: u64,
    pub ts_ns: i64,
    pub direction: String,
    pub message_kind: String,
    pub rpc_id: Option<String>,
    pub method: Option<String>,
    pub tool_name: Option<String>,
    pub payload: Value,
    pub payload_bytes: usize,
    pub is_error: bool,
    pub error_code: Option<i64>,
}

/// Encode a validated document as gzip-compressed JSON.
pub fn encode(document: &MtraceDocument) -> Result<Vec<u8>> {
    validate(document)?;
    let json = serde_json::to_vec(document)?;
    if json.len() > MAX_UNCOMPRESSED_BYTES {
        bail!(
            "mtrace JSON exceeds the {} byte limit",
            MAX_UNCOMPRESSED_BYTES
        );
    }

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&json)?;
    let bytes = encoder.finish()?;
    if bytes.len() > MAX_COMPRESSED_BYTES {
        bail!(
            "compressed mtrace exceeds the {} byte limit",
            MAX_COMPRESSED_BYTES
        );
    }
    Ok(bytes)
}

/// Decode and validate gzip-compressed JSON. Strict mode rejects unknown
/// top-level fields; default mode permits forward-compatible top-level fields.
pub fn decode(bytes: &[u8], strict: bool) -> Result<MtraceDocument> {
    if bytes.len() > MAX_COMPRESSED_BYTES {
        bail!(
            "compressed mtrace exceeds the {} byte limit",
            MAX_COMPRESSED_BYTES
        );
    }

    let decoder = GzDecoder::new(bytes);
    let mut json = Vec::new();
    decoder
        .take((MAX_UNCOMPRESSED_BYTES + 1) as u64)
        .read_to_end(&mut json)
        .context("failed to decompress mtrace")?;
    if json.len() > MAX_UNCOMPRESSED_BYTES {
        bail!(
            "mtrace JSON exceeds the {} byte limit",
            MAX_UNCOMPRESSED_BYTES
        );
    }

    let value: Value = serde_json::from_slice(&json).context("mtrace is not valid JSON")?;
    if strict {
        let object = value
            .as_object()
            .context("mtrace top level must be a JSON object")?;
        if let Some(field) = object
            .keys()
            .find(|field| !TOP_LEVEL_FIELDS.contains(&field.as_str()))
        {
            bail!("unknown top-level mtrace field in strict mode: {field}");
        }
    }

    let document: MtraceDocument =
        serde_json::from_value(value).context("mtrace does not match version 1 schema")?;
    validate(&document)?;
    Ok(document)
}

/// Read a bounded artifact file before decoding it. Import callers should use
/// this rather than unbounded `read_to_end` on untrusted artifacts.
pub fn read_file(path: impl AsRef<Path>, strict: bool) -> Result<MtraceDocument> {
    let path = path.as_ref();
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take((MAX_COMPRESSED_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {}", path.display()))?;
    decode(&bytes, strict)
}

/// Write an artifact without replacing an existing file. Export callers should
/// make overwriting an explicit future CLI option rather than silently replacing
/// a portable fixture.
///
/// On Unix, the new file is set to mode 0600 (owner-only read/write) immediately
/// after creation — before any bytes are written, so a concurrent reader racing
/// to open the file still cannot read a partially-written secret.
pub fn write_file(path: impl AsRef<Path>, bytes: &[u8]) -> Result<()> {
    let path = path.as_ref();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    // T-61: restrict the artifact to owner-only access before writing any
    // potentially sensitive bytes. On non-Unix platforms this is a no-op;
    // the file inherits the parent directory's ACL (which we never broaden).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    file.write_all(bytes)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Validate the stable v1 schema and redaction claim without exposing payload
/// values in errors.
pub fn validate(document: &MtraceDocument) -> Result<()> {
    if document.format != FORMAT {
        bail!("unsupported mtrace format: {}", document.format);
    }
    if document.version != VERSION {
        bail!(
            "unsupported mtrace version: {} (supported: {})",
            document.version,
            VERSION
        );
    }
    if document.messages.len() > MAX_MESSAGES {
        bail!("mtrace has more than the {MAX_MESSAGES} message limit");
    }

    let policy = RedactionPolicy::from_storage(
        &document.session.redaction_policy,
        &document.session.redaction_keys,
    )
    .map_err(|detail| anyhow::anyhow!("invalid mtrace redaction policy: {detail}"))?;
    if policy.custom_keys() != document.session.redaction_keys.as_slice() {
        bail!("mtrace custom redaction keys must be normalized");
    }
    if document.session.dropped_messages < 0 {
        bail!("mtrace dropped_messages must not be negative");
    }

    let mut previous_seq = None;
    for message in &document.messages {
        if let Some(previous) = previous_seq {
            if message.seq <= previous {
                bail!(
                    "mtrace message sequence is not strictly increasing at {}",
                    message.seq
                );
            }
        }
        previous_seq = Some(message.seq);

        if !matches!(message.direction.as_str(), "c2s" | "s2c") {
            bail!("mtrace message {} has an unknown direction", message.seq);
        }
        if !matches!(
            message.message_kind.as_str(),
            "request" | "response" | "notification"
        ) {
            bail!("mtrace message {} has an unknown message kind", message.seq);
        }
        if !message.payload.is_object() {
            bail!(
                "mtrace message {} payload must be a JSON object",
                message.seq
            );
        }

        reject_message_field_payload_mismatch(message)?;

        let unredacted = unredacted_sensitive_keys(&message.payload, &policy);
        if !unredacted.is_empty() {
            bail!(
                "mtrace message {} has unredacted {} field(s): {}",
                message.seq,
                policy.as_str(),
                unredacted.join(", ")
            );
        }
    }
    Ok(())
}

/// Stable SHA-256 digest identifying a document's evidentiary content: the
/// format/version, the session's own recorded metadata, and every message
/// in order (T-73). Deliberately excludes `exported_at_ns` and `exporter` —
/// those describe the export transaction, not the underlying evidence, so
/// re-exporting an unchanged session at a later time or with a newer
/// MCPTracer build must not change its digest.
///
/// Object key order never affects this digest: the workspace's `serde_json`
/// is built without the `preserve_order` feature, so its `Map` is backed by
/// a `BTreeMap` and always serializes keys in sorted order regardless of
/// parse-time order (the same property `mcptracer-intel`'s `canonical_json`
/// and `mcptracer-model::diff::tool_hash` already rely on). Array order is
/// preserved exactly, so reordering or truncating `messages` — or any array
/// inside a payload — does change the digest, matching the "ordering and
/// truncation mutations are detected" requirement.
///
/// A digest match only proves the artifact's content matches what a
/// manifest recorded; it is not on its own a tamper-evidence guarantee,
/// since anyone who can edit the manifest can also recompute a matching
/// digest for altered content. That guarantee requires a signature over the
/// digest, which v1 does not provide — see `docs/spec/artifact-verification.md`.
/// Scan every message payload plus the session's `server_command` for
/// sensitive content that key-name redaction cannot catch (bearer tokens,
/// PEM blocks, URL query secrets, command-embedded secrets), with each
/// finding's JSON pointer prefixed by its message `seq` for context. Shared
/// by `export` (a pre-export lint over the caller's own artifact) and the
/// registry service (a server-side re-check over an untrusted upload) so the
/// two call sites can never quietly disagree about what counts as sensitive.
pub fn lint_sensitive_content(document: &MtraceDocument) -> Vec<SensitiveContentFinding> {
    let mut findings = Vec::new();
    for msg in &document.messages {
        let mut message_findings = sensitive_content_lint(&msg.payload, None);
        for finding in &mut message_findings {
            finding.pointer = format!("/messages/{}{}", msg.seq, finding.pointer);
        }
        findings.extend(message_findings);
    }
    findings.extend(sensitive_content_lint(
        &Value::Null,
        Some(&document.session.server_command),
    ));
    findings
}

pub fn canonical_digest(document: &MtraceDocument) -> String {
    let identity = serde_json::json!({
        "format": document.format,
        "version": document.version,
        "session": document.session,
        "messages": document.messages,
    });
    sha256_hex(identity.to_string().as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `message_kind`, `rpc_id`, `method`, `tool_name`, `is_error`, and
/// `error_code` are all derivable purely from `payload` — the exact
/// derivation `record`/`replay` use when they first populate these columns
/// (see `Store::write_messages`). An `.mtrace` artifact carries them as
/// separate fields anyway (so a valid artifact never needs a JSON parse to
/// answer "was this an error?"), which means nothing stops a crafted
/// artifact from claiming a `tool_name` or `is_error` that its own `payload`
/// contradicts — e.g. a payload that calls `delete_all` labeled with
/// `tool_name: "read_file"`, or an error response claiming `is_error:
/// false`. `direction` is not checked here: unlike the other fields it is
/// not derivable from payload alone (MCP is bidirectional), so the artifact
/// remains the only source for it.
fn reject_message_field_payload_mismatch(message: &MtraceMessage) -> Result<()> {
    let direction = Direction::from_db_str(&message.direction)
        .context("mtrace message direction failed re-validation")?;
    let derived = McpMessage {
        seq: message.seq,
        timestamp_ns: message.ts_ns,
        direction,
        payload: message.payload.clone(),
        payload_bytes: message.payload_bytes,
    };

    if derived.kind().as_db_str() != message.message_kind {
        bail!(
            "mtrace message {} claims message_kind {:?} but its payload implies {:?}",
            message.seq,
            message.message_kind,
            derived.kind().as_db_str()
        );
    }
    if derived.id_json() != message.rpc_id {
        bail!(
            "mtrace message {} claims rpc_id {:?} but its payload implies {:?}",
            message.seq,
            message.rpc_id,
            derived.id_json()
        );
    }
    if derived.method() != message.method.as_deref() {
        bail!(
            "mtrace message {} claims method {:?} but its payload implies {:?}",
            message.seq,
            message.method,
            derived.method()
        );
    }
    if derived.tool_name() != message.tool_name.as_deref() {
        bail!(
            "mtrace message {} claims tool_name {:?} but its payload implies {:?}",
            message.seq,
            message.tool_name,
            derived.tool_name()
        );
    }
    if derived.is_error() != message.is_error {
        bail!(
            "mtrace message {} claims is_error {} but its payload implies {}",
            message.seq,
            message.is_error,
            derived.is_error()
        );
    }
    if derived.error_code() != message.error_code {
        bail!(
            "mtrace message {} claims error_code {:?} but its payload implies {:?}",
            message.seq,
            message.error_code,
            derived.error_code()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcptracer_redact::SensitiveCategory;
    use serde_json::json;

    fn encode_unchecked(document: &MtraceDocument) -> Vec<u8> {
        let json = serde_json::to_vec(document).unwrap();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&json).unwrap();
        encoder.finish().unwrap()
    }

    fn document() -> MtraceDocument {
        MtraceDocument {
            format: FORMAT.to_string(),
            version: VERSION,
            exported_at_ns: 1,
            exporter: "mcptracer/test".to_string(),
            session: MtraceSession {
                client: "test".to_string(),
                server_command: "server".to_string(),
                transport: "stdio".to_string(),
                started_at_ns: 0,
                ended_at_ns: Some(1),
                redaction_policy: "default".to_string(),
                redaction_keys: Vec::new(),
                dropped_messages: 0,
                tags: vec!["fixture".to_string()],
            },
            messages: vec![MtraceMessage {
                seq: 0,
                ts_ns: 0,
                direction: "c2s".to_string(),
                message_kind: "request".to_string(),
                rpc_id: Some("1".to_string()),
                method: Some("tools/call".to_string()),
                tool_name: Some("echo".to_string()),
                payload: json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {"name": "echo", "api_key": "***REDACTED***"}
                }),
                payload_bytes: 64,
                is_error: false,
                error_code: None,
            }],
        }
    }

    #[test]
    fn gzip_json_round_trips() {
        let source = document();
        let decoded = decode(&encode(&source).unwrap(), true).unwrap();

        assert_eq!(decoded, source);
    }

    #[test]
    fn version_guard_rejects_unknown_versions() {
        let mut source = document();
        source.version = 999;

        assert!(decode(&encode_unchecked(&source), false)
            .unwrap_err()
            .to_string()
            .contains("unsupported mtrace version"));
    }

    #[test]
    fn rejects_a_tool_name_that_disagrees_with_the_payload() {
        let mut source = document();
        source.messages[0].tool_name = Some("delete_all".to_string());

        let err = decode(&encode_unchecked(&source), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("claims tool_name"), "{err}");
    }

    #[test]
    fn rejects_a_method_that_disagrees_with_the_payload() {
        let mut source = document();
        source.messages[0].method = Some("tools/list".to_string());

        let err = decode(&encode_unchecked(&source), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("claims method"), "{err}");
    }

    #[test]
    fn rejects_an_rpc_id_that_disagrees_with_the_payload() {
        let mut source = document();
        source.messages[0].rpc_id = Some("999".to_string());

        let err = decode(&encode_unchecked(&source), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("claims rpc_id"), "{err}");
    }

    #[test]
    fn rejects_a_message_kind_that_disagrees_with_the_payload() {
        let mut source = document();
        source.messages[0].message_kind = "response".to_string();

        let err = decode(&encode_unchecked(&source), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("claims message_kind"), "{err}");
    }

    #[test]
    fn rejects_an_is_error_claim_that_disagrees_with_the_payload() {
        let mut source = document();
        source.messages[0].is_error = true;

        let err = decode(&encode_unchecked(&source), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("claims is_error"), "{err}");
    }

    #[test]
    fn rejects_an_error_code_that_disagrees_with_the_payload() {
        let mut source = document();
        source.messages[0].error_code = Some(-32000);

        let err = decode(&encode_unchecked(&source), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("claims error_code"), "{err}");
    }

    #[test]
    fn a_consistent_error_response_round_trips() {
        let mut source = document();
        source.messages[0].message_kind = "response".to_string();
        source.messages[0].method = None;
        source.messages[0].tool_name = None;
        source.messages[0].is_error = true;
        source.messages[0].error_code = Some(-32601);
        source.messages[0].payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32601, "message": "not found"}
        });

        let decoded = decode(&encode(&source).unwrap(), true).unwrap();
        assert_eq!(decoded, source);
    }

    #[test]
    fn strict_mode_rejects_unknown_top_level_fields() {
        let mut value = serde_json::to_value(document()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_string(), json!(true));
        let json = serde_json::to_vec(&value).unwrap();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&json).unwrap();
        let bytes = encoder.finish().unwrap();

        assert!(decode(&bytes, true)
            .unwrap_err()
            .to_string()
            .contains("unknown top-level mtrace field"));
    }

    #[cfg(unix)]
    #[test]
    fn write_file_sets_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "mcptracer-mtrace-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.mtrace");

        write_file(&path, &encode(&document()).unwrap()).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "exported artifact must be owner-only");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── canonical_digest tests (T-73) ────────────────────────────────────

    #[test]
    fn digest_is_stable_across_re_export_metadata() {
        let mut reexported = document();
        reexported.exported_at_ns = 999_999_999;
        reexported.exporter = "mcptracer/9.9.9".to_string();

        assert_eq!(
            canonical_digest(&document()),
            canonical_digest(&reexported),
            "exported_at_ns/exporter must not affect the identity digest"
        );
    }

    #[test]
    fn digest_is_stable_across_json_key_order() {
        // serde_json's Map is a BTreeMap (no preserve_order feature), so
        // to_string always emits sorted keys regardless of parse order —
        // this test pins that assumption rather than re-deriving it.
        let mut reordered = document();
        reordered.messages[0].payload = json!({
            "params": {"name": "echo", "api_key": "***REDACTED***"},
            "method": "tools/call",
            "id": 1,
            "jsonrpc": "2.0"
        });
        assert_eq!(
            reordered.messages[0].payload,
            document().messages[0].payload
        );

        assert_eq!(
            canonical_digest(&document()),
            canonical_digest(&reordered),
            "JSON object key order must not affect the identity digest"
        );
    }

    #[test]
    fn digest_changes_when_payload_content_changes() {
        let mut mutated = document();
        mutated.messages[0].payload["params"]["name"] = json!("delete_all");
        mutated.messages[0].tool_name = Some("delete_all".to_string());

        assert_ne!(canonical_digest(&document()), canonical_digest(&mutated));
    }

    #[test]
    fn digest_changes_when_session_metadata_changes() {
        let mut mutated = document();
        mutated.session.redaction_policy = "none".to_string();

        assert_ne!(canonical_digest(&document()), canonical_digest(&mutated));
    }

    #[test]
    fn digest_changes_when_messages_are_reordered() {
        let mut two_messages = document();
        let mut second = two_messages.messages[0].clone();
        second.seq = 1;
        second.rpc_id = Some("2".to_string());
        second.payload["id"] = json!(2);
        two_messages.messages.push(second);

        let mut reordered = two_messages.clone();
        reordered.messages.reverse();

        assert_ne!(
            canonical_digest(&two_messages),
            canonical_digest(&reordered),
            "message order must affect the identity digest"
        );
    }

    #[test]
    fn digest_changes_when_a_message_is_truncated() {
        let mut two_messages = document();
        let mut second = two_messages.messages[0].clone();
        second.seq = 1;
        second.rpc_id = Some("2".to_string());
        second.payload["id"] = json!(2);
        two_messages.messages.push(second);

        let mut truncated = two_messages.clone();
        truncated.messages.pop();

        assert_ne!(
            canonical_digest(&two_messages),
            canonical_digest(&truncated)
        );
    }

    // ── lint_sensitive_content tests ─────────────────────────────────────

    #[test]
    fn lint_sensitive_content_is_empty_for_a_clean_document() {
        assert!(lint_sensitive_content(&document()).is_empty());
    }

    #[test]
    fn lint_sensitive_content_flags_a_bearer_token_with_a_seq_prefixed_pointer() {
        let mut doc = document();
        doc.messages[0].payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "params": {"authorization": "Bearer sk-live-abcdefghijklmnop"}
        });

        let findings = lint_sensitive_content(&doc);

        assert_eq!(findings.len(), 1);
        assert!(findings[0].pointer.starts_with("/messages/0/"));
        assert_eq!(findings[0].category, SensitiveCategory::BearerToken);
    }

    #[test]
    fn lint_sensitive_content_flags_a_command_secret_in_server_command() {
        let mut doc = document();
        doc.session.server_command = "server --api-key=sk-live-abcdefgh".to_string();

        let findings = lint_sensitive_content(&doc);

        assert!(findings
            .iter()
            .any(|f| f.pointer == "/session/server_command"
                && f.category == SensitiveCategory::CommandSecret));
    }

    #[test]
    fn rejects_payload_exceeding_max_uncompressed_bytes() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        // Create a highly compressible 33MB buffer of spaces (exceeding MAX_UNCOMPRESSED_BYTES 32MB)
        let large_uncompressed = vec![b' '; 33 * 1024 * 1024];
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&large_uncompressed).unwrap();
        let compressed = encoder.finish().unwrap();

        // Compressed payload is very small (~33KB), but decompressed size exceeds 32MB
        assert!(compressed.len() < 100_000);
        let error = decode(&compressed, false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exceeds the 33554432 byte limit"),
            "unexpected error: {error}"
        );
    }
}
