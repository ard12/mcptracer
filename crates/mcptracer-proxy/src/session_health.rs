use anyhow::{bail, Result};
use mcptracer_model::{
    validate_session, SessionIntegrityIssue, SessionIntegrityIssueKind, SessionIntegrityReport,
};
use mcptracer_redact::{unredacted_sensitive_keys, RedactionPolicy};
use mcptracer_storage::{Store, StoredMessage};

pub struct SessionHealth {
    pub session_id: String,
    pub messages: Vec<StoredMessage>,
    pub report: SessionIntegrityReport,
}

/// Load a session and assess whether its stored capture is complete enough for
/// a regression gate. This stays on the read/query side of the proxy and is
/// never used while forwarding MCP frames.
pub fn inspect_session(store: &Store, session_id_or_prefix: &str) -> Result<SessionHealth> {
    let summary = store.get_session_summary(session_id_or_prefix)?;
    let messages = store.get_messages(&summary.id)?;
    let dropped_messages = summary.dropped_messages.max(0) as u64;
    let mut report = validate_session(&messages, dropped_messages);
    append_redaction_issues(
        &mut report,
        &messages,
        &summary.redaction_policy,
        &summary.redaction_keys_json,
    );
    if summary.ended_at_ns.is_none() {
        report.issues.push(SessionIntegrityIssue {
            kind: SessionIntegrityIssueKind::SessionNotClosed,
            seq: None,
            detail: "session was never finalized by the recorder".to_string(),
        });
    }

    Ok(SessionHealth {
        session_id: summary.id,
        messages,
        report,
    })
}

fn append_redaction_issues(
    report: &mut SessionIntegrityReport,
    messages: &[StoredMessage],
    policy_name: &str,
    custom_keys_json: &str,
) {
    let custom_keys: Vec<String> = match serde_json::from_str(custom_keys_json) {
        Ok(keys) => keys,
        Err(_) => {
            report.issues.push(SessionIntegrityIssue {
                kind: SessionIntegrityIssueKind::InvalidRedactionConfiguration,
                seq: None,
                detail: "stored custom redaction keys are not a JSON string array".to_string(),
            });
            return;
        }
    };
    let policy = match RedactionPolicy::from_storage(policy_name, &custom_keys) {
        Ok(policy) => policy,
        Err(detail) => {
            report.issues.push(SessionIntegrityIssue {
                kind: SessionIntegrityIssueKind::InvalidRedactionConfiguration,
                seq: None,
                detail,
            });
            return;
        }
    };

    for message in messages {
        let Ok(payload) = serde_json::from_str(&message.payload) else {
            continue;
        };
        let keys = unredacted_sensitive_keys(&payload, &policy);
        if !keys.is_empty() {
            report.issues.push(SessionIntegrityIssue {
                kind: SessionIntegrityIssueKind::UnredactedSensitiveValue,
                seq: Some(message.seq),
                detail: format!(
                    "stored {} policy left sensitive field(s) unmasked: {}",
                    policy.as_str(),
                    keys.join(", ")
                ),
            });
        }
    }
}

/// Load a session that is eligible to produce a passing diff or assertion.
/// Integrity failures are not overrideable: `--exit-zero` controls meaningful
/// behavioral diffs, not whether the capture itself can be trusted.
pub fn require_healthy_session(
    store: &Store,
    session_id_or_prefix: &str,
    operation: &str,
) -> Result<SessionHealth> {
    let health = inspect_session(store, session_id_or_prefix)?;
    if !health.report.is_healthy() {
        bail!(
            "session {} is unhealthy and cannot be used for {operation}: {} integrity issue(s); run `mcptracer validate {}` for details",
            health.session_id,
            health.report.issues.len(),
            health.session_id,
        );
    }
    Ok(health)
}

#[cfg(test)]
mod tests {
    use mcptracer_model::SessionIntegrityIssueKind;
    use mcptracer_protocol::{Direction, McpMessage};
    use mcptracer_storage::Store;
    use serde_json::json;

    use super::inspect_session;

    #[test]
    fn rejects_an_unclosed_session() {
        let store = Store::open_in_memory().unwrap();
        let session_id = store.create_session("test", "server", "stdio", 0).unwrap();

        let health = inspect_session(&store, &session_id).unwrap();

        assert!(health
            .report
            .issues
            .iter()
            .any(|issue| { issue.kind == SessionIntegrityIssueKind::SessionNotClosed }));
    }

    #[test]
    fn rejects_unmasked_values_required_by_the_recorded_policy() {
        let store = Store::open_in_memory().unwrap();
        let session_id = store.create_session("test", "server", "stdio", 0).unwrap();
        store
            .set_redaction_policy(&session_id, "default", &[])
            .unwrap();
        store
            .write_message(
                &session_id,
                &McpMessage {
                    seq: 0,
                    timestamp_ns: 0,
                    direction: Direction::ClientToServer,
                    payload: json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/test",
                        "params": {"api_key": "leaked"}
                    }),
                    payload_bytes: 80,
                },
            )
            .unwrap();
        store.close_session(&session_id, 1).unwrap();

        let health = inspect_session(&store, &session_id).unwrap();

        assert!(health.report.issues.iter().any(|issue| {
            issue.kind == SessionIntegrityIssueKind::UnredactedSensitiveValue
                && issue.seq == Some(0)
                && issue.detail.contains("api_key")
        }));
    }

    #[test]
    fn rejects_unmasked_values_required_by_persisted_custom_keys() {
        let store = Store::open_in_memory().unwrap();
        let session_id = store.create_session("test", "server", "stdio", 0).unwrap();
        store
            .set_redaction_policy(&session_id, "custom", &["tenant-id".to_string()])
            .unwrap();
        store
            .write_message(
                &session_id,
                &McpMessage {
                    seq: 0,
                    timestamp_ns: 0,
                    direction: Direction::ClientToServer,
                    payload: json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/test",
                        "params": {"tenantId": "leaked"}
                    }),
                    payload_bytes: 80,
                },
            )
            .unwrap();
        store.close_session(&session_id, 1).unwrap();

        let health = inspect_session(&store, &session_id).unwrap();

        assert!(health.report.issues.iter().any(|issue| {
            issue.kind == SessionIntegrityIssueKind::UnredactedSensitiveValue
                && issue.seq == Some(0)
                && issue.detail.contains("tenantId")
        }));
    }

    #[test]
    fn rejects_invalid_custom_redaction_configuration() {
        let store = Store::open_in_memory().unwrap();
        let session_id = store.create_session("test", "server", "stdio", 0).unwrap();
        store
            .set_redaction_policy(&session_id, "custom", &[])
            .unwrap();
        store.close_session(&session_id, 1).unwrap();

        let health = inspect_session(&store, &session_id).unwrap();

        assert!(health.report.issues.iter().any(|issue| {
            issue.kind == SessionIntegrityIssueKind::InvalidRedactionConfiguration
        }));
    }
}
