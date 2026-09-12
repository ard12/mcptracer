//! Offline agent-behavior scoring against an expected-tool-call spec.
//!
//! Scores a recorded session against a declarative spec of expected and
//! forbidden tool calls — an accuracy metric, not just pass/fail, so it can
//! be tracked over time. Runs entirely offline against a recorded artifact;
//! no live model is involved.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::assertions::toml_to_json;
use crate::diff::payloads_by_seq;
use crate::{correlate, Exchange};
use mcptracer_storage::StoredMessage;

#[derive(Debug, Clone, Deserialize)]
pub struct ExpectedCall {
    pub tool: String,
    /// A subset of the call's `arguments` that must be present with equal
    /// values. Extra arguments the call has beyond this are ignored.
    #[serde(default)]
    pub required_arguments: Option<toml::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForbiddenCall {
    pub tool: String,
}

/// The parsed spec: an ordered or unordered list of `[[expect]]` calls, plus
/// `[[forbidden]]` calls that must never occur.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EvalSpec {
    /// When true, expected calls must occur in this relative order (other,
    /// unlisted calls may still happen in between). When false (default),
    /// any order satisfies the spec.
    #[serde(default)]
    pub ordered: bool,
    #[serde(rename = "expect", default)]
    pub expected: Vec<ExpectedCall>,
    #[serde(rename = "forbidden", default)]
    pub forbidden: Vec<ForbiddenCall>,
}

/// Parse a TOML eval spec. Errors here are spec errors (CLI exit 2), the
/// same convention `assertions::parse_spec` uses.
pub fn parse_spec(input: &str) -> Result<EvalSpec, String> {
    let spec: EvalSpec = toml::from_str(input).map_err(|e| e.to_string())?;
    if spec.expected.is_empty() && spec.forbidden.is_empty() {
        return Err("spec contains no [[expect]] or [[forbidden]] entries".to_string());
    }
    Ok(spec)
}

/// One expectation's outcome.
#[derive(Debug, Clone, Serialize)]
pub struct ExpectationResult {
    pub description: String,
    pub satisfied: bool,
    pub reason: Option<String>,
}

/// `accuracy` is the fraction of expected + forbidden checks satisfied
/// (1.0 = every expected call was found and no forbidden call occurred).
#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub accuracy: f64,
    pub results: Vec<ExpectationResult>,
}

pub fn evaluate(spec: &EvalSpec, messages: &[StoredMessage]) -> EvalReport {
    let model = correlate(messages);
    let payloads: BTreeMap<u64, Value> = payloads_by_seq(messages);

    let calls: Vec<(&Exchange, Value)> = model
        .exchanges
        .iter()
        .filter(|exchange| exchange.tool_name.is_some())
        .map(|exchange| {
            let arguments = exchange
                .request_seq
                .and_then(|seq| payloads.get(&seq))
                .and_then(|payload| payload.get("params"))
                .and_then(|params| params.get("arguments"))
                .cloned()
                .unwrap_or(Value::Null);
            (exchange, arguments)
        })
        .collect();

    let mut used = vec![false; calls.len()];
    let mut results = Vec::new();
    // Ordinal position of the last successfully matched call, for `ordered`.
    let mut last_matched_position: Option<usize> = None;

    for expectation in &spec.expected {
        let required = expectation.required_arguments.as_ref().map(toml_to_json);
        let mut found = None;
        for (position, (exchange, arguments)) in calls.iter().enumerate() {
            if used[position] {
                continue;
            }
            if exchange.tool_name.as_deref() != Some(expectation.tool.as_str()) {
                continue;
            }
            if spec.ordered {
                if let Some(last) = last_matched_position {
                    if position <= last {
                        continue;
                    }
                }
            }
            if let Some(required) = &required {
                if !arguments_subset(required, arguments) {
                    continue;
                }
            }
            found = Some(position);
            break;
        }

        match found {
            Some(position) => {
                used[position] = true;
                last_matched_position = Some(position);
                results.push(ExpectationResult {
                    description: format!("expect {}", expectation.tool),
                    satisfied: true,
                    reason: None,
                });
            }
            None => {
                results.push(ExpectationResult {
                    description: format!("expect {}", expectation.tool),
                    satisfied: false,
                    reason: Some(format!(
                        "no matching call to `{}` found{}",
                        expectation.tool,
                        if spec.ordered {
                            " in the expected order"
                        } else {
                            ""
                        }
                    )),
                });
            }
        }
    }

    for forbidden in &spec.forbidden {
        let violated = calls
            .iter()
            .any(|(exchange, _)| exchange.tool_name.as_deref() == Some(forbidden.tool.as_str()));
        results.push(ExpectationResult {
            description: format!("forbidden {}", forbidden.tool),
            satisfied: !violated,
            reason: violated.then(|| format!("forbidden tool `{}` was called", forbidden.tool)),
        });
    }

    let satisfied = results.iter().filter(|result| result.satisfied).count();
    let accuracy = if results.is_empty() {
        1.0
    } else {
        satisfied as f64 / results.len() as f64
    };

    EvalReport { accuracy, results }
}

/// True when every key/value pair in `required` is present (with an equal
/// value) in `actual`. `actual` may carry additional fields. Mirrors
/// `mcptracer_model::serve`'s subset matcher.
fn arguments_subset(required: &Value, actual: &Value) -> bool {
    match required {
        Value::Object(required_map) => match actual {
            Value::Object(actual_map) => required_map
                .iter()
                .all(|(key, value)| actual_map.get(key) == Some(value)),
            _ => false,
        },
        other => other == actual,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn msg(
        seq: u64,
        ts_ns: i64,
        dir: &str,
        kind: &str,
        rpc_id: Option<&str>,
        method: Option<&str>,
        tool: Option<&str>,
        payload: Value,
    ) -> StoredMessage {
        StoredMessage {
            seq,
            ts_ns,
            direction: dir.to_string(),
            message_kind: kind.to_string(),
            rpc_id: rpc_id.map(String::from),
            method: method.map(String::from),
            tool_name: tool.map(String::from),
            payload: payload.to_string(),
            payload_bytes: 0,
            is_error: false,
            error_code: None,
        }
    }

    fn call(seq: u64, tool: &str, arguments: Value) -> Vec<StoredMessage> {
        vec![
            msg(
                seq * 2,
                0,
                "c2s",
                "request",
                Some(&seq.to_string()),
                Some("tools/call"),
                Some(tool),
                json!({"jsonrpc":"2.0","id":seq,"method":"tools/call","params":{"name":tool,"arguments":arguments}}),
            ),
            msg(
                seq * 2 + 1,
                10,
                "s2c",
                "response",
                Some(&seq.to_string()),
                None,
                None,
                json!({"jsonrpc":"2.0","id":seq,"result":{}}),
            ),
        ]
    }

    #[test]
    fn exact_match_scores_perfectly() {
        let messages = call(0, "search", json!({}));
        let spec = parse_spec("[[expect]]\ntool = \"search\"\n").unwrap();

        let report = evaluate(&spec, &messages);

        assert_eq!(report.accuracy, 1.0);
        assert!(report.results[0].satisfied);
    }

    #[test]
    fn wrong_tool_scores_less_than_one_with_breakdown() {
        let messages = call(0, "delete", json!({}));
        let spec = parse_spec("[[expect]]\ntool = \"search\"\n").unwrap();

        let report = evaluate(&spec, &messages);

        assert!(report.accuracy < 1.0);
        assert_eq!(report.results.len(), 1);
        assert!(!report.results[0].satisfied);
        assert!(report.results[0]
            .reason
            .as_ref()
            .unwrap()
            .contains("no matching call"));
    }

    #[test]
    fn partial_match_reports_per_expectation_breakdown() {
        let mut messages = call(0, "search", json!({}));
        messages.extend(call(1, "summarize", json!({})));
        let spec =
            parse_spec("[[expect]]\ntool = \"search\"\n\n[[expect]]\ntool = \"missing_tool\"\n")
                .unwrap();

        let report = evaluate(&spec, &messages);

        assert_eq!(report.accuracy, 0.5);
        assert!(report.results[0].satisfied);
        assert!(!report.results[1].satisfied);
    }

    #[test]
    fn required_arguments_must_be_a_subset_of_the_call() {
        let messages = call(0, "search", json!({"query": "weather", "page": 2}));
        let spec = parse_spec(
            "[[expect]]\ntool = \"search\"\nrequired_arguments = { query = \"weather\" }\n",
        )
        .unwrap();
        let ok = evaluate(&spec, &messages);
        assert_eq!(ok.accuracy, 1.0);

        let mismatched_spec = parse_spec(
            "[[expect]]\ntool = \"search\"\nrequired_arguments = { query = \"news\" }\n",
        )
        .unwrap();
        let fail = evaluate(&mismatched_spec, &messages);
        assert!(fail.accuracy < 1.0);
    }

    #[test]
    fn ordered_mode_rejects_out_of_order_calls() {
        let mut messages = call(0, "b", json!({}));
        messages.extend(call(1, "a", json!({})));
        let spec =
            parse_spec("ordered = true\n[[expect]]\ntool = \"a\"\n\n[[expect]]\ntool = \"b\"\n")
                .unwrap();

        let report = evaluate(&spec, &messages);

        // `a` is found first (at position 1, the only "a"), then `b` must
        // come after it — but `b` was called before `a`, so no later `b`
        // exists to satisfy the second expectation.
        assert!(report.accuracy < 1.0);
    }

    #[test]
    fn ordered_mode_accepts_in_order_calls() {
        let mut messages = call(0, "a", json!({}));
        messages.extend(call(1, "b", json!({})));
        let spec =
            parse_spec("ordered = true\n[[expect]]\ntool = \"a\"\n\n[[expect]]\ntool = \"b\"\n")
                .unwrap();

        let report = evaluate(&spec, &messages);

        assert_eq!(report.accuracy, 1.0);
    }

    #[test]
    fn forbidden_call_reduces_accuracy() {
        let mut messages = call(0, "search", json!({}));
        messages.extend(call(1, "delete_everything", json!({})));
        let spec = parse_spec(
            "[[expect]]\ntool = \"search\"\n\n[[forbidden]]\ntool = \"delete_everything\"\n",
        )
        .unwrap();

        let report = evaluate(&spec, &messages);

        assert_eq!(report.accuracy, 0.5);
        let forbidden_result = report
            .results
            .iter()
            .find(|r| r.description.contains("delete_everything"))
            .unwrap();
        assert!(!forbidden_result.satisfied);
    }

    #[test]
    fn spec_errors_on_empty_spec() {
        assert!(parse_spec("").is_err());
        assert!(parse_spec("ordered = true\n").is_err());
    }
}
