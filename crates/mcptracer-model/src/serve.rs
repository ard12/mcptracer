//! Deterministic request matcher for stdio serve-mode replay.
//!
//! [`MatchStrategy`] selects how a live client request is paired with a
//! recorded exchange (Agent VCR parity). The command layer (`serve`) never
//! changes; only which recorded response comes back changes.

use std::collections::BTreeMap;

use mcptracer_protocol::Direction;
use mcptracer_storage::StoredMessage;
use serde_json::Value;
use thiserror::Error;

use crate::{correlate, ExchangeStatus};

/// How a live client request is paired with a recorded exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchStrategy {
    /// Method, tool name, and canonicalized params must match exactly. No
    /// fallback: an unmatched request stays unmatched.
    Exact,
    /// Method only (ignores tool name and params). The next unused exchange
    /// with the same JSON-RPC method, in recorded order.
    Method,
    /// Exact match preferred; falls back to the next unused exchange with the
    /// same method and tool name when params differ (e.g. a live timestamp or
    /// request id embedded in `params`). This is the T-33 behavior.
    #[default]
    MethodAndParams,
    /// Method and tool name must match, and every key/value pair in the
    /// *recorded* request's params must also be present (with an equal value)
    /// in the *live* request's params. The live request may carry additional
    /// fields the recording never saw.
    Subset,
    /// Ignores method, tool name, and params entirely: always the next unused
    /// exchange in recorded order. For a client that always sends the same
    /// fixed sequence of calls.
    Sequential,
}

#[derive(Debug, Error)]
pub enum MockSessionError {
    #[error("recorded {side} message seq {seq} is not valid JSON: {source}")]
    InvalidPayload {
        seq: u64,
        side: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("recorded request seq {seq} has no string method")]
    MissingMethod { seq: u64 },
}

/// A response selected from a fully answered client-originated source exchange.
#[derive(Debug, Clone)]
pub struct MockResponse {
    pub response: Value,
    pub source_request_seq: u64,
    pub source_response_seq: u64,
}

#[derive(Debug, Clone)]
struct RecordedExchange {
    method: String,
    tool_name: Option<String>,
    params_key: String,
    /// Raw (non-canonicalized) `params`, used by [`MatchStrategy::Subset`].
    /// `serde_json::Value` object equality already ignores key order, so no
    /// canonicalization is needed for the containment check.
    params: Value,
    response: Value,
    source_request_seq: u64,
    source_response_seq: u64,
    used: bool,
}

/// A one-shot response inventory for an offline MCP mock server. A recorded
/// response cannot be reused, preserving source-session call order when a
/// client makes repeated requests.
#[derive(Debug, Clone, Default)]
pub struct MockSession {
    exchanges: Vec<RecordedExchange>,
    strategy: MatchStrategy,
}

impl MockSession {
    /// Build a mockable inventory using the default [`MatchStrategy`]
    /// (`MethodAndParams`).
    pub fn from_messages(messages: &[StoredMessage]) -> Result<Self, MockSessionError> {
        Self::from_messages_with_strategy(messages, MatchStrategy::default())
    }

    /// Build a mockable inventory from answered requests sent by the original
    /// client, matched using `strategy`. Server-initiated exchanges are
    /// deliberately excluded because serve-mode is acting as the server.
    pub fn from_messages_with_strategy(
        messages: &[StoredMessage],
        strategy: MatchStrategy,
    ) -> Result<Self, MockSessionError> {
        let by_seq: BTreeMap<u64, &StoredMessage> = messages
            .iter()
            .map(|message| (message.seq, message))
            .collect();
        let model = correlate(messages);
        let mut exchanges = Vec::new();

        for exchange in model.exchanges {
            if exchange.origin != Direction::ClientToServer
                || !matches!(exchange.status, ExchangeStatus::Ok | ExchangeStatus::Error)
            {
                continue;
            }
            let (Some(request_seq), Some(response_seq)) =
                (exchange.request_seq, exchange.response_seq)
            else {
                continue;
            };
            let request_message = by_seq
                .get(&request_seq)
                .expect("correlated request seq must exist in source messages");
            let response_message = by_seq
                .get(&response_seq)
                .expect("correlated response seq must exist in source messages");
            let request: Value =
                serde_json::from_str(&request_message.payload).map_err(|source| {
                    MockSessionError::InvalidPayload {
                        seq: request_seq,
                        side: "request",
                        source,
                    }
                })?;
            let response: Value =
                serde_json::from_str(&response_message.payload).map_err(|source| {
                    MockSessionError::InvalidPayload {
                        seq: response_seq,
                        side: "response",
                        source,
                    }
                })?;
            let method = request
                .get("method")
                .and_then(Value::as_str)
                .ok_or(MockSessionError::MissingMethod { seq: request_seq })?
                .to_string();

            exchanges.push(RecordedExchange {
                tool_name: tool_name(&request),
                params_key: params_key(&request),
                params: request.get("params").cloned().unwrap_or(Value::Null),
                method,
                response,
                source_request_seq: request_seq,
                source_response_seq: response_seq,
                used: false,
            });
        }

        Ok(Self {
            exchanges,
            strategy,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.exchanges.is_empty()
    }

    /// Match a live client request under this session's [`MatchStrategy`].
    pub fn match_request(&mut self, request: &Value) -> Option<MockResponse> {
        let method = request.get("method")?.as_str()?;
        let tool_name = tool_name(request);
        let params_key = params_key(request);
        let live_params = request.get("params").cloned().unwrap_or(Value::Null);

        let index = match self.strategy {
            MatchStrategy::Sequential => self.exchanges.iter().position(|c| !c.used),
            MatchStrategy::Method => self
                .exchanges
                .iter()
                .position(|c| !c.used && c.method == method),
            MatchStrategy::Exact => self.exchanges.iter().position(|c| {
                !c.used
                    && c.method == method
                    && c.tool_name == tool_name
                    && c.params_key == params_key
            }),
            MatchStrategy::MethodAndParams => {
                let exact = self.exchanges.iter().position(|c| {
                    !c.used
                        && c.method == method
                        && c.tool_name == tool_name
                        && c.params_key == params_key
                });
                exact.or_else(|| {
                    self.exchanges
                        .iter()
                        .position(|c| !c.used && c.method == method && c.tool_name == tool_name)
                })
            }
            MatchStrategy::Subset => self.exchanges.iter().position(|c| {
                !c.used
                    && c.method == method
                    && c.tool_name == tool_name
                    && params_subset(&c.params, &live_params)
            }),
        };

        let candidate = self.exchanges.get_mut(index?)?;
        candidate.used = true;
        Some(MockResponse {
            response: candidate.response.clone(),
            source_request_seq: candidate.source_request_seq,
            source_response_seq: candidate.source_response_seq,
        })
    }
}

fn tool_name(request: &Value) -> Option<String> {
    (request.get("method").and_then(Value::as_str) == Some("tools/call"))
        .then(|| {
            request
                .get("params")
                .and_then(|params| params.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .flatten()
}

fn params_key(request: &Value) -> String {
    canonicalize(request.get("params").unwrap_or(&Value::Null)).to_string()
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted: BTreeMap<_, _> = object
                .iter()
                .map(|(key, value)| (key.clone(), canonicalize(value)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        value => value.clone(),
    }
}

/// True when every key/value pair in `recorded` is also present (with an
/// equal value) in `live`. `live` may carry additional fields the recording
/// never saw. A non-object `recorded` value must equal `live` exactly; a
/// `Null` `recorded` value (no params were recorded) is always satisfied.
fn params_subset(recorded: &Value, live: &Value) -> bool {
    match recorded {
        Value::Null => true,
        Value::Object(recorded_map) => match live {
            Value::Object(live_map) => recorded_map
                .iter()
                .all(|(key, value)| live_map.get(key) == Some(value)),
            _ => false,
        },
        other => other == live,
    }
}

#[cfg(test)]
mod tests {
    use mcptracer_storage::StoredMessage;
    use serde_json::json;

    use super::{MatchStrategy, MockSession};

    fn message(
        seq: u64,
        direction: &str,
        kind: &str,
        rpc_id: &str,
        payload: serde_json::Value,
    ) -> StoredMessage {
        StoredMessage {
            seq,
            ts_ns: seq as i64,
            direction: direction.to_string(),
            message_kind: kind.to_string(),
            rpc_id: Some(rpc_id.to_string()),
            method: payload
                .get("method")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            tool_name: payload
                .get("params")
                .and_then(|params| params.get("name"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            payload: payload.to_string(),
            payload_bytes: 0,
            is_error: payload.get("error").is_some(),
            error_code: None,
        }
    }

    fn source_messages() -> Vec<StoredMessage> {
        vec![
            message(
                0,
                "c2s",
                "request",
                "1",
                json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"a":1,"b":2}}}),
            ),
            message(
                1,
                "s2c",
                "response",
                "1",
                json!({"jsonrpc":"2.0","id":1,"result":{"text":"first"}}),
            ),
            message(
                2,
                "c2s",
                "request",
                "2",
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo","arguments":{"a":3}}}),
            ),
            message(
                3,
                "s2c",
                "response",
                "2",
                json!({"jsonrpc":"2.0","id":2,"result":{"text":"second"}}),
            ),
            message(
                4,
                "s2c",
                "request",
                "9",
                json!({"jsonrpc":"2.0","id":9,"method":"roots/list","params":{}}),
            ),
            message(
                5,
                "c2s",
                "response",
                "9",
                json!({"jsonrpc":"2.0","id":9,"result":{"roots":[]}}),
            ),
        ]
    }

    #[test]
    fn matches_canonicalized_params_before_ordinal_fallback() {
        let mut session = MockSession::from_messages(&source_messages()).unwrap();
        let request = json!({"jsonrpc":"2.0","id":"new","method":"tools/call","params":{"arguments":{"b":2,"a":1},"name":"echo"}});

        let response = session.match_request(&request).unwrap();

        assert_eq!(response.source_request_seq, 0);
        assert_eq!(response.response["result"]["text"], "first");
    }

    #[test]
    fn falls_back_to_next_same_method_and_tool_in_source_order() {
        let mut session = MockSession::from_messages(&source_messages()).unwrap();
        let dynamic_request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"timestamp":99}}});

        let first = session.match_request(&dynamic_request).unwrap();
        let second = session.match_request(&dynamic_request).unwrap();

        assert_eq!(first.response["result"]["text"], "first");
        assert_eq!(second.response["result"]["text"], "second");
        assert!(session.match_request(&dynamic_request).is_none());
    }

    #[test]
    fn ignores_server_initiated_exchanges() {
        let mut session = MockSession::from_messages(&source_messages()).unwrap();
        let request = json!({"jsonrpc":"2.0","id":9,"method":"roots/list","params":{}});

        assert!(session.match_request(&request).is_none());
    }

    #[test]
    fn exact_strategy_matches_precisely_and_never_falls_back() {
        let mut session =
            MockSession::from_messages_with_strategy(&source_messages(), MatchStrategy::Exact)
                .unwrap();

        let exact_match = json!({"jsonrpc":"2.0","id":"x","method":"tools/call","params":{"arguments":{"b":2,"a":1},"name":"echo"}});
        let response = session.match_request(&exact_match).unwrap();
        assert_eq!(response.response["result"]["text"], "first");

        // Dynamic params that don't exactly match either recorded call: no
        // ordinal fallback under `Exact`, unlike `MethodAndParams`.
        let dynamic = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"timestamp":99}}});
        assert!(session.match_request(&dynamic).is_none());
    }

    #[test]
    fn method_strategy_ignores_tool_name_and_params() {
        let mut session =
            MockSession::from_messages_with_strategy(&source_messages(), MatchStrategy::Method)
                .unwrap();
        let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"totally_different","arguments":{"nonsense":true}}});

        let first = session.match_request(&request).unwrap();
        let second = session.match_request(&request).unwrap();

        assert_eq!(first.response["result"]["text"], "first");
        assert_eq!(second.response["result"]["text"], "second");
        assert!(session.match_request(&request).is_none());
    }

    #[test]
    fn subset_strategy_tolerates_extra_live_fields_but_requires_recorded_values() {
        let mut session =
            MockSession::from_messages_with_strategy(&source_messages(), MatchStrategy::Subset)
                .unwrap();

        // The recorded params {"name":"echo","arguments":{"a":1,"b":2}} are a
        // subset of the live request, which carries an extra field the
        // recording never saw.
        let with_extra_field = json!({"jsonrpc":"2.0","id":"x","method":"tools/call","params":{"name":"echo","arguments":{"a":1,"b":2},"clientRequestId":"abc-123"}});
        let response = session.match_request(&with_extra_field).unwrap();
        assert_eq!(response.response["result"]["text"], "first");

        // Neither recorded call's `arguments` value is contained in this
        // request, so nothing matches.
        let mismatched_value = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"a":999}}});
        assert!(session.match_request(&mismatched_value).is_none());
    }

    #[test]
    fn sequential_strategy_ignores_request_content_and_uses_recorded_order() {
        let mut session =
            MockSession::from_messages_with_strategy(&source_messages(), MatchStrategy::Sequential)
                .unwrap();
        let anything =
            json!({"jsonrpc":"2.0","id":1,"method":"totally/unrelated","params":{"whatever":true}});

        let first = session.match_request(&anything).unwrap();
        let second = session.match_request(&anything).unwrap();

        assert_eq!(first.response["result"]["text"], "first");
        assert_eq!(second.response["result"]["text"], "second");
        assert!(session.match_request(&anything).is_none());
    }
}
