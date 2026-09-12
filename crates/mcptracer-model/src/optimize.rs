//! Local-history suggestion engine.
//!
//! Mines already-recorded sessions for patterns worth turning into config —
//! latency thresholds, volatile response pointers, recurring-failure
//! assertions, a golden-session candidate, and `bench` parameters. Every
//! suggestion is a proposal, never a mutation: nothing here writes files or
//! calls a model. See `docs/architecture/overview.md`.

use std::collections::BTreeMap;

use mcptracer_protocol::Direction;
use mcptracer_storage::StoredMessage;
use serde::Serialize;
use serde_json::Value;

use crate::diff::{compare_values, payloads_by_seq};
use crate::{correlate, percentile, ExchangeStatus};

/// One session's identity and raw messages, as input to suggestion mining.
/// `healthy` should come from `validate_session` — unhealthy sessions are
/// still mined for latency/failure patterns but excluded from golden-session
/// candidacy.
#[derive(Debug, Clone, Copy)]
pub struct SessionHistory<'a> {
    pub session_id: &'a str,
    pub healthy: bool,
    pub messages: &'a [StoredMessage],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionKind {
    LatencyThreshold,
    IgnorePointer,
    AssertionTemplate,
    GoldenSession,
    BenchParams,
}

/// A proposed change, never applied automatically. `confidence` is a rough
/// 0.0-1.0 signal from sample size, not a statistical guarantee.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Suggestion {
    pub kind: SuggestionKind,
    pub description: String,
    pub command: String,
    pub confidence: f64,
    pub provenance: Vec<String>,
}

/// Mine every category of suggestion from a set of session histories. Order
/// is stable and does not depend on iteration order of any internal map.
pub fn suggest(histories: &[SessionHistory<'_>]) -> Vec<Suggestion> {
    let mut suggestions = Vec::new();
    suggestions.extend(latency_threshold_suggestions(histories));
    suggestions.extend(ignore_pointer_suggestions(histories));
    suggestions.extend(assertion_template_suggestions(histories));
    suggestions.extend(golden_session_suggestion(histories));
    suggestions.extend(bench_params_suggestion(histories));
    suggestions
}

/// Per-tool p95 latency samples pooled across every session.
fn latency_threshold_suggestions(histories: &[SessionHistory<'_>]) -> Vec<Suggestion> {
    let mut samples: BTreeMap<String, Vec<(i64, String)>> = BTreeMap::new();
    for history in histories {
        let model = correlate(history.messages);
        for exchange in &model.exchanges {
            let (Some(tool), Some(latency_ns)) = (&exchange.tool_name, exchange.latency_ns) else {
                continue;
            };
            if exchange.status != ExchangeStatus::Ok {
                continue;
            }
            samples
                .entry(tool.clone())
                .or_default()
                .push((latency_ns, history.session_id.to_string()));
        }
    }

    const MIN_SAMPLES: usize = 3;
    let mut suggestions = Vec::new();
    for (tool, mut entries) in samples {
        if entries.len() < MIN_SAMPLES {
            continue;
        }
        entries.sort_by_key(|(latency_ns, _)| *latency_ns);
        let latencies: Vec<i64> = entries.iter().map(|(latency_ns, _)| *latency_ns).collect();
        let Some(p95_ns) = percentile(&latencies, 0.95) else {
            continue;
        };
        // 25% margin over the observed p95, rounded to the nearest 10ms so
        // the suggested threshold reads as an intentional number.
        let suggested_ms = ((p95_ns as f64 / 1_000_000.0) * 1.25 / 10.0).ceil() * 10.0;
        let mut sessions: Vec<String> = entries.into_iter().map(|(_, session)| session).collect();
        sessions.sort();
        sessions.dedup();

        suggestions.push(Suggestion {
            kind: SuggestionKind::LatencyThreshold,
            description: format!(
                "tool `{tool}`: observed p95 {:.1}ms over {} call(s); suggest a latency bound with headroom",
                p95_ns as f64 / 1_000_000.0,
                latencies.len()
            ),
            command: format!(
                "[[assert]]\nkind = \"latency\"\ntool = \"{tool}\"\np95_under_ms = {suggested_ms:.0}"
            ),
            confidence: (latencies.len() as f64 / 10.0).min(1.0),
            provenance: sessions,
        });
    }
    suggestions
}

/// Per-tool response pointers whose value varies across recorded calls,
/// compared against the first observed sample as a baseline.
fn ignore_pointer_suggestions(histories: &[SessionHistory<'_>]) -> Vec<Suggestion> {
    let mut samples: BTreeMap<String, Vec<(Value, String)>> = BTreeMap::new();
    for history in histories {
        let model = correlate(history.messages);
        let payloads = payloads_by_seq(history.messages);
        for exchange in &model.exchanges {
            if exchange.origin != Direction::ClientToServer || exchange.status != ExchangeStatus::Ok
            {
                continue;
            }
            let Some(tool) = &exchange.tool_name else {
                continue;
            };
            let Some(result) = exchange
                .response_seq
                .and_then(|seq| payloads.get(&seq))
                .and_then(|payload| payload.get("result"))
            else {
                continue;
            };
            samples
                .entry(tool.clone())
                .or_default()
                .push((result.clone(), history.session_id.to_string()));
        }
    }

    const MIN_SAMPLES: usize = 2;
    let mut suggestions = Vec::new();
    for (tool, entries) in samples {
        if entries.len() < MIN_SAMPLES {
            continue;
        }
        let (baseline, _) = &entries[0];
        let mut varying_pointers = Vec::new();
        let mut sessions = Vec::new();
        for (sample, session) in &entries[1..] {
            let mut diffs = Vec::new();
            compare_values(baseline, sample, String::new(), &mut diffs);
            if !diffs.is_empty() {
                sessions.push(session.clone());
            }
            for diff in diffs {
                if !varying_pointers.contains(&diff.pointer) {
                    varying_pointers.push(diff.pointer);
                }
            }
        }
        if varying_pointers.is_empty() {
            continue;
        }
        varying_pointers.sort();
        sessions.sort();
        sessions.dedup();

        suggestions.push(Suggestion {
            kind: SuggestionKind::IgnorePointer,
            description: format!(
                "tool `{tool}`: response varies at {} pointer(s) across {} call(s) with otherwise stable structure",
                varying_pointers.len(),
                entries.len()
            ),
            command: format!(
                "mcptracer diff <a> <b> --ignore {}",
                varying_pointers.join(",")
            ),
            confidence: (sessions.len() as f64 / entries.len() as f64).min(1.0),
            provenance: sessions,
        });
    }
    suggestions
}

/// A tool that errored in two or more distinct sessions is a recurring
/// failure worth a standing `no_errors` gate, not just a one-off.
fn assertion_template_suggestions(histories: &[SessionHistory<'_>]) -> Vec<Suggestion> {
    let mut error_sessions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for history in histories {
        let model = correlate(history.messages);
        for exchange in &model.exchanges {
            let Some(tool) = &exchange.tool_name else {
                continue;
            };
            if exchange.status != ExchangeStatus::Error {
                continue;
            }
            let sessions = error_sessions.entry(tool.clone()).or_default();
            if !sessions.contains(&history.session_id.to_string()) {
                sessions.push(history.session_id.to_string());
            }
        }
    }

    const MIN_DISTINCT_SESSIONS: usize = 2;
    let mut suggestions = Vec::new();
    for (tool, mut sessions) in error_sessions {
        if sessions.len() < MIN_DISTINCT_SESSIONS {
            continue;
        }
        sessions.sort();
        suggestions.push(Suggestion {
            kind: SuggestionKind::AssertionTemplate,
            description: format!(
                "tool `{tool}` errored in {} distinct session(s); recurring failure, not a one-off",
                sessions.len()
            ),
            command: format!("[[assert]]\nkind = \"no_errors\"\ntool = \"{tool}\""),
            confidence: (sessions.len() as f64 / histories.len().max(1) as f64).min(1.0),
            provenance: sessions,
        });
    }
    suggestions
}

/// The healthiest, most complete, error-free session is the best golden
/// baseline for `assert --golden` and `diff`.
fn golden_session_suggestion(histories: &[SessionHistory<'_>]) -> Option<Suggestion> {
    let mut candidates: Vec<(&SessionHistory<'_>, usize)> = histories
        .iter()
        .filter_map(|history| {
            if !history.healthy {
                return None;
            }
            let model = correlate(history.messages);
            if model.stats.errors > 0 || model.stats.total_exchanges == 0 {
                return None;
            }
            Some((history, model.stats.total_exchanges))
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.session_id.cmp(b.0.session_id))
    });
    let (best, exchange_count) = candidates[0];

    Some(Suggestion {
        kind: SuggestionKind::GoldenSession,
        description: format!(
            "session {} is healthy, error-free, and has the most coverage ({exchange_count} exchange(s)) among {} candidate(s)",
            best.session_id,
            candidates.len()
        ),
        command: format!("mcptracer assert <candidate> --golden {}", best.session_id),
        confidence: (candidates.len() as f64 / histories.len().max(1) as f64).min(1.0),
        provenance: vec![best.session_id.to_string()],
    })
}

/// `--repeat`/`--concurrency` scaled off historical call volume: enough
/// repeats to get a stable percentile, a fixed modest concurrency since
/// there is no real load signal to size it from.
fn bench_params_suggestion(histories: &[SessionHistory<'_>]) -> Option<Suggestion> {
    let mut total_exchanges = 0usize;
    let mut sessions = Vec::new();
    for history in histories {
        let model = correlate(history.messages);
        if model.stats.total_exchanges == 0 {
            continue;
        }
        total_exchanges += model.stats.total_exchanges;
        sessions.push(history.session_id.to_string());
    }
    if sessions.is_empty() {
        return None;
    }
    let average = total_exchanges as f64 / sessions.len() as f64;
    let repeat = ((average * 2.0).round() as usize).max(20);
    let concurrency = 4;

    Some(Suggestion {
        kind: SuggestionKind::BenchParams,
        description: format!(
            "{} historical session(s) average {average:.1} exchange(s) each",
            sessions.len()
        ),
        command: format!("mcptracer bench <session-id> --repeat {repeat} --concurrency {concurrency} -- <server-command>"),
        confidence: (sessions.len() as f64 / 10.0).min(1.0),
        provenance: sessions,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn msg(seq: u64, ts_ns: i64, dir: Direction, payload: Value, is_error: bool) -> StoredMessage {
        StoredMessage {
            seq,
            ts_ns,
            direction: match dir {
                Direction::ClientToServer => "c2s".to_string(),
                Direction::ServerToClient => "s2c".to_string(),
            },
            message_kind: if payload.get("method").is_some() {
                "request".to_string()
            } else {
                "response".to_string()
            },
            rpc_id: payload
                .get("id")
                .map(|id| id.to_string())
                .or_else(|| Some("1".to_string())),
            method: payload
                .get("method")
                .and_then(Value::as_str)
                .map(String::from),
            tool_name: payload
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .map(String::from),
            payload: payload.to_string(),
            payload_bytes: 0,
            is_error,
            error_code: if is_error { Some(-32000) } else { None },
        }
    }

    /// A tools/call echo request/response pair, `latency_ms` apart, with the
    /// given result body (or a JSON-RPC error when `error` is set).
    fn call(seq: u64, latency_ms: i64, result: Value, error: bool) -> Vec<StoredMessage> {
        let request =
            json!({"jsonrpc":"2.0","id":seq,"method":"tools/call","params":{"name":"echo"}});
        let response = if error {
            json!({"jsonrpc":"2.0","id":seq,"error":{"code":-32000,"message":"boom"}})
        } else {
            json!({"jsonrpc":"2.0","id":seq,"result":result})
        };
        vec![
            msg(seq * 2, 0, Direction::ClientToServer, request, false),
            msg(
                seq * 2 + 1,
                latency_ms * 1_000_000,
                Direction::ServerToClient,
                response,
                error,
            ),
        ]
    }

    #[test]
    fn latency_threshold_suggested_from_enough_samples() {
        let session_messages: Vec<StoredMessage> = (0..5)
            .flat_map(|i| call(i, 100 + i as i64 * 10, json!({"ok": true}), false))
            .collect();
        let histories = vec![SessionHistory {
            session_id: "s1",
            healthy: true,
            messages: &session_messages,
        }];

        let suggestions = suggest(&histories);
        let latency = suggestions
            .iter()
            .find(|s| s.kind == SuggestionKind::LatencyThreshold)
            .expect("expected a latency suggestion");
        assert!(latency.command.contains("kind = \"latency\""));
        assert!(latency.confidence > 0.0);
    }

    #[test]
    fn too_few_samples_suppresses_latency_suggestion() {
        let session_messages: Vec<StoredMessage> = (0..2)
            .flat_map(|i| call(i, 100, json!({"ok": true}), false))
            .collect();
        let histories = vec![SessionHistory {
            session_id: "s1",
            healthy: true,
            messages: &session_messages,
        }];

        let suggestions = suggest(&histories);
        assert!(!suggestions
            .iter()
            .any(|s| s.kind == SuggestionKind::LatencyThreshold));
    }

    #[test]
    fn varying_response_field_suggests_ignore_pointer() {
        let mut messages = call(0, 10, json!({"text": "hi", "requestId": "aaa"}), false);
        messages.extend(call(
            1,
            10,
            json!({"text": "hi", "requestId": "bbb"}),
            false,
        ));
        let histories = vec![SessionHistory {
            session_id: "s1",
            healthy: true,
            messages: &messages,
        }];

        let suggestions = suggest(&histories);
        let ignore = suggestions
            .iter()
            .find(|s| s.kind == SuggestionKind::IgnorePointer)
            .expect("expected an ignore-pointer suggestion");
        assert!(ignore.command.contains("/requestId"));
        assert!(!ignore.command.contains("/text"));
    }

    #[test]
    fn recurring_error_across_sessions_suggests_assertion_template() {
        let first = call(0, 10, json!({}), true);
        let second = call(0, 10, json!({}), true);
        let histories = vec![
            SessionHistory {
                session_id: "s1",
                healthy: true,
                messages: &first,
            },
            SessionHistory {
                session_id: "s2",
                healthy: true,
                messages: &second,
            },
        ];

        let suggestions = suggest(&histories);
        let template = suggestions
            .iter()
            .find(|s| s.kind == SuggestionKind::AssertionTemplate)
            .expect("expected an assertion-template suggestion");
        assert_eq!(
            template.provenance,
            vec!["s1".to_string(), "s2".to_string()]
        );
    }

    #[test]
    fn single_session_error_is_not_recurring() {
        let messages = call(0, 10, json!({}), true);
        let histories = vec![SessionHistory {
            session_id: "s1",
            healthy: true,
            messages: &messages,
        }];

        let suggestions = suggest(&histories);
        assert!(!suggestions
            .iter()
            .any(|s| s.kind == SuggestionKind::AssertionTemplate));
    }

    #[test]
    fn healthiest_most_complete_session_is_golden_candidate() {
        let small = call(0, 10, json!({}), false);
        let mut large = call(0, 10, json!({}), false);
        large.extend(call(1, 10, json!({}), false));
        let mut unhealthy_but_bigger = call(0, 10, json!({}), false);
        unhealthy_but_bigger.extend(call(1, 10, json!({}), false));

        let histories = vec![
            SessionHistory {
                session_id: "small",
                healthy: true,
                messages: &small,
            },
            SessionHistory {
                session_id: "large",
                healthy: true,
                messages: &large,
            },
            SessionHistory {
                session_id: "unhealthy",
                healthy: false,
                messages: &unhealthy_but_bigger,
            },
        ];

        let suggestions = suggest(&histories);
        let golden = suggestions
            .iter()
            .find(|s| s.kind == SuggestionKind::GoldenSession)
            .expect("expected a golden-session suggestion");
        assert_eq!(golden.provenance, vec!["large".to_string()]);
    }

    #[test]
    fn no_healthy_error_free_session_suppresses_golden_suggestion() {
        let errored = call(0, 10, json!({}), true);
        let histories = vec![SessionHistory {
            session_id: "s1",
            healthy: true,
            messages: &errored,
        }];

        let suggestions = suggest(&histories);
        assert!(!suggestions
            .iter()
            .any(|s| s.kind == SuggestionKind::GoldenSession));
    }

    #[test]
    fn bench_params_scale_with_historical_call_volume() {
        let messages: Vec<StoredMessage> = (0..30)
            .flat_map(|i| call(i, 10, json!({}), false))
            .collect();
        let histories = vec![SessionHistory {
            session_id: "s1",
            healthy: true,
            messages: &messages,
        }];

        let suggestions = suggest(&histories);
        let bench = suggestions
            .iter()
            .find(|s| s.kind == SuggestionKind::BenchParams)
            .expect("expected a bench-params suggestion");
        assert!(bench.command.contains("--repeat 60"));
        assert!(bench.command.contains("--concurrency 4"));
    }

    #[test]
    fn empty_history_suggests_nothing() {
        assert!(suggest(&[]).is_empty());
    }
}
