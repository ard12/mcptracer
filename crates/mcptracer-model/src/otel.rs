//! OTLP/JSON span export for a correlated session.
//!
//! Best-effort mapping onto OTel's GenAI semantic conventions (`gen_ai.*`
//! attributes), file output only (`--out spans.json`); OTLP/HTTP push is
//! future work. Interop with Elastic/Grafana-class backends, not a
//! competing observability product. See `docs/spec/otel-export.md`.

use mcptracer_protocol::Direction;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{Exchange, ExchangeStatus, SessionModel};

/// OTel `SpanKind` numeric values (from the OTLP proto enum).
const SPAN_KIND_CLIENT: u8 = 3;

/// OTel `Status.code` numeric values.
const STATUS_CODE_UNSET: u8 = 0;
const STATUS_CODE_OK: u8 = 1;
const STATUS_CODE_ERROR: u8 = 2;

/// Render a correlated session as a single OTLP/JSON `resourceSpans`
/// document (the standard file-export shape: one JSON object, not NDJSON).
/// Deterministic: the same session always produces byte-identical output,
/// since trace/span ids are derived from the session id rather than random.
pub fn session_to_otlp_json(session_id: &str, client: &str, model: &SessionModel) -> Value {
    let trace_id = deterministic_trace_id(session_id);
    let spans: Vec<Value> = model
        .exchanges
        .iter()
        .enumerate()
        .filter(|(_, exchange)| exchange.status != ExchangeStatus::OrphanResponse)
        .map(|(index, exchange)| exchange_to_span(&trace_id, index, exchange))
        .collect();

    json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [
                    {"key": "service.name", "value": {"stringValue": client}},
                    {"key": "mcp.session.id", "value": {"stringValue": session_id}},
                ]
            },
            "scopeSpans": [{
                "scope": {"name": "mcptracer", "version": env!("CARGO_PKG_VERSION")},
                "spans": spans,
            }]
        }]
    })
}

/// `{method} {tool}` when both are known, else just the method.
pub fn span_name(exchange: &Exchange) -> String {
    match (&exchange.method, &exchange.tool_name) {
        (Some(method), Some(tool)) => format!("{method} {tool}"),
        (Some(method), None) => method.clone(),
        (None, _) => "unknown".to_string(),
    }
}

fn exchange_to_span(trace_id: &str, index: usize, exchange: &Exchange) -> Value {
    let span_id = deterministic_span_id(trace_id, index);
    let start_ns = exchange.request_ts_ns.unwrap_or(0).max(0);
    let end_ns = exchange.response_ts_ns.unwrap_or(start_ns).max(start_ns);

    let mut attributes = vec![json!({
        "key": "gen_ai.operation.name",
        "value": {"stringValue": exchange.method.clone().unwrap_or_default()}
    })];
    if let Some(tool) = &exchange.tool_name {
        attributes.push(json!({
            "key": "gen_ai.tool.name",
            "value": {"stringValue": tool}
        }));
    }
    attributes.push(json!({
        "key": "mcp.direction",
        "value": {"stringValue": match exchange.origin {
            Direction::ClientToServer => "client_to_server",
            Direction::ServerToClient => "server_to_client",
        }}
    }));
    if let Some(code) = exchange.error_code {
        attributes.push(json!({
            "key": "error.type",
            "value": {"stringValue": code.to_string()}
        }));
    }

    let (status_code, status_message) = match exchange.status {
        ExchangeStatus::Ok | ExchangeStatus::Subscribed => (STATUS_CODE_OK, None),
        ExchangeStatus::Error => (STATUS_CODE_ERROR, Some("error response")),
        ExchangeStatus::ToolError => (STATUS_CODE_ERROR, Some("tool execution error")),
        ExchangeStatus::Unanswered => (STATUS_CODE_ERROR, Some("unanswered request")),
        ExchangeStatus::OrphanResponse => (STATUS_CODE_UNSET, None),
    };
    let mut status = json!({"code": status_code});
    if let Some(message) = status_message {
        status["message"] = json!(message);
    }

    json!({
        "traceId": trace_id,
        "spanId": span_id,
        "name": span_name(exchange),
        "kind": SPAN_KIND_CLIENT,
        "startTimeUnixNano": start_ns.to_string(),
        "endTimeUnixNano": end_ns.to_string(),
        "attributes": attributes,
        "status": status,
    })
}

/// 16-byte trace id, hex-encoded (32 chars), derived from the session id.
fn deterministic_trace_id(session_id: &str) -> String {
    let digest = Sha256::digest(session_id.as_bytes());
    hex_string(&digest[..16])
}

/// 8-byte span id, hex-encoded (16 chars), derived from the trace id and
/// this exchange's position so ids are stable and collision-free within one
/// session's export.
fn deterministic_span_id(trace_id: &str, index: usize) -> String {
    let digest = Sha256::digest(format!("{trace_id}:{index}").as_bytes());
    hex_string(&digest[..8])
}

fn hex_string(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use mcptracer_storage::StoredMessage;

    use super::*;
    use crate::correlate;

    #[allow(clippy::too_many_arguments)]
    fn msg(
        seq: u64,
        ts_ns: i64,
        dir: &str,
        kind: &str,
        rpc_id: Option<&str>,
        method: Option<&str>,
        tool: Option<&str>,
        is_error: bool,
        error_code: Option<i64>,
    ) -> StoredMessage {
        StoredMessage {
            seq,
            ts_ns,
            direction: dir.to_string(),
            message_kind: kind.to_string(),
            rpc_id: rpc_id.map(String::from),
            method: method.map(String::from),
            tool_name: tool.map(String::from),
            payload: "{}".to_string(),
            payload_bytes: 2,
            is_error,
            error_code,
        }
    }

    fn fixture() -> Vec<StoredMessage> {
        vec![
            msg(
                0,
                100,
                "c2s",
                "request",
                Some("1"),
                Some("tools/call"),
                Some("echo"),
                false,
                None,
            ),
            msg(
                1,
                150,
                "s2c",
                "response",
                Some("1"),
                None,
                None,
                false,
                None,
            ),
            msg(
                2,
                200,
                "c2s",
                "request",
                Some("2"),
                Some("tools/call"),
                Some("delete"),
                false,
                None,
            ),
            msg(
                3,
                220,
                "s2c",
                "response",
                Some("2"),
                None,
                None,
                true,
                Some(-32000),
            ),
        ]
    }

    #[test]
    fn span_names_match_method_and_tool_convention() {
        let model = correlate(&fixture());
        let document = session_to_otlp_json("session-1", "codex", &model);
        let spans = document["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        let names: Vec<&str> = spans.iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["tools/call echo", "tools/call delete"]);
    }

    #[test]
    fn error_exchange_gets_error_status() {
        let model = correlate(&fixture());
        let document = session_to_otlp_json("session-1", "codex", &model);
        let spans = document["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans[0]["status"]["code"], 1);
        assert_eq!(spans[1]["status"]["code"], 2);
    }

    #[test]
    fn export_is_deterministic_across_runs() {
        let model = correlate(&fixture());
        let a = session_to_otlp_json("session-1", "codex", &model);
        let b = session_to_otlp_json("session-1", "codex", &model);
        assert_eq!(a, b);
    }

    #[test]
    fn trace_and_span_ids_are_valid_hex_lengths() {
        let model = correlate(&fixture());
        let document = session_to_otlp_json("session-1", "codex", &model);
        let spans = document["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        for span in spans {
            let trace_id = span["traceId"].as_str().unwrap();
            let span_id = span["spanId"].as_str().unwrap();
            assert_eq!(trace_id.len(), 32);
            assert_eq!(span_id.len(), 16);
            assert!(trace_id.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(span_id.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn orphan_responses_are_excluded() {
        let messages = vec![msg(
            0,
            100,
            "s2c",
            "response",
            Some("9"),
            None,
            None,
            false,
            None,
        )];
        let model = correlate(&messages);
        let document = session_to_otlp_json("session-1", "codex", &model);
        let spans = document["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert!(spans.is_empty());
    }
}
