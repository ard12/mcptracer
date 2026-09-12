//! Token-bucket rate limiting and P(429 | R, C) quota simulation for MCP sessions.
//!
//! Evaluates recorded MCP tool exchanges and sampling messages against cloud provider
//! rate limits (TPM / RPM) to verify deployment viability at $0 inference cost.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use mcptracer_storage::StoredMessage;

/// Cloud provider rate tier preset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderPreset {
    pub name: String,
    pub description: String,
    pub refill_rate: f64,  // tokens / second
    pub capacity: f64,     // max burst capacity tokens
    pub req_rate: f64,     // requests / second
    pub req_capacity: f64, // max burst capacity requests
}

impl ProviderPreset {
    /// Look up a provider preset by identifier string.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "openai-tier1" => Some(Self {
                name: "OpenAI Tier 1".into(),
                description: "30k TPM, 500 RPM".into(),
                refill_rate: 500.0,
                capacity: 10_000.0,
                req_rate: 8.33,
                req_capacity: 500.0,
            }),
            "openai-tier2" => Some(Self {
                name: "OpenAI Tier 2".into(),
                description: "120k TPM, 1k RPM".into(),
                refill_rate: 2000.0,
                capacity: 40_000.0,
                req_rate: 16.66,
                req_capacity: 1000.0,
            }),
            "openai-tier3" => Some(Self {
                name: "OpenAI Tier 3".into(),
                description: "480k TPM, 5k RPM".into(),
                refill_rate: 8000.0,
                capacity: 150_000.0,
                req_rate: 83.33,
                req_capacity: 5000.0,
            }),
            "anthropic-tier1" => Some(Self {
                name: "Anthropic Tier 1".into(),
                description: "25k TPM, 50 RPM".into(),
                refill_rate: 416.0,
                capacity: 10_000.0,
                req_rate: 5.0,
                req_capacity: 50.0,
            }),
            "anthropic-tier2" => Some(Self {
                name: "Anthropic Tier 2".into(),
                description: "100k TPM, 1k RPM".into(),
                refill_rate: 1666.0,
                capacity: 40_000.0,
                req_rate: 16.66,
                req_capacity: 1000.0,
            }),
            "groq-llama3" => Some(Self {
                name: "Groq Free/On-Demand".into(),
                description: "60k TPM, 30 RPM".into(),
                refill_rate: 1000.0,
                capacity: 30_000.0,
                req_rate: 0.5,
                req_capacity: 30.0,
            }),
            _ => None,
        }
    }
}

/// Continuous-time token bucket rate limiter.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    pub rate: f64,
    pub capacity: f64,
    pub tokens: f64,
    pub last_refill: Option<f64>,
}

impl TokenBucket {
    /// Create a new token bucket starting at full capacity.
    pub fn new(rate: f64, capacity: f64) -> Self {
        Self {
            rate,
            capacity,
            tokens: capacity,
            last_refill: None,
        }
    }

    /// Advance the bucket's time to `now` (seconds), refilling linearly up to `capacity`.
    pub fn refill(&mut self, now: f64) {
        if let Some(last) = self.last_refill {
            if now > last {
                let elapsed = now - last;
                self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
            }
        }
        self.last_refill = Some(now);
    }

    /// Try to consume `amount` tokens at time `now`. Returns `true` if admitted, `false` if rejected.
    pub fn try_consume(&mut self, amount: f64, now: f64) -> bool {
        self.refill(now);
        if self.tokens >= amount {
            self.tokens -= amount;
            true
        } else {
            false
        }
    }
}

/// Dual-window rate limiter simultaneously tracking TPM (tokens) and RPM (requests).
#[derive(Debug, Clone)]
pub struct DualTokenBucket {
    pub token_bucket: TokenBucket,
    pub req_bucket: TokenBucket,
}

impl DualTokenBucket {
    pub fn new(token_rate: f64, token_capacity: f64, req_rate: f64, req_capacity: f64) -> Self {
        Self {
            token_bucket: TokenBucket::new(token_rate, token_capacity),
            req_bucket: TokenBucket::new(req_rate, req_capacity),
        }
    }

    pub fn try_consume(&mut self, tokens: f64, requests: f64, now_s: f64) -> bool {
        self.token_bucket.refill(now_s);
        self.req_bucket.refill(now_s);
        if tokens > self.token_bucket.tokens || requests > self.req_bucket.tokens {
            return false;
        }
        self.token_bucket.tokens -= tokens;
        self.req_bucket.tokens -= requests;
        true
    }
}

/// Discrete token debit event in a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaEvent {
    pub call_id: String,
    pub ts_s: f64,
    pub tokens: u64,
    pub token_source: TokenSource,
    pub method: Option<String>,
}

/// Provenance for a token count used by quota simulation.
///
/// `Reported` means the MCP payload contained `usage.total_tokens` (directly
/// or under `result`). `EstimatedFromBytes` is MCPTracer's uncalibrated
/// fallback of `payload_bytes / 4`, with a floor of 10. It is not the output
/// of a provider tokenizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenSource {
    Reported,
    EstimatedFromBytes,
}

/// One token count together with the evidence used to obtain it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCount {
    pub tokens: u64,
    pub source: TokenSource,
}

/// Aggregate provenance for token counts contributing to a result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSourceSummary {
    pub reported_events: usize,
    pub reported_tokens: u64,
    pub estimated_events: usize,
    pub estimated_tokens: u64,
}

impl TokenSourceSummary {
    fn record(&mut self, source: TokenSource, tokens: u64) {
        match source {
            TokenSource::Reported => {
                self.reported_events = self.reported_events.saturating_add(1);
                self.reported_tokens = self.reported_tokens.saturating_add(tokens);
            }
            TokenSource::EstimatedFromBytes => {
                self.estimated_events = self.estimated_events.saturating_add(1);
                self.estimated_tokens = self.estimated_tokens.saturating_add(tokens);
            }
        }
    }

    /// Merge another independently-computed summary into this one.
    pub fn merge(&mut self, other: Self) {
        self.reported_events = self.reported_events.saturating_add(other.reported_events);
        self.reported_tokens = self.reported_tokens.saturating_add(other.reported_tokens);
        self.estimated_events = self.estimated_events.saturating_add(other.estimated_events);
        self.estimated_tokens = self.estimated_tokens.saturating_add(other.estimated_tokens);
    }

    /// True when at least one uncalibrated byte-derived estimate contributed.
    pub fn contains_estimates(&self) -> bool {
        self.estimated_events > 0
    }
}

/// Outcome of replaying a session trace against a token bucket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaSimulationResult {
    pub ok: bool,
    pub first_violation_call_id: Option<String>,
    pub first_violation_time_s: Option<f64>,
    pub total_violations: usize,
    pub total_calls: usize,
    pub total_tokens: u64,
    /// Provenance split for `total_tokens` and the simulation derived from it.
    pub token_sources: TokenSourceSummary,
    pub peak_demand_per_sec: u64,
    /// Provenance split for the events in the one-second peak-demand window.
    pub peak_demand_token_sources: TokenSourceSummary,
    pub burst_factor: f64,
}

/// Obtain the token count carried by a stored message, with provenance.
///
/// If exact usage metadata is absent, this uses the deliberately simple and
/// uncalibrated fallback `payload_bytes / 4` with a floor of 10. The estimate
/// is useful for rough burst simulation but is not a tokenizer result.
pub fn message_token_count(msg: &StoredMessage) -> TokenCount {
    // Check if JSON contains a usage object with total_tokens.
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&msg.payload) {
        if let Some(usage) = val.get("usage") {
            if let Some(tot) = usage.get("total_tokens").and_then(|v| v.as_u64()) {
                return TokenCount {
                    tokens: tot,
                    source: TokenSource::Reported,
                };
            }
        }
        if let Some(res) = val.get("result") {
            if let Some(usage) = res.get("usage") {
                if let Some(tot) = usage.get("total_tokens").and_then(|v| v.as_u64()) {
                    return TokenCount {
                        tokens: tot,
                        source: TokenSource::Reported,
                    };
                }
            }
        }
    }
    // Fallback: estimate 1 token per 4 UTF-8 bytes, minimum 10 tokens.
    TokenCount {
        tokens: (msg.payload_bytes as u64 / 4).max(10),
        source: TokenSource::EstimatedFromBytes,
    }
}

/// Extract chronological token debit events from stored messages.
pub fn extract_quota_events(messages: &[StoredMessage]) -> Vec<QuotaEvent> {
    let mut events = Vec::new();
    for msg in messages {
        // Debit on outgoing requests (which carry a method/tool_name) and on
        // incoming responses (which carry neither, but may carry usage tokens).
        if msg.method.is_some() || msg.tool_name.is_some() || msg.message_kind == "response" {
            let ts_s = msg.ts_ns as f64 / 1_000_000_000.0; // convert ns to s
            let token_count = message_token_count(msg);
            events.push(QuotaEvent {
                call_id: msg
                    .rpc_id
                    .clone()
                    .unwrap_or_else(|| format!("msg_{}", msg.seq)),
                ts_s,
                tokens: token_count.tokens,
                token_source: token_count.source,
                method: msg.method.clone().or_else(|| msg.tool_name.clone()),
            });
        }
    }
    events.sort_by(|a, b| {
        a.ts_s
            .partial_cmp(&b.ts_s)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    events
}

/// Simulate a sequence of quota events against a token bucket.
pub fn simulate_events(events: &[QuotaEvent], rate: f64, capacity: f64) -> QuotaSimulationResult {
    let mut bucket = TokenBucket::new(rate, capacity);
    let mut violations = 0;
    let mut first_violation_call_id = None;
    let mut first_violation_time_s = None;
    let mut total_tokens = 0u64;
    let mut token_sources = TokenSourceSummary::default();

    // Bucket into 1-second windows for peak demand and burst factor calculation
    let mut window_demand: BTreeMap<i64, (u64, TokenSourceSummary)> = BTreeMap::new();
    let t0 = events.first().map(|e| e.ts_s).unwrap_or(0.0);

    for e in events {
        total_tokens = total_tokens.saturating_add(e.tokens);
        token_sources.record(e.token_source, e.tokens);
        let window_idx = (e.ts_s - t0).max(0.0) as i64;
        let window = window_demand.entry(window_idx).or_default();
        window.0 = window.0.saturating_add(e.tokens);
        window.1.record(e.token_source, e.tokens);

        if !bucket.try_consume(e.tokens as f64, e.ts_s) {
            violations += 1;
            if first_violation_call_id.is_none() {
                first_violation_call_id = Some(e.call_id.clone());
                first_violation_time_s = Some(e.ts_s);
            }
        }
    }

    let (peak_demand_per_sec, peak_demand_token_sources) = window_demand
        .values()
        .max_by_key(|(tokens, _)| *tokens)
        .copied()
        .unwrap_or_default();
    let burst_factor = if !window_demand.is_empty() {
        let mut vals: Vec<f64> = window_demand
            .values()
            .map(|(tokens, _)| *tokens as f64)
            .collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = vals[vals.len() / 2];
        if median > 0.0 {
            peak_demand_per_sec as f64 / median
        } else {
            let mean = vals.iter().sum::<f64>() / vals.len() as f64;
            if mean > 0.0 {
                peak_demand_per_sec as f64 / mean
            } else {
                0.0
            }
        }
    } else {
        0.0
    };

    QuotaSimulationResult {
        ok: violations == 0,
        first_violation_call_id,
        first_violation_time_s,
        total_violations: violations,
        total_calls: events.len(),
        total_tokens,
        token_sources,
        peak_demand_per_sec,
        peak_demand_token_sources,
        burst_factor,
    }
}

/// Simulate an entire stored session against a token bucket.
pub fn simulate_session(
    messages: &[StoredMessage],
    rate: f64,
    capacity: f64,
) -> QuotaSimulationResult {
    let events = extract_quota_events(messages);
    simulate_events(&events, rate, capacity)
}

/// Estimate the quota violation probability P(429 | R, C) across multiple sessions.
pub fn estimate_p429(sessions: &[Vec<StoredMessage>], rate: f64, capacity: f64) -> f64 {
    if sessions.is_empty() {
        return 0.0;
    }
    let failed = sessions
        .iter()
        .filter(|s| !simulate_session(s, rate, capacity).ok)
        .count();
    failed as f64 / sessions.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_bucket_refill_and_consumption() {
        let mut bucket = TokenBucket::new(100.0, 500.0);
        // Initially at full capacity (500)
        assert!(bucket.try_consume(300.0, 0.0));
        assert_eq!(bucket.tokens, 200.0);

        // Cannot consume 300 when only 200 left
        assert!(!bucket.try_consume(300.0, 0.0));

        // After 1.5 seconds, refills +150 tokens -> 350 total
        assert!(bucket.try_consume(300.0, 1.5));
        assert_eq!(bucket.tokens, 50.0);

        // After 10 seconds, capped at max capacity (500.0)
        bucket.refill(11.5);
        assert_eq!(bucket.tokens, 500.0);
    }

    #[test]
    fn test_simulate_events_clean_and_violating() {
        let events = vec![
            QuotaEvent {
                call_id: "c1".into(),
                ts_s: 0.0,
                tokens: 200,
                token_source: TokenSource::Reported,
                method: None,
            },
            QuotaEvent {
                call_id: "c2".into(),
                ts_s: 0.1,
                tokens: 200,
                token_source: TokenSource::EstimatedFromBytes,
                method: None,
            },
        ];
        // Capacity 500, Rate 100 -> fits
        let res1 = simulate_events(&events, 100.0, 500.0);
        assert!(res1.ok);
        assert_eq!(res1.total_violations, 0);
        assert_eq!(res1.token_sources.reported_tokens, 200);
        assert_eq!(res1.token_sources.estimated_tokens, 200);
        assert_eq!(res1.peak_demand_token_sources, res1.token_sources);

        // Capacity 300, Rate 100 -> c2 fails at t=0.1
        let res2 = simulate_events(&events, 100.0, 300.0);
        assert!(!res2.ok);
        assert_eq!(res2.total_violations, 1);
        assert_eq!(res2.first_violation_call_id.as_deref(), Some("c2"));
    }

    #[test]
    fn test_provider_preset_lookup() {
        let p = ProviderPreset::from_name("openai-tier2").unwrap();
        assert_eq!(p.refill_rate, 2000.0);
        assert_eq!(p.capacity, 40_000.0);
        assert_eq!(p.req_capacity, 1000.0);
        assert!(ProviderPreset::from_name("unknown").is_none());
    }

    #[test]
    fn test_dual_token_bucket() {
        let mut dual = DualTokenBucket::new(1000.0, 5000.0, 1.0, 2.0);
        assert!(dual.try_consume(500.0, 1.0, 0.0));
        assert!(dual.try_consume(500.0, 1.0, 0.0));
        // RPM exhausted (capacity 2.0 reached)
        assert!(!dual.try_consume(10.0, 1.0, 0.0));
        // After 1.0s, refills 1 request
        assert!(dual.try_consume(10.0, 1.0, 1.0));
    }

    fn stored_message(
        seq: u64,
        message_kind: &str,
        method: Option<&str>,
        payload: &str,
    ) -> StoredMessage {
        StoredMessage {
            seq,
            ts_ns: seq as i64 * 1_000_000_000,
            direction: if message_kind == "request" {
                "c2s".into()
            } else {
                "s2c".into()
            },
            message_kind: message_kind.into(),
            rpc_id: Some(format!("req-{seq}")),
            method: method.map(str::to_string),
            tool_name: None,
            payload: payload.into(),
            payload_bytes: payload.len(),
            is_error: false,
            error_code: None,
        }
    }

    #[test]
    fn extract_quota_events_counts_response_usage_tokens() {
        let messages = vec![
            stored_message(
                1,
                "request",
                Some("tools/call"),
                r#"{"method":"tools/call"}"#,
            ),
            stored_message(
                2,
                "response",
                None,
                r#"{"result":{"usage":{"total_tokens":1234}}}"#,
            ),
        ];

        let events = extract_quota_events(&messages);

        assert_eq!(
            events.len(),
            2,
            "response message must produce a debit event"
        );
        let response_event = events
            .iter()
            .find(|e| e.call_id == "req-2")
            .expect("response event present");
        assert_eq!(
            response_event.tokens, 1234,
            "response usage.total_tokens must be honored, not dropped"
        );
        assert_eq!(response_event.token_source, TokenSource::Reported);

        let request_event = events
            .iter()
            .find(|e| e.call_id == "req-1")
            .expect("request event present");
        assert_eq!(
            request_event.token_source,
            TokenSource::EstimatedFromBytes,
            "payloads without usage metadata must be labeled estimated"
        );

        let result = simulate_events(&events, 10_000.0, 10_000.0);
        assert_eq!(result.token_sources.reported_events, 1);
        assert_eq!(result.token_sources.reported_tokens, 1234);
        assert_eq!(result.token_sources.estimated_events, 1);
        assert!(result.token_sources.estimated_tokens > 0);
        assert!(result.token_sources.contains_estimates());
    }

    #[test]
    fn message_token_count_labels_reported_and_estimated_values() {
        let reported = stored_message(1, "response", None, r#"{"usage":{"total_tokens":77}}"#);
        assert_eq!(
            message_token_count(&reported),
            TokenCount {
                tokens: 77,
                source: TokenSource::Reported,
            }
        );

        let estimated = stored_message(2, "response", None, r#"{"result":{"ok":true}}"#);
        let count = message_token_count(&estimated);
        assert_eq!(count.source, TokenSource::EstimatedFromBytes);
        assert_eq!(count.tokens, 10, "the documented minimum is retained");
    }
}
