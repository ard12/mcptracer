//! Best-effort `.vcr` cassette import (Agent VCR's plain-JSON cassette
//! format).
//!
//! Agent VCR does not publish a formal schema, so this is mcptracer's own
//! reasonable interpretation of a "JSON-RPC request/response interactions"
//! cassette shape:
//!
//! ```jsonc
//! {
//!   "version": 1,
//!   "interactions": [
//!     {"request": {"jsonrpc":"2.0","id":1,"method":"tools/call","params":{...}},
//!      "response": {"jsonrpc":"2.0","id":1,"result":{...}}}
//!   ]
//! }
//! ```
//!
//! Only `request`/`response` JSON-RPC payloads are mapped; any other field
//! on an interaction (timestamps, custom metadata, tags) is ignored rather
//! than merged into the stored message. Unrecognized cassette versions fail
//! cleanly rather than guessing at a different shape.
//!
//! Imported sessions are marked redaction policy `none`: a `.vcr` cassette
//! carries no redaction metadata, so mcptracer cannot claim the payloads
//! were ever redacted.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context, Result};
use mcptracer_protocol::{Direction, McpMessage};
use serde::Deserialize;
use serde_json::Value;

/// The only cassette version this importer understands.
pub const SUPPORTED_VERSION: u32 = 1;

/// Matches `mtrace::MAX_UNCOMPRESSED_BYTES`: `.vcr` has no compression layer,
/// so this is the only size bound on an untrusted cassette file.
pub const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;

/// Matches the spirit of `mtrace::MAX_MESSAGES` (each interaction can become
/// up to two stored messages, request + response).
pub const MAX_INTERACTIONS: usize = 500_000;

#[derive(Debug, Clone, Deserialize)]
struct AgentVcrCassette {
    format_version: String,
    session: AgentVcrSession,
}

#[derive(Debug, Clone, Deserialize)]
struct AgentVcrSession {
    #[serde(default)]
    initialize_request: Option<Value>,
    #[serde(default)]
    initialize_response: Option<Value>,
    #[serde(default)]
    interactions: Vec<VcrInteraction>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VcrCassette {
    pub version: u32,
    #[serde(default)]
    pub interactions: Vec<VcrInteraction>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VcrInteraction {
    pub request: Value,
    #[serde(default)]
    pub response: Option<Value>,
}

/// Read and parse a `.vcr` cassette. Refuses an unrecognized version
/// cleanly rather than guessing at a different cassette shape. Bounds both
/// the raw file read and the interaction count so a malformed or oversized
/// cassette (this is, after all, another tool's file, not necessarily
/// trustworthy) cannot exhaust memory - the same discipline `mtrace::decode`
/// already applies to `.mtrace` artifacts.
pub fn read_file(path: &Path) -> Result<VcrCassette> {
    let file = File::open(path)
        .with_context(|| format!("failed to open .vcr cassette {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read .vcr cassette {}", path.display()))?;
    if bytes.len() > MAX_FILE_BYTES {
        bail!(".vcr cassette exceeds the {MAX_FILE_BYTES} byte limit");
    }

    let cassette = match serde_json::from_slice::<VcrCassette>(&bytes) {
        Ok(cassette) => cassette,
        Err(native_error) => {
            let agent: AgentVcrCassette = serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "invalid .vcr cassette JSON {}: {native_error}",
                    path.display()
                )
            })?;
            if agent.format_version != "1.0.0" {
                bail!(
                    "unsupported Agent VCR format_version {}",
                    agent.format_version
                );
            }
            let mut interactions = agent.session.interactions;
            if let (Some(request), Some(response)) = (
                agent.session.initialize_request,
                agent.session.initialize_response,
            ) {
                interactions.insert(
                    0,
                    VcrInteraction {
                        request,
                        response: Some(response),
                    },
                );
            }
            VcrCassette {
                version: SUPPORTED_VERSION,
                interactions,
            }
        }
    };
    validate_cassette(&cassette)?;
    Ok(cassette)
}

fn validate_cassette(cassette: &VcrCassette) -> Result<()> {
    if cassette.version != SUPPORTED_VERSION {
        bail!(
            "unsupported .vcr cassette version {} (mcptracer imports version {SUPPORTED_VERSION})",
            cassette.version
        );
    }
    if cassette.interactions.len() > MAX_INTERACTIONS {
        bail!(".vcr cassette has more than the {MAX_INTERACTIONS} interaction limit");
    }
    Ok(())
}

/// Convert a parsed cassette into an ordered message list ready for
/// `Store::write_messages`. Each interaction becomes a client request (and,
/// when present, a server response) in cassette order; direction, message
/// kind, rpc id, method, tool name, and error status are all derived from
/// the JSON-RPC payload itself, same as any other recorded message.
pub fn cassette_to_messages(cassette: &VcrCassette) -> Result<Vec<McpMessage>> {
    let mut messages = Vec::with_capacity(cassette.interactions.len() * 2);
    let mut seq = 0u64;

    for (index, interaction) in cassette.interactions.iter().enumerate() {
        if interaction
            .request
            .get("method")
            .and_then(Value::as_str)
            .is_none()
        {
            bail!("interaction {index} request has no string `method` field");
        }

        let request_bytes = interaction.request.to_string();
        messages.push(McpMessage {
            seq,
            // No timestamps in the cassette shape; synthesize a stable,
            // strictly increasing order so correlation and stats still work.
            timestamp_ns: (index as i64) * 2_000_000,
            direction: Direction::ClientToServer,
            payload_bytes: request_bytes.len(),
            payload: interaction.request.clone(),
        });
        seq += 1;

        if let Some(response) = &interaction.response {
            let response_bytes = response.to_string();
            messages.push(McpMessage {
                seq,
                timestamp_ns: (index as i64) * 2_000_000 + 1_000_000,
                direction: Direction::ServerToClient,
                payload_bytes: response_bytes.len(),
                payload: response.clone(),
            });
            seq += 1;
        }
    }

    Ok(messages)
}

pub struct VcrImportResult {
    pub session_id: String,
    pub total_messages: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn rejects_unknown_version_cleanly() {
        let text = serde_json::to_string(&serde_json::json!({
            "version": 2,
            "interactions": []
        }))
        .unwrap();
        let path = std::env::temp_dir().join("mcptracer-vcr-version-test.vcr");
        fs::write(&path, text).unwrap();

        let err = read_file(&path).unwrap_err();
        assert!(err
            .to_string()
            .contains("unsupported .vcr cassette version"));

        let _ = fs::remove_file(&path);
    }

    /// Security/robustness regression: an oversized cassette file must be
    /// rejected, not read unbounded into memory - the same threat model
    /// `mtrace::decode`'s size caps already cover for `.mtrace` artifacts.
    #[test]
    fn rejects_a_cassette_file_over_the_size_limit() {
        let path = std::env::temp_dir().join("mcptracer-vcr-oversized-test.vcr");
        // Valid-shaped JSON padded with whitespace past MAX_FILE_BYTES, so
        // the failure is unambiguously the size cap, not a parse error.
        let padding = " ".repeat(MAX_FILE_BYTES + 1);
        let text = format!("{{\"version\":1,\"interactions\":[]{padding}}}");
        fs::write(&path, &text).unwrap();

        let err = read_file(&path).unwrap_err();
        assert!(
            err.to_string().contains("byte limit"),
            "expected a size-limit error, got: {err}"
        );

        let _ = fs::remove_file(&path);
    }

    /// Exercises the count check directly against an in-memory cassette
    /// rather than round-tripping MAX_INTERACTIONS+1 elements through a real
    /// file: that many even-minimal interactions serialize past
    /// MAX_FILE_BYTES, which would trip the (already-covered) size check
    /// first and say nothing about this specific limit.
    #[test]
    fn rejects_a_cassette_with_too_many_interactions() {
        let interaction = VcrInteraction {
            request: serde_json::json!({"method": "tools/call"}),
            response: None,
        };
        let cassette = VcrCassette {
            version: SUPPORTED_VERSION,
            interactions: vec![interaction; MAX_INTERACTIONS + 1],
        };

        let err = validate_cassette(&cassette).unwrap_err();
        assert!(
            err.to_string().contains("interaction limit"),
            "expected an interaction-limit error, got: {err}"
        );
    }

    #[test]
    fn maps_request_response_pair_to_client_and_server_messages() {
        let cassette: VcrCassette = serde_json::from_value(serde_json::json!({
            "version": 1,
            "interactions": [{
                "request": {"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{}}},
                "response": {"jsonrpc":"2.0","id":1,"result":{"ok":true}}
            }]
        }))
        .unwrap();

        let messages = cassette_to_messages(&cassette).unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].direction, Direction::ClientToServer);
        assert_eq!(messages[0].method(), Some("tools/call"));
        assert_eq!(messages[0].tool_name(), Some("echo"));
        assert_eq!(messages[1].direction, Direction::ServerToClient);
        assert!(!messages[1].is_error());
    }

    #[test]
    fn request_without_response_is_a_lone_message() {
        let cassette: VcrCassette = serde_json::from_value(serde_json::json!({
            "version": 1,
            "interactions": [{
                "request": {"jsonrpc":"2.0","method":"notifications/initialized"}
            }]
        }))
        .unwrap();

        let messages = cassette_to_messages(&cassette).unwrap();

        assert_eq!(messages.len(), 1);
        assert!(messages[0].is_notification());
    }

    #[test]
    fn error_response_is_marked_as_error() {
        let cassette: VcrCassette = serde_json::from_value(serde_json::json!({
            "version": 1,
            "interactions": [{
                "request": {"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo"}},
                "response": {"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"not found"}}
            }]
        }))
        .unwrap();

        let messages = cassette_to_messages(&cassette).unwrap();

        assert!(messages[1].is_error());
        assert_eq!(messages[1].error_code(), Some(-32601));
    }

    #[test]
    fn request_without_method_field_fails_cleanly() {
        let cassette: VcrCassette = serde_json::from_value(serde_json::json!({
            "version": 1,
            "interactions": [{"request": {"jsonrpc":"2.0","id":1}}]
        }))
        .unwrap();

        assert!(cassette_to_messages(&cassette).is_err());
    }
}
