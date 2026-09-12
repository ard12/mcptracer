//! Correlated session model for MCPTracer.
//!
//! [`correlate`] turns the flat, seq-ordered message log of one recorded session
//! into a structured view: requests paired with their responses, latencies,
//! statuses, standalone notifications, and session-level aggregates. This is the
//! data contract that `replay`, `diff`, `assert`, and `.mtrace` build on. See
//! `docs/spec/session-model.md` for the frozen design.
//!
//! ## Why direction matters
//!
//! MCP is bidirectional: servers can originate requests too (e.g.
//! `sampling/createMessage`). So a JSON-RPC id is not globally unique within a
//! session — it is scoped to the originator. Correlation therefore keys on
//! `(responder-direction, rpc_id)`, never `rpc_id` alone. Because a reply always
//! travels in the [`Direction::opposite`] direction from its request, the same
//! matching rule handles both client- and server-initiated calls.

pub mod assertions;
pub mod cache;
pub mod diff;
pub mod eval;
pub mod manifest;
pub mod optimize;
pub mod otel;
pub mod plan;
pub mod quota;
pub mod serve;

use std::collections::HashMap;

use mcptracer_protocol::Direction;
use mcptracer_storage::StoredMessage;
use serde::Serialize;

/// Outcome of a single request/response exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExchangeStatus {
    /// Request answered with a non-error response.
    Ok,
    /// Request answered with a JSON-RPC error response.
    Error,
    /// A `tools/call` response that completed at the JSON-RPC layer but whose
    /// tool execution reported `result.isError: true`.
    ToolError,
    /// A long-lived `subscriptions/listen` request whose stream sent the
    /// required acknowledgment notification. It has no terminal response yet.
    Subscribed,
    /// Request had no matching response in the session (pending at end, server
    /// crash, or a dropped record).
    Unanswered,
    /// A response with no matching prior request (protocol anomaly or a
    /// recording gap / dropped request record).
    OrphanResponse,
}

/// One correlated request/response pair (or an orphan on either side).
#[derive(Debug, Clone, Serialize)]
pub struct Exchange {
    /// `seq` of the request message, if present.
    pub request_seq: Option<u64>,
    /// `seq` of the response message, if present.
    pub response_seq: Option<u64>,
    /// JSON-encoded rpc id, exactly as stored (`1` -> `"1"`, `"x"` -> `"\"x\""`).
    pub rpc_id: Option<String>,
    /// Direction of the *request* (who originated the call). For an
    /// [`ExchangeStatus::OrphanResponse`] this is the direction the request
    /// would have had (opposite the response).
    pub origin: Direction,
    pub method: Option<String>,
    pub tool_name: Option<String>,
    pub status: ExchangeStatus,
    /// `response_ts_ns - request_ts_ns`. `None` unless both are present. This is
    /// a proxy-observed latency (capture-to-capture), not the server's internal
    /// processing time; it is still valid for relative comparison.
    pub latency_ns: Option<i64>,
    pub error_code: Option<i64>,
    pub request_ts_ns: Option<i64>,
    pub response_ts_ns: Option<i64>,
}

/// A standalone notification (no id, no response).
#[derive(Debug, Clone, Serialize)]
pub struct NotificationEvent {
    pub seq: u64,
    pub ts_ns: i64,
    pub direction: Direction,
    pub method: String,
}

/// The full correlated view of one session.
#[derive(Debug, Clone, Serialize)]
pub struct SessionModel {
    /// Exchanges ordered by request seq (orphan responses by their response seq).
    pub exchanges: Vec<Exchange>,
    pub notifications: Vec<NotificationEvent>,
    pub stats: SessionStats,
}

/// Session-level aggregates. Latency percentiles use answered exchanges only.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionStats {
    pub total_exchanges: usize,
    pub ok: usize,
    pub errors: usize,
    pub unanswered: usize,
    pub orphan_responses: usize,
    pub notifications: usize,
    pub latency_p50_ns: Option<i64>,
    pub latency_p95_ns: Option<i64>,
    pub latency_max_ns: Option<i64>,
    /// Count of calls per `tools/call` tool name, sorted by count desc then name.
    pub tool_call_counts: Vec<(String, usize)>,
}

/// A capture-integrity problem that makes a session unsafe to use as a
/// passing regression gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionIntegrityIssueKind {
    EmptySession,
    SessionNotClosed,
    DroppedMessages,
    NonMonotonicSequence,
    InvalidPayload,
    InvalidRedactionConfiguration,
    UnredactedSensitiveValue,
    UnknownDirection,
    UnknownMessageKind,
    UnansweredRequest,
    OrphanResponse,
}

/// One concrete capture-integrity problem with safe, payload-free context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionIntegrityIssue {
    pub kind: SessionIntegrityIssueKind,
    pub seq: Option<u64>,
    pub detail: String,
}

/// Deterministic integrity assessment for a stored session.
///
/// A report is healthy only when the capture has at least one message, no
/// known recording loss, valid stored JSON, recognized protocol metadata, and
/// no unmatched request/response pairs. This is deliberately stricter than a
/// descriptive session view: callers that act as CI gates must fail closed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SessionIntegrityReport {
    pub issues: Vec<SessionIntegrityIssue>,
}

impl SessionIntegrityReport {
    pub fn is_healthy(&self) -> bool {
        self.issues.is_empty()
    }
}

/// Build the correlated view from an ordered message slice.
///
/// `messages` MUST be ordered by `seq` ascending, exactly as
/// `mcptracer_storage::Store::get_messages` returns it.
pub fn correlate(messages: &[StoredMessage]) -> SessionModel {
    let mut exchanges: Vec<Exchange> = Vec::new();
    let mut notifications: Vec<NotificationEvent> = Vec::new();
    // (direction the response will travel, rpc_id) -> index into `exchanges`.
    let mut pending: HashMap<(Direction, String), usize> = HashMap::new();

    for msg in messages {
        let Some(dir) = Direction::from_db_str(&msg.direction) else {
            // Unknown direction encoding: skip defensively rather than panic.
            continue;
        };

        match msg.message_kind.as_str() {
            "request" => {
                let idx = exchanges.len();
                exchanges.push(Exchange {
                    request_seq: Some(msg.seq),
                    response_seq: None,
                    rpc_id: msg.rpc_id.clone(),
                    origin: dir,
                    method: msg.method.clone(),
                    tool_name: msg.tool_name.clone(),
                    status: ExchangeStatus::Unanswered,
                    latency_ns: None,
                    error_code: None,
                    request_ts_ns: Some(msg.ts_ns),
                    response_ts_ns: None,
                });
                if let Some(id) = &msg.rpc_id {
                    // The response arrives in the opposite direction. If an id is
                    // already in flight for this responder, the newer request
                    // claims it; the displaced one stays Unanswered.
                    pending.insert((dir.opposite(), id.clone()), idx);
                }
            }
            "response" => {
                let matched = msg
                    .rpc_id
                    .as_ref()
                    .and_then(|id| pending.remove(&(dir, id.clone())));

                if let Some(idx) = matched {
                    let e = &mut exchanges[idx];
                    e.response_seq = Some(msg.seq);
                    e.response_ts_ns = Some(msg.ts_ns);
                    e.error_code = msg.error_code;
                    if let Some(req_ts) = e.request_ts_ns {
                        e.latency_ns = Some(msg.ts_ns - req_ts);
                    }
                    e.status = if msg.is_error {
                        ExchangeStatus::Error
                    } else if e.method.as_deref() == Some("tools/call")
                        && serde_json::from_str::<serde_json::Value>(&msg.payload)
                            .ok()
                            .is_some_and(|payload| {
                                payload
                                    .pointer("/result/isError")
                                    .and_then(serde_json::Value::as_bool)
                                    == Some(true)
                            })
                    {
                        ExchangeStatus::ToolError
                    } else {
                        ExchangeStatus::Ok
                    };
                } else {
                    exchanges.push(Exchange {
                        request_seq: None,
                        response_seq: Some(msg.seq),
                        rpc_id: msg.rpc_id.clone(),
                        origin: dir.opposite(),
                        method: None,
                        tool_name: None,
                        status: ExchangeStatus::OrphanResponse,
                        latency_ns: None,
                        error_code: msg.error_code,
                        request_ts_ns: None,
                        response_ts_ns: Some(msg.ts_ns),
                    });
                }
            }
            "notification" => {
                if dir == Direction::ServerToClient
                    && msg.method.as_deref() == Some("notifications/subscriptions/acknowledged")
                {
                    let subscription_id = serde_json::from_str::<serde_json::Value>(&msg.payload)
                        .ok()
                        .and_then(|payload| {
                            payload
                                .pointer("/params/_meta/io.modelcontextprotocol~1subscriptionId")
                                .cloned()
                        })
                        .and_then(|id| match id {
                            serde_json::Value::String(_) | serde_json::Value::Number(_) => {
                                serde_json::to_string(&id).ok()
                            }
                            _ => None,
                        });
                    if let Some(subscription_id) = subscription_id {
                        if let Some(idx) = pending.get(&(dir, subscription_id)).copied() {
                            let exchange = &mut exchanges[idx];
                            if exchange.origin == Direction::ClientToServer
                                && exchange.method.as_deref() == Some("subscriptions/listen")
                            {
                                exchange.status = ExchangeStatus::Subscribed;
                            }
                        }
                    }
                }
                notifications.push(NotificationEvent {
                    seq: msg.seq,
                    ts_ns: msg.ts_ns,
                    direction: dir,
                    method: msg.method.clone().unwrap_or_default(),
                });
            }
            _ => {
                // Unknown message kind: skip defensively.
            }
        }
    }

    // Stable output order: by request seq, falling back to response seq (orphans).
    exchanges.sort_by_key(|e| e.request_seq.or(e.response_seq).unwrap_or(u64::MAX));

    let stats = compute_stats(&exchanges, &notifications);
    SessionModel {
        exchanges,
        notifications,
        stats,
    }
}

/// Validate stored capture data before it is used as a passing regression
/// gate. `dropped_messages` is session metadata maintained by storage, so it
/// is supplied separately from the message stream.
pub fn validate_session(
    messages: &[StoredMessage],
    dropped_messages: u64,
) -> SessionIntegrityReport {
    let mut report = SessionIntegrityReport::default();

    if messages.is_empty() {
        report.issues.push(SessionIntegrityIssue {
            kind: SessionIntegrityIssueKind::EmptySession,
            seq: None,
            detail: "session contains no recorded messages".to_string(),
        });
    }

    if dropped_messages > 0 {
        report.issues.push(SessionIntegrityIssue {
            kind: SessionIntegrityIssueKind::DroppedMessages,
            seq: None,
            detail: format!("session dropped {dropped_messages} message record(s)"),
        });
    }

    let mut previous_seq = None;
    for message in messages {
        if let Some(previous) = previous_seq {
            if message.seq <= previous {
                report.issues.push(SessionIntegrityIssue {
                    kind: SessionIntegrityIssueKind::NonMonotonicSequence,
                    seq: Some(message.seq),
                    detail: format!("sequence {0} follows {1}", message.seq, previous),
                });
            }
        }
        previous_seq = Some(message.seq);

        if Direction::from_db_str(&message.direction).is_none() {
            report.issues.push(SessionIntegrityIssue {
                kind: SessionIntegrityIssueKind::UnknownDirection,
                seq: Some(message.seq),
                detail: format!("unknown direction {}", message.direction),
            });
        }

        if !matches!(
            message.message_kind.as_str(),
            "request" | "response" | "notification"
        ) {
            report.issues.push(SessionIntegrityIssue {
                kind: SessionIntegrityIssueKind::UnknownMessageKind,
                seq: Some(message.seq),
                detail: format!("unknown message kind {}", message.message_kind),
            });
        }

        if serde_json::from_str::<serde_json::Value>(&message.payload).is_err() {
            report.issues.push(SessionIntegrityIssue {
                kind: SessionIntegrityIssueKind::InvalidPayload,
                seq: Some(message.seq),
                detail: "stored payload is not valid JSON".to_string(),
            });
        }
    }

    let model = correlate(messages);
    for exchange in model.exchanges {
        let (kind, seq, detail) = match exchange.status {
            ExchangeStatus::Unanswered => (
                SessionIntegrityIssueKind::UnansweredRequest,
                exchange.request_seq,
                "request has no matching response".to_string(),
            ),
            ExchangeStatus::OrphanResponse => (
                SessionIntegrityIssueKind::OrphanResponse,
                exchange.response_seq,
                "response has no matching request".to_string(),
            ),
            ExchangeStatus::Ok
            | ExchangeStatus::Error
            | ExchangeStatus::ToolError
            | ExchangeStatus::Subscribed => continue,
        };
        report
            .issues
            .push(SessionIntegrityIssue { kind, seq, detail });
    }

    report
}

fn compute_stats(exchanges: &[Exchange], notifications: &[NotificationEvent]) -> SessionStats {
    let mut stats = SessionStats {
        total_exchanges: exchanges.len(),
        notifications: notifications.len(),
        ..Default::default()
    };

    let mut latencies: Vec<i64> = Vec::new();
    let mut tool_counts: HashMap<String, usize> = HashMap::new();

    for e in exchanges {
        match e.status {
            ExchangeStatus::Ok | ExchangeStatus::Subscribed => stats.ok += 1,
            ExchangeStatus::Error | ExchangeStatus::ToolError => stats.errors += 1,
            ExchangeStatus::Unanswered => stats.unanswered += 1,
            ExchangeStatus::OrphanResponse => stats.orphan_responses += 1,
        }
        if let Some(latency) = e.latency_ns {
            latencies.push(latency);
        }
        if let Some(tool) = &e.tool_name {
            *tool_counts.entry(tool.clone()).or_insert(0) += 1;
        }
    }

    latencies.sort_unstable();
    stats.latency_p50_ns = percentile(&latencies, 0.50);
    stats.latency_p95_ns = percentile(&latencies, 0.95);
    stats.latency_max_ns = latencies.last().copied();

    let mut tool_call_counts: Vec<(String, usize)> = tool_counts.into_iter().collect();
    tool_call_counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    stats.tool_call_counts = tool_call_counts;

    stats
}

/// Nearest-rank percentile over a sorted slice: `sorted[ceil(p*n) - 1]`.
pub(crate) fn percentile(sorted: &[i64], p: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let n = sorted.len();
    let rank = (p * n as f64).ceil() as usize; // 1-based
    let idx = rank.saturating_sub(1).min(n - 1);
    Some(sorted[idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn m(
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

    /// Every other serialized enum in this workspace uses
    /// `#[serde(rename_all = "snake_case")]`; `ExchangeStatus` previously did
    /// not, so `diff --json`'s `status_changed` delta and `Exchange.status`
    /// leaked Rust variant names (`"Ok"`, `"ToolError"`, ...) into the public
    /// JSON API while every other status-like field used snake_case.
    #[test]
    fn exchange_status_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_value(ExchangeStatus::Ok).unwrap(),
            serde_json::json!("ok")
        );
        assert_eq!(
            serde_json::to_value(ExchangeStatus::Error).unwrap(),
            serde_json::json!("error")
        );
        assert_eq!(
            serde_json::to_value(ExchangeStatus::ToolError).unwrap(),
            serde_json::json!("tool_error")
        );
        assert_eq!(
            serde_json::to_value(ExchangeStatus::Subscribed).unwrap(),
            serde_json::json!("subscribed")
        );
        assert_eq!(
            serde_json::to_value(ExchangeStatus::Unanswered).unwrap(),
            serde_json::json!("unanswered")
        );
        assert_eq!(
            serde_json::to_value(ExchangeStatus::OrphanResponse).unwrap(),
            serde_json::json!("orphan_response")
        );
    }

    #[test]
    fn acknowledged_subscription_is_healthy_without_terminal_response() {
        let mut listen = m(
            0,
            10,
            "c2s",
            "request",
            Some("\"listen-1\""),
            Some("subscriptions/listen"),
            None,
            false,
            None,
        );
        listen.payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "listen-1",
            "method": "subscriptions/listen",
            "params": {"notifications": {"toolsListChanged": true}}
        })
        .to_string();
        let mut acknowledgement = m(
            1,
            20,
            "s2c",
            "notification",
            None,
            Some("notifications/subscriptions/acknowledged"),
            None,
            false,
            None,
        );
        acknowledgement.payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/subscriptions/acknowledged",
            "params": {
                "notifications": {"toolsListChanged": true},
                "_meta": {"io.modelcontextprotocol/subscriptionId": "listen-1"}
            }
        })
        .to_string();

        let messages = [listen, acknowledgement];
        let model = correlate(&messages);
        assert_eq!(model.exchanges[0].status, ExchangeStatus::Subscribed);
        assert_eq!(model.stats.ok, 1);
        assert!(validate_session(&messages, 0).is_healthy());
    }
    // 1
    #[test]
    fn client_initiated_happy_path() {
        let msgs = [
            m(
                0,
                100,
                "c2s",
                "request",
                Some("1"),
                Some("initialize"),
                None,
                false,
                None,
            ),
            m(
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
        ];
        let model = correlate(&msgs);
        assert_eq!(model.exchanges.len(), 1);
        let e = &model.exchanges[0];
        assert_eq!(e.status, ExchangeStatus::Ok);
        assert_eq!(e.origin, Direction::ClientToServer);
        assert_eq!(e.request_seq, Some(0));
        assert_eq!(e.response_seq, Some(1));
        assert_eq!(e.latency_ns, Some(50));
        assert_eq!(model.stats.ok, 1);
    }

    // 2
    #[test]
    fn server_initiated_call() {
        let msgs = [
            m(
                0,
                100,
                "s2c",
                "request",
                Some("1"),
                Some("sampling/createMessage"),
                None,
                false,
                None,
            ),
            m(
                1,
                130,
                "c2s",
                "response",
                Some("1"),
                None,
                None,
                false,
                None,
            ),
        ];
        let model = correlate(&msgs);
        assert_eq!(model.exchanges.len(), 1);
        let e = &model.exchanges[0];
        assert_eq!(e.origin, Direction::ServerToClient);
        assert_eq!(e.status, ExchangeStatus::Ok);
        assert_eq!(e.latency_ns, Some(30));
    }

    // 3
    #[test]
    fn id_collision_across_directions() {
        let msgs = [
            m(
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
            m(
                1,
                110,
                "s2c",
                "request",
                Some("1"),
                Some("roots/list"),
                None,
                false,
                None,
            ),
            m(
                2,
                150,
                "s2c",
                "response",
                Some("1"),
                None,
                None,
                false,
                None,
            ),
            m(
                3,
                160,
                "c2s",
                "response",
                Some("1"),
                None,
                None,
                false,
                None,
            ),
        ];
        let model = correlate(&msgs);
        assert_eq!(model.exchanges.len(), 2);
        assert_eq!(model.stats.ok, 2);

        let client_call = model
            .exchanges
            .iter()
            .find(|e| e.origin == Direction::ClientToServer)
            .unwrap();
        assert_eq!(client_call.tool_name.as_deref(), Some("echo"));
        assert_eq!(client_call.response_seq, Some(2));
        assert_eq!(client_call.latency_ns, Some(50));

        let server_call = model
            .exchanges
            .iter()
            .find(|e| e.origin == Direction::ServerToClient)
            .unwrap();
        assert_eq!(server_call.method.as_deref(), Some("roots/list"));
        assert_eq!(server_call.response_seq, Some(3));
        assert_eq!(server_call.latency_ns, Some(50));
    }

    // 4
    #[test]
    fn error_response() {
        let msgs = [
            m(
                0,
                100,
                "c2s",
                "request",
                Some("1"),
                Some("tools/call"),
                Some("nope"),
                false,
                None,
            ),
            m(
                1,
                120,
                "s2c",
                "response",
                Some("1"),
                None,
                None,
                true,
                Some(-32601),
            ),
        ];
        let model = correlate(&msgs);
        let e = &model.exchanges[0];
        assert_eq!(e.status, ExchangeStatus::Error);
        assert_eq!(e.error_code, Some(-32601));
        assert_eq!(model.stats.errors, 1);
    }

    // 5
    #[test]
    fn unanswered_request() {
        let msgs = [m(
            0,
            100,
            "c2s",
            "request",
            Some("1"),
            Some("tools/list"),
            None,
            false,
            None,
        )];
        let model = correlate(&msgs);
        assert_eq!(model.exchanges.len(), 1);
        let e = &model.exchanges[0];
        assert_eq!(e.status, ExchangeStatus::Unanswered);
        assert_eq!(e.latency_ns, None);
        assert_eq!(model.stats.unanswered, 1);
    }

    // 6
    #[test]
    fn orphan_response() {
        let msgs = [m(
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
        let model = correlate(&msgs);
        assert_eq!(model.exchanges.len(), 1);
        let e = &model.exchanges[0];
        assert_eq!(e.status, ExchangeStatus::OrphanResponse);
        assert_eq!(e.origin, Direction::ClientToServer);
        assert_eq!(e.response_seq, Some(0));
        assert_eq!(model.stats.orphan_responses, 1);
    }

    // 7
    #[test]
    fn notification_is_standalone() {
        let msgs = [m(
            0,
            100,
            "c2s",
            "notification",
            None,
            Some("notifications/initialized"),
            None,
            false,
            None,
        )];
        let model = correlate(&msgs);
        assert!(model.exchanges.is_empty());
        assert_eq!(model.notifications.len(), 1);
        assert_eq!(model.notifications[0].method, "notifications/initialized");
        assert_eq!(model.stats.notifications, 1);
    }

    // 8
    #[test]
    fn id_reuse_after_completion() {
        let msgs = [
            m(
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
            m(
                1,
                110,
                "s2c",
                "response",
                Some("1"),
                None,
                None,
                false,
                None,
            ),
            m(
                2,
                200,
                "c2s",
                "request",
                Some("1"),
                Some("tools/call"),
                Some("echo"),
                false,
                None,
            ),
            m(
                3,
                210,
                "s2c",
                "response",
                Some("1"),
                None,
                None,
                false,
                None,
            ),
        ];
        let model = correlate(&msgs);
        assert_eq!(model.exchanges.len(), 2);
        assert_eq!(model.stats.ok, 2);
        assert_eq!(model.exchanges[0].latency_ns, Some(10));
        assert_eq!(model.exchanges[1].latency_ns, Some(10));
    }

    // 9
    #[test]
    fn duplicate_in_flight_id() {
        let msgs = [
            m(
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
            m(
                1,
                110,
                "c2s",
                "request",
                Some("1"),
                Some("tools/call"),
                Some("echo"),
                false,
                None,
            ),
            m(
                2,
                150,
                "s2c",
                "response",
                Some("1"),
                None,
                None,
                false,
                None,
            ),
        ];
        let model = correlate(&msgs);
        assert_eq!(model.exchanges.len(), 2);

        let first = model
            .exchanges
            .iter()
            .find(|e| e.request_seq == Some(0))
            .unwrap();
        assert_eq!(first.status, ExchangeStatus::Unanswered);

        let second = model
            .exchanges
            .iter()
            .find(|e| e.request_seq == Some(1))
            .unwrap();
        assert_eq!(second.status, ExchangeStatus::Ok);
        assert_eq!(second.response_seq, Some(2));
        assert_eq!(second.latency_ns, Some(40));
    }

    // 10
    #[test]
    fn empty_session() {
        let model = correlate(&[]);
        assert!(model.exchanges.is_empty());
        assert!(model.notifications.is_empty());
        assert_eq!(model.stats.total_exchanges, 0);
        assert_eq!(model.stats.latency_p50_ns, None);
        assert_eq!(model.stats.latency_p95_ns, None);
        assert_eq!(model.stats.latency_max_ns, None);
    }

    // 11
    #[test]
    fn string_and_integer_ids_do_not_collide() {
        let msgs = [
            m(
                0,
                100,
                "c2s",
                "request",
                Some("\"list-1\""),
                Some("tools/list"),
                None,
                false,
                None,
            ),
            m(
                1,
                110,
                "c2s",
                "request",
                Some("1"),
                Some("tools/call"),
                Some("echo"),
                false,
                None,
            ),
            m(
                2,
                150,
                "s2c",
                "response",
                Some("1"),
                None,
                None,
                false,
                None,
            ),
            m(
                3,
                160,
                "s2c",
                "response",
                Some("\"list-1\""),
                None,
                None,
                false,
                None,
            ),
        ];
        let model = correlate(&msgs);
        assert_eq!(model.exchanges.len(), 2);
        assert_eq!(model.stats.ok, 2);

        let int_call = model
            .exchanges
            .iter()
            .find(|e| e.rpc_id.as_deref() == Some("1"))
            .unwrap();
        assert_eq!(int_call.response_seq, Some(2));
        assert_eq!(int_call.latency_ns, Some(40));

        let str_call = model
            .exchanges
            .iter()
            .find(|e| e.rpc_id.as_deref() == Some("\"list-1\""))
            .unwrap();
        assert_eq!(str_call.response_seq, Some(3));
        assert_eq!(str_call.latency_ns, Some(60));
    }

    // aggregate stats: exact percentiles and tool counts
    #[test]
    fn stats_percentiles_and_tool_counts() {
        // Four answered tools/call exchanges with latencies 10, 20, 30, 40.
        // Three "echo", one "read".
        let msgs = [
            m(
                0,
                0,
                "c2s",
                "request",
                Some("1"),
                Some("tools/call"),
                Some("echo"),
                false,
                None,
            ),
            m(1, 10, "s2c", "response", Some("1"), None, None, false, None),
            m(
                2,
                0,
                "c2s",
                "request",
                Some("2"),
                Some("tools/call"),
                Some("echo"),
                false,
                None,
            ),
            m(3, 20, "s2c", "response", Some("2"), None, None, false, None),
            m(
                4,
                0,
                "c2s",
                "request",
                Some("3"),
                Some("tools/call"),
                Some("echo"),
                false,
                None,
            ),
            m(5, 30, "s2c", "response", Some("3"), None, None, false, None),
            m(
                6,
                0,
                "c2s",
                "request",
                Some("4"),
                Some("tools/call"),
                Some("read"),
                false,
                None,
            ),
            m(7, 40, "s2c", "response", Some("4"), None, None, false, None),
        ];
        let model = correlate(&msgs);

        assert_eq!(model.stats.ok, 4);
        assert_eq!(model.stats.total_exchanges, 4);
        // sorted latencies [10,20,30,40]: p50 = idx ceil(2)-1 = 1 -> 20;
        // p95 = idx ceil(3.8)-1 = 3 -> 40.
        assert_eq!(model.stats.latency_p50_ns, Some(20));
        assert_eq!(model.stats.latency_p95_ns, Some(40));
        assert_eq!(model.stats.latency_max_ns, Some(40));
        assert_eq!(
            model.stats.tool_call_counts,
            vec![("echo".to_string(), 3), ("read".to_string(), 1)]
        );
    }

    #[test]
    fn validation_accepts_complete_session() {
        let messages = [
            m(
                0,
                100,
                "c2s",
                "request",
                Some("1"),
                Some("initialize"),
                None,
                false,
                None,
            ),
            m(
                1,
                110,
                "s2c",
                "response",
                Some("1"),
                None,
                None,
                false,
                None,
            ),
        ];

        assert!(validate_session(&messages, 0).is_healthy());
    }

    #[test]
    fn validation_reports_capture_integrity_problems() {
        let mut first = m(
            2,
            100,
            "c2s",
            "request",
            Some("1"),
            Some("tools/list"),
            None,
            false,
            None,
        );
        first.payload = "{".to_string();
        let second = m(
            2,
            110,
            "invalid-direction",
            "unknown-kind",
            Some("2"),
            None,
            None,
            false,
            None,
        );

        let report = validate_session(&[first, second], 3);
        let kinds: Vec<_> = report.issues.iter().map(|issue| issue.kind).collect();

        assert!(kinds.contains(&SessionIntegrityIssueKind::DroppedMessages));
        assert!(kinds.contains(&SessionIntegrityIssueKind::InvalidPayload));
        assert!(kinds.contains(&SessionIntegrityIssueKind::NonMonotonicSequence));
        assert!(kinds.contains(&SessionIntegrityIssueKind::UnknownDirection));
        assert!(kinds.contains(&SessionIntegrityIssueKind::UnknownMessageKind));
        assert!(kinds.contains(&SessionIntegrityIssueKind::UnansweredRequest));
        assert!(!report.is_healthy());
    }
}
