//! Shared "what would this call do" planning support for `replay --plan` and
//! `bench --plan`: derives the driven call list, the tool-name allow/deny
//! filter, and per-call risk annotations observed from the session's own
//! `tools/list` capture — all without touching a subprocess or the network,
//! so a caller (human or CI) can decide whether to run for real before
//! anything executes. See `docs/spec/replay-plan.md` for the frozen contract.

use std::collections::BTreeMap;

use mcptracer_redact::REDACTED_PLACEHOLDER;
use mcptracer_storage::StoredMessage;
use serde::Serialize;
use serde_json::Value;

/// MCP tool annotation hints as declared by the server's own `tools/list`
/// response, observed in the source session. `None` means the hint was not
/// present in the tool definition, not that it is known to be false.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ToolAnnotations {
    pub destructive_hint: Option<bool>,
    pub read_only_hint: Option<bool>,
    pub idempotent_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}

impl ToolAnnotations {
    fn from_value(value: &Value) -> Self {
        Self {
            destructive_hint: value.get("destructiveHint").and_then(Value::as_bool),
            read_only_hint: value.get("readOnlyHint").and_then(Value::as_bool),
            idempotent_hint: value.get("idempotentHint").and_then(Value::as_bool),
            open_world_hint: value.get("openWorldHint").and_then(Value::as_bool),
        }
    }
}

/// Coarse risk classification for one driven call, derived only from what the
/// session itself recorded — never assumed from a tool's name or arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// A protocol-lifecycle message with no tool call of its own
    /// (`initialize`, `notifications/*`), or a tool the session's own
    /// `tools/list` marked `readOnlyHint: true`.
    ReadOnly,
    /// The session's own `tools/list` marked this tool `destructiveHint:
    /// true`.
    Destructive,
    /// A tool call whose annotations were never observed in this session (no
    /// `tools/list` capture, or the tool wasn't in it) — risk unknown, not
    /// assumed safe.
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedCall {
    pub seq: u64,
    pub kind: &'static str,
    pub method: Option<String>,
    pub tool_name: Option<String>,
    pub rpc_id: Option<String>,
    pub annotations: Option<ToolAnnotations>,
    pub risk: RiskLevel,
    pub contains_redacted_placeholder: bool,
    /// Whether this call survives `--allow-tool`/`--deny-tool` filtering. A
    /// filtered-out call is still listed here for visibility but is skipped
    /// during real execution.
    pub allowed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub total_calls: usize,
    pub allowed_calls: usize,
    pub filtered_out_calls: usize,
    pub destructive_calls: usize,
    pub unknown_risk_calls: usize,
    pub redacted_placeholder_calls: usize,
    /// True when at least one allowed call is destructive or contains the
    /// redaction placeholder — the two conditions this project treats as
    /// worth a deliberate acknowledgement before automating a run, since
    /// both are strong, session-observed signals rather than mere absence of
    /// information. `unknown_risk_calls` alone does not set this: most real
    /// MCP servers never declare tool annotations at all, so treating
    /// "unknown" as "acknowledgement required" would make the flag
    /// effectively mandatory for almost every session. This field is a
    /// signal for a caller (human or CI) to act on; `replay`/`bench` do not
    /// enforce it themselves — see docs/spec/replay-plan.md.
    pub requires_acknowledgement: bool,
    pub calls: Vec<PlannedCall>,
}

/// The client-originated stream a real run would drive into the target
/// server: `c2s` requests and notifications, in source `seq` order, with
/// `initialize` forced first regardless of source ordering quirks (per the
/// MCP lifecycle). Shared by `replay`, `bench`, and `--plan` so they can
/// never disagree about what "the driven calls" are.
pub fn driven_messages(source: &[StoredMessage]) -> Vec<&StoredMessage> {
    let mut driven: Vec<&StoredMessage> = source
        .iter()
        .filter(|m| {
            m.direction == "c2s"
                && (m.message_kind == "request" || m.message_kind == "notification")
        })
        .collect();

    if let Some(init_pos) = driven
        .iter()
        .position(|m| m.method.as_deref() == Some("initialize"))
    {
        if init_pos != 0 {
            let init = driven.remove(init_pos);
            driven.insert(0, init);
        }
    }
    driven
}

/// True when a tool call should actually be sent during real execution,
/// given `--allow-tool`/`--deny-tool` filters. Non-tool driven messages
/// (protocol lifecycle: `initialize`, `notifications/*`) are never filtered,
/// since they are not tool calls.
pub fn tool_is_allowed(
    tool_name: Option<&str>,
    allow_tools: &[String],
    deny_tools: &[String],
) -> bool {
    let Some(name) = tool_name else { return true };
    !deny_tools.iter().any(|deny| deny == name)
        && (allow_tools.is_empty() || allow_tools.iter().any(|allow| allow == name))
}

/// Tool name -> annotations, parsed from every `tools/list` response the
/// session itself recorded (`s2c` responses carrying a `result.tools` array).
/// A later listing wins on a name collision, since a session can re-list
/// mid-stream.
fn observed_tool_annotations(source: &[StoredMessage]) -> BTreeMap<String, ToolAnnotations> {
    let mut annotations = BTreeMap::new();
    for message in source {
        if message.direction != "s2c" {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<Value>(&message.payload) else {
            continue;
        };
        let Some(tools) = payload.pointer("/result/tools").and_then(Value::as_array) else {
            continue;
        };
        for tool in tools {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            let hints = tool
                .get("annotations")
                .map(ToolAnnotations::from_value)
                .unwrap_or_default();
            annotations.insert(name.to_string(), hints);
        }
    }
    annotations
}

fn risk_for(tool_name: Option<&str>, annotations: Option<&ToolAnnotations>) -> RiskLevel {
    if tool_name.is_none() {
        return RiskLevel::ReadOnly;
    }
    match annotations {
        Some(hints) if hints.destructive_hint == Some(true) => RiskLevel::Destructive,
        Some(hints) if hints.read_only_hint == Some(true) => RiskLevel::ReadOnly,
        _ => RiskLevel::Unknown,
    }
}

/// Build the `--plan` report: every driven call a real run would send,
/// annotated with what the session itself observed about its risk, without
/// spawning a server process or making a network call.
pub fn build_plan(source: &[StoredMessage], allow_tools: &[String], deny_tools: &[String]) -> Plan {
    let known_annotations = observed_tool_annotations(source);

    let calls: Vec<PlannedCall> = driven_messages(source)
        .into_iter()
        .map(|message| {
            let annotations = message
                .tool_name
                .as_deref()
                .and_then(|name| known_annotations.get(name).cloned());
            let risk = risk_for(message.tool_name.as_deref(), annotations.as_ref());
            let allowed = tool_is_allowed(message.tool_name.as_deref(), allow_tools, deny_tools);
            PlannedCall {
                seq: message.seq,
                kind: if message.message_kind == "request" {
                    "request"
                } else {
                    "notification"
                },
                method: message.method.clone(),
                tool_name: message.tool_name.clone(),
                rpc_id: message.rpc_id.clone(),
                annotations,
                risk,
                contains_redacted_placeholder: message.payload.contains(REDACTED_PLACEHOLDER),
                allowed,
            }
        })
        .collect();

    let allowed_calls = calls.iter().filter(|c| c.allowed).count();
    let destructive_calls = calls
        .iter()
        .filter(|c| c.allowed && c.risk == RiskLevel::Destructive)
        .count();
    let unknown_risk_calls = calls
        .iter()
        .filter(|c| c.allowed && c.risk == RiskLevel::Unknown)
        .count();
    let redacted_placeholder_calls = calls
        .iter()
        .filter(|c| c.allowed && c.contains_redacted_placeholder)
        .count();

    Plan {
        total_calls: calls.len(),
        allowed_calls,
        filtered_out_calls: calls.len() - allowed_calls,
        destructive_calls,
        unknown_risk_calls,
        redacted_placeholder_calls,
        requires_acknowledgement: destructive_calls > 0 || redacted_placeholder_calls > 0,
        calls,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stored(
        seq: u64,
        direction: &str,
        kind: &str,
        rpc_id: Option<&str>,
        method: Option<&str>,
        tool_name: Option<&str>,
        payload: &str,
    ) -> StoredMessage {
        StoredMessage {
            seq,
            ts_ns: 0,
            direction: direction.to_string(),
            message_kind: kind.to_string(),
            rpc_id: rpc_id.map(String::from),
            method: method.map(String::from),
            tool_name: tool_name.map(String::from),
            payload: payload.to_string(),
            payload_bytes: payload.len(),
            is_error: false,
            error_code: None,
        }
    }

    fn tools_list_response(seq: u64, tools: Value) -> StoredMessage {
        stored(
            seq,
            "s2c",
            "response",
            Some("99"),
            None,
            None,
            &json!({"jsonrpc":"2.0","id":99,"result":{"tools":tools}}).to_string(),
        )
    }

    #[test]
    fn classifies_a_destructive_annotated_tool_call() {
        let source = vec![
            tools_list_response(
                0,
                json!([{"name":"delete_file","annotations":{"destructiveHint":true}}]),
            ),
            stored(
                1,
                "c2s",
                "request",
                Some("1"),
                Some("tools/call"),
                Some("delete_file"),
                r#"{"jsonrpc":"2.0","id":1,"params":{"name":"delete_file"}}"#,
            ),
        ];

        let plan = build_plan(&source, &[], &[]);

        assert_eq!(plan.destructive_calls, 1);
        assert_eq!(plan.unknown_risk_calls, 0);
        assert!(plan.requires_acknowledgement);
        assert_eq!(plan.calls[0].risk, RiskLevel::Destructive);
    }

    #[test]
    fn classifies_a_payment_style_tool_with_no_observed_annotations_as_unknown_not_safe() {
        let source = vec![stored(
            0,
            "c2s",
            "request",
            Some("1"),
            Some("tools/call"),
            Some("charge_card"),
            r#"{"jsonrpc":"2.0","id":1,"params":{"name":"charge_card"}}"#,
        )];

        let plan = build_plan(&source, &[], &[]);

        assert_eq!(plan.unknown_risk_calls, 1);
        assert_eq!(plan.destructive_calls, 0);
        // Unknown risk alone (no observed annotation, no redacted argument)
        // does not force acknowledgement — see the field doc comment.
        assert!(!plan.requires_acknowledgement);
        assert_eq!(plan.calls[0].risk, RiskLevel::Unknown);
    }

    #[test]
    fn a_read_only_annotated_tool_never_requires_acknowledgement() {
        let source = vec![
            tools_list_response(
                0,
                json!([{"name":"search","annotations":{"readOnlyHint":true}}]),
            ),
            stored(
                1,
                "c2s",
                "request",
                Some("1"),
                Some("tools/call"),
                Some("search"),
                r#"{"jsonrpc":"2.0","id":1,"params":{"name":"search"}}"#,
            ),
        ];

        let plan = build_plan(&source, &[], &[]);

        assert_eq!(plan.calls[0].risk, RiskLevel::ReadOnly);
        assert!(!plan.requires_acknowledgement);
    }

    #[test]
    fn a_redacted_argument_call_requires_acknowledgement_even_with_no_annotations() {
        let source = vec![stored(
            0,
            "c2s",
            "request",
            Some("1"),
            Some("tools/call"),
            Some("send_email"),
            r#"{"jsonrpc":"2.0","id":1,"params":{"api_key":"***REDACTED***"}}"#,
        )];

        let plan = build_plan(&source, &[], &[]);

        assert_eq!(plan.redacted_placeholder_calls, 1);
        assert!(plan.requires_acknowledgement);
        assert!(plan.calls[0].contains_redacted_placeholder);
    }

    #[test]
    fn a_missing_annotation_tool_call_is_still_listed_and_flagged_unknown() {
        let source = vec![stored(
            0,
            "c2s",
            "request",
            Some("1"),
            Some("tools/call"),
            Some("mystery_tool"),
            r#"{"jsonrpc":"2.0","id":1,"params":{"name":"mystery_tool"}}"#,
        )];

        let plan = build_plan(&source, &[], &[]);

        assert_eq!(plan.total_calls, 1);
        assert!(plan.calls[0].annotations.is_none());
        assert_eq!(plan.calls[0].risk, RiskLevel::Unknown);
    }

    #[test]
    fn deny_tool_filters_out_a_call_but_keeps_it_visible_in_the_plan() {
        let source = vec![stored(
            0,
            "c2s",
            "request",
            Some("1"),
            Some("tools/call"),
            Some("delete_file"),
            r#"{"jsonrpc":"2.0","id":1,"params":{"name":"delete_file"}}"#,
        )];

        let plan = build_plan(&source, &[], &["delete_file".to_string()]);

        assert_eq!(plan.total_calls, 1);
        assert_eq!(plan.allowed_calls, 0);
        assert_eq!(plan.filtered_out_calls, 1);
        assert!(!plan.calls[0].allowed);
        // A filtered-out call cannot force acknowledgement: it will never run.
        assert!(!plan.requires_acknowledgement);
    }

    #[test]
    fn allow_tool_excludes_every_other_tool() {
        let source = vec![
            stored(
                0,
                "c2s",
                "request",
                Some("1"),
                Some("tools/call"),
                Some("search"),
                r#"{"jsonrpc":"2.0","id":1,"params":{"name":"search"}}"#,
            ),
            stored(
                1,
                "c2s",
                "request",
                Some("2"),
                Some("tools/call"),
                Some("delete_file"),
                r#"{"jsonrpc":"2.0","id":2,"params":{"name":"delete_file"}}"#,
            ),
        ];

        let plan = build_plan(&source, &["search".to_string()], &[]);

        assert!(tool_is_allowed(
            Some("search"),
            &["search".to_string()],
            &[]
        ));
        assert!(!tool_is_allowed(
            Some("delete_file"),
            &["search".to_string()],
            &[]
        ));
        assert_eq!(plan.allowed_calls, 1);
        assert_eq!(plan.filtered_out_calls, 1);
    }

    #[test]
    fn non_tool_lifecycle_messages_are_never_filtered_and_are_read_only() {
        let source = vec![stored(
            0,
            "c2s",
            "request",
            Some("1"),
            Some("initialize"),
            None,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
        )];

        let plan = build_plan(&source, &[], &["anything".to_string()]);

        assert!(plan.calls[0].allowed);
        assert_eq!(plan.calls[0].risk, RiskLevel::ReadOnly);
    }

    #[test]
    fn driven_messages_forces_initialize_first_regardless_of_source_order() {
        let source = vec![
            stored(
                0,
                "c2s",
                "notification",
                None,
                Some("notifications/cancelled"),
                None,
                "{}",
            ),
            stored(
                1,
                "c2s",
                "request",
                Some("1"),
                Some("initialize"),
                None,
                "{}",
            ),
        ];

        let driven = driven_messages(&source);

        assert_eq!(driven[0].method.as_deref(), Some("initialize"));
        assert_eq!(driven[1].seq, 0);
    }
}
