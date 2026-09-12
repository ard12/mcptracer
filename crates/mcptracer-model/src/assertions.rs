//! Declarative assertions over a recorded session.
//!
//! Parses a TOML spec (`docs/spec/assertions.md`) into typed rules and
//! evaluates them against the correlated session model. Spec errors (malformed
//! TOML, unknown kinds) are a distinct failure class from assertion failures so
//! CI can tell "test failed" from "test is broken" (exit 2 vs exit 1 at the
//! CLI layer).

use std::collections::BTreeMap;

use mcptracer_redact::REDACTED_PLACEHOLDER;
use mcptracer_storage::StoredMessage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::diff::{extract_tools, tool_hash, ToolDef};
use crate::{correlate, Exchange, ExchangeStatus};

/// One assertion rule. `kind` selects the variant; unknown kinds or fields are
/// spec errors.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Assertion {
    /// No exchange (optionally scoped to `tool`) has status Error/Unanswered.
    NoErrors { tool: Option<String> },
    /// A `tools/call` for `tool` occurred; optional count bounds (min default 1).
    ToolCalled {
        tool: String,
        min: Option<usize>,
        max: Option<usize>,
    },
    /// A method occurred; optional count bounds (min default 1).
    MethodCalled {
        method: String,
        min: Option<usize>,
        max: Option<usize>,
    },
    /// Every occurrence of `after` follows at least one occurrence of `before`.
    /// Names match a method or a tool name.
    CallOrder { before: String, after: String },
    /// Latency bounds (proxy-observed) over answered exchanges in scope.
    Latency {
        tool: Option<String>,
        method: Option<String>,
        p50_under_ms: Option<f64>,
        p95_under_ms: Option<f64>,
        max_under_ms: Option<f64>,
    },
    /// JSON-pointer value inside matching responses equals/contains a value.
    /// Redacted values cannot be asserted on and fail with a clear reason.
    ResponseMatches {
        tool: Option<String>,
        method: Option<String>,
        pointer: String,
        equals: Option<toml::Value>,
        contains: Option<String>,
    },
    /// Every pinned tool's `(name, description, inputSchema)` hash, from the
    /// session's last `tools/list` response, matches a stored golden value —
    /// the recorded-regression counterpart to live tool pinning. An empty
    /// `hashes` map fails and reports each discovered tool's current hash, to
    /// bootstrap a golden set from a trusted recording.
    ToolsPinned {
        #[serde(default)]
        hashes: BTreeMap<String, String>,
    },
    /// Quota rate-limit and token burst bounds for the session ($0 offline cost).
    Quota {
        preset: Option<String>,
        refill_rate: Option<f64>,
        capacity: Option<f64>,
        max_violations: Option<usize>,
        max_burst_factor: Option<f64>,
        max_peak_tokens_per_sec: Option<u64>,
    },
}

/// One spec entry: an optional human label plus the rule itself.
#[derive(Debug, Clone, Deserialize)]
pub struct AssertionEntry {
    /// Optional label shown in PASS/FAIL output instead of the
    /// auto-generated description — e.g. `description = "echo is called once"`.
    #[serde(default)]
    pub description: Option<String>,
    #[serde(flatten)]
    pub rule: Assertion,
}

/// The parsed spec: a list of `[[assert]]` tables (`[[assertions]]` is
/// accepted as an alias).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssertSpec {
    #[serde(rename = "assert", alias = "assertions", default)]
    pub assertions: Vec<AssertionEntry>,
}

/// Parse a TOML assertion spec. Errors here are spec errors (CLI exit 2).
pub fn parse_spec(input: &str) -> Result<AssertSpec, String> {
    let spec: AssertSpec = toml::from_str(input).map_err(|e| e.to_string())?;
    if spec.assertions.is_empty() {
        return Err("spec contains no [[assert]] entries".to_string());
    }
    for entry in &spec.assertions {
        if let Assertion::Latency {
            p50_under_ms,
            p95_under_ms,
            max_under_ms,
            ..
        } = &entry.rule
        {
            if p50_under_ms.is_none() && p95_under_ms.is_none() && max_under_ms.is_none() {
                return Err(
                    "latency assertion needs one of p50_under_ms/p95_under_ms/max_under_ms"
                        .to_string(),
                );
            }
            for (name, value) in [
                ("p50_under_ms", *p50_under_ms),
                ("p95_under_ms", *p95_under_ms),
                ("max_under_ms", *max_under_ms),
            ] {
                if let Some(value) = value {
                    if !value.is_finite() || value < 0.0 {
                        return Err(format!(
                            "latency assertion `{name}` must be a finite non-negative number"
                        ));
                    }
                }
            }
        }
        if let Assertion::ResponseMatches {
            equals, contains, ..
        } = &entry.rule
        {
            if equals.is_none() && contains.is_none() {
                return Err("response_matches needs `equals` or `contains`".to_string());
            }
        }
        if let Assertion::Quota {
            preset,
            refill_rate,
            capacity,
            ..
        } = &entry.rule
        {
            let has_explicit_bounds = refill_rate.is_some() && capacity.is_some();
            if !has_explicit_bounds {
                match preset.as_deref() {
                    Some(name) if crate::quota::ProviderPreset::from_name(name).is_some() => {}
                    Some(name) => {
                        return Err(format!(
                            "quota assertion has unknown preset `{name}`; provide a valid preset or both refill_rate and capacity"
                        ));
                    }
                    None => {
                        return Err(
                            "quota assertion needs `preset`, or both `refill_rate` and `capacity`"
                                .to_string(),
                        );
                    }
                }
            }
        }
    }
    Ok(spec)
}

/// Outcome of a single assertion.
#[derive(Debug, Clone, Serialize)]
pub struct AssertionResult {
    /// Human description of the rule, e.g. `tool_called echo`.
    pub description: String,
    pub passed: bool,
    /// Failure reason when `passed` is false.
    pub reason: Option<String>,
}

/// Evaluate every assertion in the spec against a session's messages.
pub fn evaluate(spec: &AssertSpec, messages: &[StoredMessage]) -> Vec<AssertionResult> {
    let model = correlate(messages);
    let payloads: BTreeMap<u64, Value> = messages
        .iter()
        .filter_map(|m| serde_json::from_str(&m.payload).ok().map(|v| (m.seq, v)))
        .collect();
    let tools = extract_tools(messages);

    spec.assertions
        .iter()
        .map(|entry| {
            let mut result =
                evaluate_one(&entry.rule, &model.exchanges, &payloads, &tools, messages);
            if let Some(label) = &entry.description {
                result.description = label.clone();
            }
            result
        })
        .collect()
}

fn in_scope(exchange: &Exchange, tool: &Option<String>, method: &Option<String>) -> bool {
    if let Some(tool) = tool {
        if exchange.tool_name.as_deref() != Some(tool.as_str()) {
            return false;
        }
    }
    if let Some(method) = method {
        if exchange.method.as_deref() != Some(method.as_str()) {
            return false;
        }
    }
    true
}

fn name_matches(exchange: &Exchange, name: &str) -> bool {
    exchange.method.as_deref() == Some(name) || exchange.tool_name.as_deref() == Some(name)
}

fn evaluate_one(
    assertion: &Assertion,
    exchanges: &[Exchange],
    payloads: &BTreeMap<u64, Value>,
    tools: &BTreeMap<String, ToolDef>,
    messages: &[StoredMessage],
) -> AssertionResult {
    match assertion {
        Assertion::NoErrors { tool } => {
            let bad: Vec<String> = exchanges
                .iter()
                .filter(|e| in_scope(e, tool, &None))
                .filter(|e| {
                    matches!(
                        e.status,
                        ExchangeStatus::Error
                            | ExchangeStatus::ToolError
                            | ExchangeStatus::Unanswered
                    )
                })
                .map(describe_exchange)
                .collect();
            result(
                match tool {
                    Some(tool) => format!("no_errors (tool {tool})"),
                    None => "no_errors".to_string(),
                },
                bad.is_empty(),
                (!bad.is_empty()).then(|| format!("failing exchanges: {}", bad.join(", "))),
            )
        }
        Assertion::ToolCalled { tool, min, max } => {
            let count = exchanges
                .iter()
                .filter(|e| e.tool_name.as_deref() == Some(tool.as_str()))
                .count();
            check_bounds(format!("tool_called {tool}"), count, *min, *max)
        }
        Assertion::MethodCalled { method, min, max } => {
            let count = exchanges
                .iter()
                .filter(|e| e.method.as_deref() == Some(method.as_str()))
                .count();
            check_bounds(format!("method_called {method}"), count, *min, *max)
        }
        Assertion::CallOrder { before, after } => {
            let first_before = exchanges.iter().position(|e| name_matches(e, before));
            let after_positions: Vec<usize> = exchanges
                .iter()
                .enumerate()
                .filter(|(_, e)| name_matches(e, after))
                .map(|(i, _)| i)
                .collect();
            let description = format!("call_order {before} -> {after}");
            if after_positions.is_empty() {
                return result(
                    description,
                    false,
                    Some(format!("no occurrence of `{after}` in session")),
                );
            }
            match first_before {
                None => result(
                    description,
                    false,
                    Some(format!("no occurrence of `{before}` in session")),
                ),
                Some(before_idx) => {
                    let violating = after_positions.iter().any(|idx| *idx < before_idx);
                    result(
                        description,
                        !violating,
                        violating
                            .then(|| format!("`{after}` occurred before the first `{before}`")),
                    )
                }
            }
        }
        Assertion::Latency {
            tool,
            method,
            p50_under_ms,
            p95_under_ms,
            max_under_ms,
        } => {
            let mut latencies: Vec<i64> = exchanges
                .iter()
                .filter(|e| in_scope(e, tool, method))
                .filter_map(|e| e.latency_ns)
                .collect();
            let scope = tool
                .clone()
                .or_else(|| method.clone())
                .unwrap_or_else(|| "all".to_string());
            let description = format!("latency ({scope})");
            if latencies.is_empty() {
                return result(
                    description,
                    false,
                    Some("no answered exchanges matched scope".to_string()),
                );
            }
            latencies.sort_unstable();
            let mut failures = Vec::new();
            let checks = [
                ("p50", *p50_under_ms, crate::percentile(&latencies, 0.50)),
                ("p95", *p95_under_ms, crate::percentile(&latencies, 0.95)),
                ("max", *max_under_ms, latencies.last().copied()),
            ];
            for (label, bound_ms, actual_ns) in checks {
                if let (Some(bound_ms), Some(actual_ns)) = (bound_ms, actual_ns) {
                    let actual_ms = actual_ns as f64 / 1_000_000.0;
                    if actual_ms >= bound_ms {
                        failures.push(format!("{label} {actual_ms:.1}ms >= {bound_ms}ms"));
                    }
                }
            }
            result(
                description,
                failures.is_empty(),
                (!failures.is_empty()).then(|| failures.join("; ")),
            )
        }
        Assertion::ResponseMatches {
            tool,
            method,
            pointer,
            equals,
            contains,
        } => {
            let description = format!("response_matches {pointer}");
            let matching: Vec<&Exchange> = exchanges
                .iter()
                .filter(|e| in_scope(e, tool, method))
                .filter(|e| e.response_seq.is_some())
                .collect();
            if matching.is_empty() {
                return result(
                    description,
                    false,
                    Some("no responded exchanges matched scope".to_string()),
                );
            }
            for exchange in matching {
                let payload = exchange.response_seq.and_then(|seq| payloads.get(&seq));
                let Some(value) = payload.and_then(|p| p.pointer(pointer)) else {
                    return result(
                        description,
                        false,
                        Some(format!(
                            "pointer {pointer} not found in response of {}",
                            describe_exchange(exchange)
                        )),
                    );
                };
                if matches!(value, Value::String(s) if s == REDACTED_PLACEHOLDER) {
                    return result(
                        description,
                        false,
                        Some(format!(
                            "cannot assert on redacted field {pointer}; record with a policy that keeps it"
                        )),
                    );
                }
                if let Some(expected) = equals {
                    let expected_json = toml_to_json(expected);
                    if *value != expected_json {
                        return result(
                            description,
                            false,
                            Some(format!("expected {expected_json}, found {value}")),
                        );
                    }
                }
                if let Some(needle) = contains {
                    let haystack = match value {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    if !haystack.contains(needle.as_str()) {
                        return result(
                            description,
                            false,
                            Some(format!("value {haystack:?} does not contain {needle:?}")),
                        );
                    }
                }
            }
            result(description, true, None)
        }
        Assertion::ToolsPinned { hashes } => {
            let description = "tools_pinned".to_string();
            if hashes.is_empty() {
                if tools.is_empty() {
                    return result(
                        description,
                        false,
                        Some("no tools/list response found in session; nothing to pin".to_string()),
                    );
                }
                let discovered: Vec<String> = tools
                    .iter()
                    .map(|(name, def)| format!("{name} = \"{}\"", tool_hash(name, def)))
                    .collect();
                return result(
                    description,
                    false,
                    Some(format!(
                        "no pinned hashes provided; current tool hashes: {}",
                        discovered.join(", ")
                    )),
                );
            }
            let mut failures = Vec::new();
            for (name, expected_hash) in hashes {
                match tools.get(name) {
                    None => failures.push(format!(
                        "tool `{name}` not found in session (no tools/list, or tool no longer offered)"
                    )),
                    Some(def) => {
                        let actual_hash = tool_hash(name, def);
                        if &actual_hash != expected_hash {
                            failures.push(format!(
                                "tool `{name}` hash mismatch: expected {expected_hash}, got {actual_hash}"
                            ));
                        }
                    }
                }
            }
            result(
                description,
                failures.is_empty(),
                (!failures.is_empty()).then(|| failures.join("; ")),
            )
        }
        Assertion::Quota {
            preset,
            refill_rate,
            capacity,
            max_violations,
            max_burst_factor,
            max_peak_tokens_per_sec,
        } => {
            // parse_spec() already rejected any spec that reaches here without
            // either explicit bounds or a resolvable preset; the branches
            // below cover those two guaranteed-valid cases only.
            let (r, c) = if let (Some(r), Some(c)) = (*refill_rate, *capacity) {
                (r, c)
            } else if let Some(p) = preset
                .as_deref()
                .and_then(crate::quota::ProviderPreset::from_name)
            {
                (p.refill_rate, p.capacity)
            } else {
                unreachable!("parse_spec validates preset/bounds before evaluation")
            };
            let sim = crate::quota::simulate_session(messages, r, c);
            let max_v = max_violations.unwrap_or(0);
            let mut failures = Vec::new();
            if sim.total_violations > max_v {
                failures.push(format!(
                    "{} violations exceeded max allowable {}",
                    sim.total_violations, max_v
                ));
            }
            if let Some(limit_b) = max_burst_factor {
                if sim.burst_factor > *limit_b {
                    failures.push(format!(
                        "burst factor {:.2}x exceeded max limit {:.2}x",
                        sim.burst_factor, limit_b
                    ));
                }
            }
            if let Some(limit_peak) = max_peak_tokens_per_sec {
                if sim.peak_demand_per_sec > *limit_peak {
                    failures.push(format!(
                        "peak demand {} tok/s exceeded limit {} tok/s",
                        sim.peak_demand_per_sec, limit_peak
                    ));
                }
            }
            let desc = format!("quota (rate={:.0} tok/s, cap={:.0} tok)", r, c);
            result(
                desc,
                failures.is_empty(),
                (!failures.is_empty()).then(|| failures.join("; ")),
            )
        }
    }
}

fn check_bounds(
    description: String,
    count: usize,
    min: Option<usize>,
    max: Option<usize>,
) -> AssertionResult {
    let min = min.unwrap_or(1);
    if count < min {
        return result(
            description,
            false,
            Some(format!("occurred {count} time(s), expected at least {min}")),
        );
    }
    if let Some(max) = max {
        if count > max {
            return result(
                description,
                false,
                Some(format!("occurred {count} time(s), expected at most {max}")),
            );
        }
    }
    result(description, true, None)
}

fn result(description: String, passed: bool, reason: Option<String>) -> AssertionResult {
    AssertionResult {
        description,
        passed,
        reason,
    }
}

fn describe_exchange(exchange: &Exchange) -> String {
    match (&exchange.method, &exchange.tool_name) {
        (Some(method), Some(tool)) => format!("{method} {tool}"),
        (Some(method), None) => method.clone(),
        _ => "<orphan response>".to_string(),
    }
}

pub(crate) fn toml_to_json(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => Value::from(*f),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(dt) => Value::String(dt.to_string()),
        toml::Value::Array(items) => Value::Array(items.iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => Value::Object(
            table
                .iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        is_error: bool,
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
            is_error,
            error_code: if is_error { Some(-32000) } else { None },
        }
    }

    fn fixture() -> Vec<StoredMessage> {
        vec![
            msg(
                0,
                0,
                "c2s",
                "request",
                Some("1"),
                Some("initialize"),
                None,
                json!({"id":1,"method":"initialize"}),
                false,
            ),
            msg(
                1,
                2_000_000,
                "s2c",
                "response",
                Some("1"),
                None,
                None,
                json!({"id":1,"result":{}}),
                false,
            ),
            msg(
                2,
                3_000_000,
                "c2s",
                "request",
                Some("2"),
                Some("tools/call"),
                Some("echo"),
                json!({"id":2,"method":"tools/call","params":{"name":"echo"}}),
                false,
            ),
            msg(
                3,
                4_000_000,
                "s2c",
                "response",
                Some("2"),
                None,
                None,
                json!({"id":2,"result":{"isError":false,"content":[{"type":"text","text":"Echo: hi"}]}}),
                false,
            ),
        ]
    }

    fn eval_one(toml_src: &str, messages: &[StoredMessage]) -> AssertionResult {
        let spec = parse_spec(toml_src).expect("spec parses");
        let mut results = evaluate(&spec, messages);
        assert_eq!(results.len(), 1);
        results.remove(0)
    }

    #[test]
    fn no_errors_passes_and_fails() {
        let ok = eval_one("[[assert]]\nkind = \"no_errors\"\n", &fixture());
        assert!(ok.passed, "{ok:?}");

        let mut bad = fixture();
        bad[3] = msg(
            3,
            4_000_000,
            "s2c",
            "response",
            Some("2"),
            None,
            None,
            json!({"id":2,"error":{"code":-32000,"message":"x"}}),
            true,
        );
        let fail = eval_one("[[assert]]\nkind = \"no_errors\"\n", &bad);
        assert!(!fail.passed);
        assert!(fail.reason.unwrap().contains("tools/call echo"));
    }

    #[test]
    fn no_errors_fails_for_tool_execution_error() {
        let mut bad = fixture();
        bad[3] = msg(
            3,
            4_000_000,
            "s2c",
            "response",
            Some("2"),
            None,
            None,
            json!({"id":2,"result":{"isError":true,"content":[{"type":"text","text":"failed"}]}}),
            false,
        );
        let fail = eval_one("[[assert]]\nkind = \"no_errors\"\n", &bad);
        assert!(!fail.passed, "{fail:?}");
        assert!(fail.reason.unwrap().contains("tools/call echo"));
    }

    #[test]
    fn tool_called_bounds() {
        let ok = eval_one(
            "[[assert]]\nkind = \"tool_called\"\ntool = \"echo\"\nmin = 1\nmax = 1\n",
            &fixture(),
        );
        assert!(ok.passed);

        let fail = eval_one(
            "[[assert]]\nkind = \"tool_called\"\ntool = \"missing\"\n",
            &fixture(),
        );
        assert!(!fail.passed);
    }

    #[test]
    fn method_called_counts() {
        let ok = eval_one(
            "[[assert]]\nkind = \"method_called\"\nmethod = \"initialize\"\nmax = 1\n",
            &fixture(),
        );
        assert!(ok.passed, "{ok:?}");
    }

    #[test]
    fn call_order_pass_and_fail() {
        let ok = eval_one(
            "[[assert]]\nkind = \"call_order\"\nbefore = \"initialize\"\nafter = \"tools/call\"\n",
            &fixture(),
        );
        assert!(ok.passed, "{ok:?}");

        let fail = eval_one(
            "[[assert]]\nkind = \"call_order\"\nbefore = \"tools/call\"\nafter = \"initialize\"\n",
            &fixture(),
        );
        assert!(!fail.passed);
    }

    #[test]
    fn latency_bounds() {
        // Latencies: initialize 2ms, echo 1ms.
        let ok = eval_one(
            "[[assert]]\nkind = \"latency\"\np95_under_ms = 10.0\n",
            &fixture(),
        );
        assert!(ok.passed, "{ok:?}");

        let fail = eval_one(
            "[[assert]]\nkind = \"latency\"\ntool = \"echo\"\nmax_under_ms = 0.5\n",
            &fixture(),
        );
        assert!(!fail.passed, "{fail:?}");

        let no_scope = eval_one(
            "[[assert]]\nkind = \"latency\"\ntool = \"missing\"\nmax_under_ms = 1.0\n",
            &fixture(),
        );
        assert!(!no_scope.passed);
        assert!(no_scope.reason.unwrap().contains("no answered exchanges"));
    }

    #[test]
    fn response_matches_equals_contains_and_redacted() {
        let ok = eval_one(
            "[[assert]]\nkind = \"response_matches\"\ntool = \"echo\"\npointer = \"/result/isError\"\nequals = false\n",
            &fixture(),
        );
        assert!(ok.passed, "{ok:?}");

        let contains = eval_one(
            "[[assert]]\nkind = \"response_matches\"\ntool = \"echo\"\npointer = \"/result/content/0/text\"\ncontains = \"Echo\"\n",
            &fixture(),
        );
        assert!(contains.passed, "{contains:?}");

        let wrong = eval_one(
            "[[assert]]\nkind = \"response_matches\"\ntool = \"echo\"\npointer = \"/result/isError\"\nequals = true\n",
            &fixture(),
        );
        assert!(!wrong.passed);

        let mut redacted = fixture();
        redacted[3] = msg(
            3,
            4_000_000,
            "s2c",
            "response",
            Some("2"),
            None,
            None,
            json!({"id":2,"result":{"isError":false,"content":[{"type":"text","text":"***REDACTED***"}]}}),
            false,
        );
        let blocked = eval_one(
            "[[assert]]\nkind = \"response_matches\"\ntool = \"echo\"\npointer = \"/result/content/0/text\"\ncontains = \"Echo\"\n",
            &redacted,
        );
        assert!(!blocked.passed);
        assert!(blocked.reason.unwrap().contains("redacted"));
    }

    /// `initialize`, `tools/list` listing one `echo` tool, then `tools/call echo`.
    fn fixture_with_tools() -> Vec<StoredMessage> {
        vec![
            msg(
                0,
                0,
                "c2s",
                "request",
                Some("1"),
                Some("initialize"),
                None,
                json!({"id":1,"method":"initialize"}),
                false,
            ),
            msg(
                1,
                1_000_000,
                "s2c",
                "response",
                Some("1"),
                None,
                None,
                json!({"id":1,"result":{}}),
                false,
            ),
            msg(
                2,
                2_000_000,
                "c2s",
                "request",
                Some("9"),
                Some("tools/list"),
                None,
                json!({"id":9,"method":"tools/list"}),
                false,
            ),
            msg(
                3,
                2_500_000,
                "s2c",
                "response",
                Some("9"),
                None,
                None,
                json!({"id":9,"result":{"tools":[{"name":"echo","description":"Echo back the input","inputSchema":{"type":"object"}}]}}),
                false,
            ),
            msg(
                4,
                3_000_000,
                "c2s",
                "request",
                Some("2"),
                Some("tools/call"),
                Some("echo"),
                json!({"id":2,"method":"tools/call","params":{"name":"echo"}}),
                false,
            ),
            msg(
                5,
                4_000_000,
                "s2c",
                "response",
                Some("2"),
                None,
                None,
                json!({"id":2,"result":{"isError":false,"content":[{"type":"text","text":"Echo: hi"}]}}),
                false,
            ),
        ]
    }

    fn echo_tool_def() -> ToolDef {
        ToolDef {
            title: None,
            description: Some("Echo back the input".to_string()),
            input_schema: Some(json!({"type":"object"})),
            output_schema: None,
            annotations: None,
        }
    }

    #[test]
    fn tools_pinned_passes_when_hash_matches() {
        let expected = tool_hash("echo", &echo_tool_def());
        let toml_src =
            format!("[[assert]]\nkind = \"tools_pinned\"\nhashes = {{ echo = \"{expected}\" }}\n");
        let ok = eval_one(&toml_src, &fixture_with_tools());
        assert!(ok.passed, "{ok:?}");
    }

    #[test]
    fn tools_pinned_fails_on_hash_mismatch() {
        let toml_src =
            "[[assert]]\nkind = \"tools_pinned\"\nhashes = { echo = \"not-the-real-hash\" }\n";
        let fail = eval_one(toml_src, &fixture_with_tools());
        assert!(!fail.passed);
        assert!(fail.reason.unwrap().contains("hash mismatch"));
    }

    #[test]
    fn tools_pinned_fails_when_pinned_tool_is_missing() {
        let toml_src = "[[assert]]\nkind = \"tools_pinned\"\nhashes = { ghost = \"whatever\" }\n";
        let fail = eval_one(toml_src, &fixture_with_tools());
        assert!(!fail.passed);
        assert!(fail.reason.unwrap().contains("not found"));
    }

    #[test]
    fn tools_pinned_empty_hashes_reports_discovered_hashes_to_bootstrap() {
        let expected = tool_hash("echo", &echo_tool_def());
        let toml_src = "[[assert]]\nkind = \"tools_pinned\"\n";
        let fail = eval_one(toml_src, &fixture_with_tools());
        assert!(!fail.passed);
        let reason = fail.reason.unwrap();
        assert!(reason.contains("echo"));
        assert!(reason.contains(&expected));
    }

    #[test]
    fn tools_pinned_empty_hashes_and_no_tools_list_fails_clearly() {
        let toml_src = "[[assert]]\nkind = \"tools_pinned\"\n";
        let fail = eval_one(toml_src, &fixture());
        assert!(!fail.passed);
        assert!(fail.reason.unwrap().contains("no tools/list"));
    }

    #[test]
    fn assertions_alias_and_description_label() {
        // `[[assertions]]` (README style) parses the same as `[[assert]]`,
        // and a `description` overrides the auto-generated label.
        let toml_src = "\n".to_string()
            + "[[assertions]]\n"
            + "kind = \"no_errors\"\n"
            + "description = \"session contains no JSON-RPC errors\"\n";
        let spec = parse_spec(&toml_src).expect("alias spec parses");
        let results = evaluate(&spec, &fixture());
        assert_eq!(results.len(), 1);
        assert!(results[0].passed);
        assert_eq!(
            results[0].description,
            "session contains no JSON-RPC errors"
        );
    }

    #[test]
    fn spec_errors_are_distinguished() {
        assert!(parse_spec("not toml at [[").is_err());
        assert!(parse_spec("[[assert]]\nkind = \"bogus_kind\"\n").is_err());
        assert!(parse_spec("").is_err(), "empty spec is a spec error");
        assert!(
            parse_spec("[[assert]]\nkind = \"latency\"\n").is_err(),
            "latency without bounds is a spec error"
        );
        assert!(
            parse_spec("[[assert]]\nkind = \"response_matches\"\npointer = \"/x\"\n").is_err(),
            "response_matches without equals/contains is a spec error"
        );
        assert!(
            parse_spec("[[assert]]\nkind = \"latency\"\np95_under_ms = -1\n").is_err(),
            "negative latency bounds are spec errors"
        );
        assert!(
            parse_spec("[[assert]]\nkind = \"quota\"\npreset = \"not-a-real-preset\"\n").is_err(),
            "an unknown quota preset is a spec error, not a silent default"
        );
        assert!(
            parse_spec("[[assert]]\nkind = \"quota\"\n").is_err(),
            "quota without a preset or explicit refill_rate/capacity is a spec error"
        );
    }

    #[test]
    fn quota_assertions_pass_and_fail() {
        let toml_pass =
            "[[assert]]\nkind = \"quota\"\npreset = \"openai-tier2\"\nmax_violations = 0\n";
        let pass = eval_one(toml_pass, &fixture());
        assert!(pass.passed);

        // Low capacity (10 tokens) -> fails on fixture
        let toml_fail = "[[assert]]\nkind = \"quota\"\nrefill_rate = 1.0\ncapacity = 10.0\nmax_violations = 0\n";
        let fail = eval_one(toml_fail, &fixture());
        assert!(!fail.passed);
        assert!(fail.reason.unwrap().contains("violations exceeded"));

        // Explicit refill_rate/capacity are honored even with an unrecognized
        // preset name alongside them (preset is only consulted when bounds
        // are absent) -- this is intentional, unlike quota-with-no-bounds.
        let toml_explicit_wins = "[[assert]]\nkind = \"quota\"\npreset = \"not-a-real-preset\"\nrefill_rate = 1.0\ncapacity = 10.0\nmax_violations = 0\n";
        assert!(parse_spec(toml_explicit_wins).is_ok());
    }
}
