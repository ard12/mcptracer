use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::Args;
use mcptracer_model::quota::{estimate_p429, simulate_session, ProviderPreset, TokenSourceSummary};
use mcptracer_storage::Store;

const QUOTA_REPORT_SCHEMA_VERSION: u32 = 2;

/// Upper bound on sessions pulled into one `--all` evaluation. This exists to
/// bound memory, not to sample: hitting it makes the fleet P(429) a partial
/// result over a partial denominator, so it is reported rather than absorbed.
const FLEET_SESSION_CAP: usize = 1000;

#[derive(Args)]
pub struct QuotaArgs {
    /// Session id (or unique prefix) to evaluate. Omit or use --all to evaluate all stored sessions.
    pub session_id: Option<String>,

    /// Evaluate all recorded sessions in the database.
    #[arg(long)]
    pub all: bool,

    /// Cloud provider rate tier preset (e.g. openai-tier1, openai-tier2, anthropic-tier2, groq-llama3).
    #[arg(long, default_value = "openai-tier2")]
    pub preset: String,

    /// Custom token bucket refill rate override (tokens / sec).
    #[arg(long)]
    pub rate: Option<f64>,

    /// Custom token bucket burst capacity override (tokens).
    #[arg(long)]
    pub capacity: Option<f64>,

    /// Maximum allowable P(429) failure probability across multi-session evaluations (default: 0.05).
    #[arg(long, default_value_t = 0.05)]
    pub max_p429: f64,

    /// Print structured JSON output instead of human-readable report.
    #[arg(long)]
    pub json: bool,
}

/// Evaluate recorded sessions against a target rate-limit model offline.
///
/// Token counts are reported by the payload when available and otherwise
/// explicitly labeled as estimates derived from payload bytes.
pub async fn run(args: QuotaArgs, db_path: PathBuf) -> Result<()> {
    let store = Store::open(&db_path)?;

    let (preset_name, rate, capacity) = if let (Some(r), Some(c)) = (args.rate, args.capacity) {
        ("custom".to_string(), r, c)
    } else if let Some(p) = ProviderPreset::from_name(&args.preset) {
        (
            format!("{} ({})", p.name, p.description),
            p.refill_rate,
            p.capacity,
        )
    } else {
        bail!(
            "Unknown provider preset '{}'. Valid options: openai-tier1, openai-tier2, openai-tier3, anthropic-tier1, anthropic-tier2, groq-llama3",
            args.preset
        );
    };

    if args.all || args.session_id.is_none() {
        let sessions = store.list_sessions(FLEET_SESSION_CAP)?;
        if sessions.is_empty() {
            bail!(
                "No recorded sessions found in database at {}",
                db_path.display()
            );
        }

        let mut all_session_messages = Vec::new();
        let mut individual_results = Vec::new();

        for s in &sessions {
            if let Ok(msgs) = store.get_messages(&s.id) {
                if !msgs.is_empty() {
                    let sim = simulate_session(&msgs, rate, capacity);
                    individual_results.push((s.id.clone(), sim));
                    all_session_messages.push(msgs);
                }
            }
        }

        if all_session_messages.is_empty() {
            bail!("No messages found in recorded sessions.");
        }

        // `list_sessions` returns at most the cap, so an exact hit is the
        // only signal available that older sessions were left out.
        let truncated = sessions.len() >= FLEET_SESSION_CAP;
        if truncated {
            eprintln!(
                "[mcptracer] WARNING: evaluated only the {FLEET_SESSION_CAP} most recent \
                 session(s); older sessions were not included, so this fleet P(429) is a \
                 partial result and --max-p429 is gating on a partial denominator."
            );
        }

        let p429 = estimate_p429(&all_session_messages, rate, capacity);
        let passed = p429 <= args.max_p429;
        let mut fleet_token_sources = TokenSourceSummary::default();
        for (_, result) in &individual_results {
            fleet_token_sources.merge(result.token_sources);
        }

        if args.json {
            let payload = serde_json::json!({
                "schema_version": QUOTA_REPORT_SCHEMA_VERSION,
                "kind": "fleet",
                "preset": preset_name,
                "refill_rate_tokens_per_second": rate,
                "capacity_tokens": capacity,
                "total_sessions": all_session_messages.len(),
                "sessions_considered": individual_results.len(),
                "truncated": truncated,
                "p429": p429,
                "max_p429_threshold": args.max_p429,
                "ok": passed,
                "token_sources": fleet_token_sources,
                "sessions": individual_results.iter().map(|(id, r)| {
                    serde_json::json!({
                        "session_id": id,
                        "result": r,
                    })
                }).collect::<Vec<_>>()
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
        } else {
            println!("=================================================================");
            println!("      FLEET MCP RATE-LIMIT & QUOTA PRE-FLIGHT EVALUATION         ");
            println!("=================================================================");
            println!("Target Preset    : {}", preset_name);
            println!(
                "Bucket Config    : Refill = {:.0} tok/s | Capacity = {:.0} tok",
                rate, capacity
            );
            println!("Total Sessions   : {}", all_session_messages.len());
            println!(
                "Token Evidence    : {}",
                format_token_sources(fleet_token_sources)
            );
            print_estimate_warning(fleet_token_sources);
            println!("-----------------------------------------------------------------");
            println!(
                "P(429 | R, C)    : {:.2}% (Threshold: <= {:.2}%)",
                p429 * 100.0,
                args.max_p429 * 100.0
            );
            if passed {
                println!(
                    "[PASS] CI FLEET GATE PASSED: All sessions satisfy {} quotas.",
                    preset_name
                );
            } else {
                println!(
                    "[FAIL] CI FLEET GATE FAILED: Quota violation probability {:.2}% exceeds threshold {:.2}%.",
                    p429 * 100.0,
                    args.max_p429 * 100.0
                );
            }
            println!("=================================================================");
        }

        if !passed {
            std::process::exit(1);
        }
        return Ok(());
    }

    let session_id = args.session_id.unwrap();
    let messages = store.get_messages(&session_id)?;

    if messages.is_empty() {
        bail!("Session '{}' has no recorded messages.", session_id);
    }

    let result = simulate_session(&messages, rate, capacity);

    if args.json {
        let payload = serde_json::json!({
            "schema_version": QUOTA_REPORT_SCHEMA_VERSION,
            "kind": "single_session",
            "session_id": session_id,
            "preset": preset_name,
            "refill_rate_tokens_per_second": rate,
            "capacity_tokens": capacity,
            "result": result,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("=================================================================");
        println!("         MCP SESSION RATE-LIMIT & QUOTA PRE-FLIGHT EVALUATION    ");
        println!("=================================================================");
        println!("Session ID       : {}", session_id);
        println!("Target Preset    : {}", preset_name);
        println!(
            "Bucket Config    : Refill = {:.0} tok/s | Capacity = {:.0} tok",
            rate, capacity
        );
        println!("Total Calls      : {}", result.total_calls);
        println!(
            "Total Tokens     : {} ({})",
            result.total_tokens,
            format_token_sources(result.token_sources)
        );
        println!(
            "Peak Demand/s    : {} tok/s ({})",
            result.peak_demand_per_sec,
            format_token_sources(result.peak_demand_token_sources)
        );
        println!("Burst Factor (B) : {:.2}x", result.burst_factor);
        print_estimate_warning(result.token_sources);
        println!("-----------------------------------------------------------------");
        if result.ok {
            println!(
                "[PASS] CI GATE PASSED: Session satisfies {} rate limits with zero 429 violations.",
                preset_name
            );
        } else {
            println!(
                "[FAIL] CI GATE FAILED: Session encountered {} token bucket violations.",
                result.total_violations
            );
            if let Some(ref call_id) = result.first_violation_call_id {
                println!("       First violation at call ID: {}", call_id);
            }
            if let Some(t) = result.first_violation_time_s {
                println!("       First violation timestamp : {:.3}s into session", t);
            }
            println!(
                "       Recommendation: Apply burst pacing or upgrade provider capacity tier."
            );
        }
        println!("=================================================================");
    }

    if !result.ok {
        std::process::exit(1);
    }

    Ok(())
}

fn format_token_sources(summary: TokenSourceSummary) -> String {
    match (summary.reported_events, summary.estimated_events) {
        (0, 0) => "no token-bearing events".to_string(),
        (_, 0) => format!(
            "reported: {} tokens across {} events",
            summary.reported_tokens, summary.reported_events
        ),
        (0, _) => format!(
            "estimated_from_bytes: {} tokens across {} events",
            summary.estimated_tokens, summary.estimated_events
        ),
        _ => format!(
            "mixed: {} reported tokens/{} events + {} estimated tokens/{} events",
            summary.reported_tokens,
            summary.reported_events,
            summary.estimated_tokens,
            summary.estimated_events
        ),
    }
}

fn print_estimate_warning(summary: TokenSourceSummary) {
    if summary.contains_estimates() {
        println!(
            "Token Estimate     : payload_bytes / 4 with a 10-token floor; uncalibrated, not a tokenizer count"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_source_labels_distinguish_reported_estimated_and_mixed() {
        assert_eq!(
            format_token_sources(TokenSourceSummary {
                reported_events: 1,
                reported_tokens: 100,
                estimated_events: 0,
                estimated_tokens: 0,
            }),
            "reported: 100 tokens across 1 events"
        );
        assert_eq!(
            format_token_sources(TokenSourceSummary {
                reported_events: 0,
                reported_tokens: 0,
                estimated_events: 2,
                estimated_tokens: 40,
            }),
            "estimated_from_bytes: 40 tokens across 2 events"
        );
        assert!(format_token_sources(TokenSourceSummary {
            reported_events: 1,
            reported_tokens: 100,
            estimated_events: 2,
            estimated_tokens: 40,
        })
        .starts_with("mixed:"));
    }
}
