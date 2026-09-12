use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::Args;
use mcptracer_model::optimize::{suggest, SessionHistory};
use mcptracer_storage::Store;

use crate::session_health::inspect_session;

#[derive(Args)]
pub struct OptimizeArgs {
    /// Print suggestions as JSON (includes confidence and provenance).
    #[arg(long)]
    pub json: bool,
    /// Acknowledge that optimize will inspect unredacted stored payloads.
    #[arg(long)]
    pub allow_unredacted: bool,
}

/// Mine every recorded session for latency thresholds, volatile response
/// pointers, recurring-failure assertion templates, a golden-session
/// candidate, and bench parameters. Never mutates anything.
pub async fn run(args: OptimizeArgs, db_path: PathBuf) -> Result<()> {
    let store = Store::open(&db_path)?;
    let session_ids = store.all_session_ids()?;
    if session_ids.is_empty() {
        println!("No recorded sessions to mine. Record one first with `mcptracer record`.");
        return Ok(());
    }
    require_redaction_acknowledgement(&store, &session_ids, args.allow_unredacted)?;

    let mut healths = Vec::with_capacity(session_ids.len());
    for session_id in &session_ids {
        healths.push(inspect_session(&store, session_id)?);
    }
    let histories: Vec<SessionHistory<'_>> = healths
        .iter()
        .map(|health| SessionHistory {
            session_id: &health.session_id,
            healthy: health.report.is_healthy(),
            messages: &health.messages,
        })
        .collect();

    let suggestions = suggest(&histories);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&suggestions)?);
    } else if suggestions.is_empty() {
        println!("No suggestions; not enough recorded history yet.");
    } else {
        for suggestion in &suggestions {
            println!("[{:?}] {}", suggestion.kind, suggestion.description);
            println!("  confidence: {:.2}", suggestion.confidence);
            for line in suggestion.command.lines() {
                println!("  {line}");
            }
            println!();
        }
    }

    Ok(())
}

fn require_redaction_acknowledgement(
    store: &Store,
    session_ids: &[String],
    allow_unredacted: bool,
) -> Result<()> {
    let unredacted: Vec<String> = session_ids
        .iter()
        .map(|session_id| store.get_session_summary(session_id))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|summary| summary.redaction_policy == "none")
        .map(|summary| summary.id)
        .collect();

    if unredacted.is_empty() {
        return Ok(());
    }
    if !allow_unredacted {
        bail!(
            "refusing to optimize unredacted sessions; pass --allow-unredacted, or re-record with --redact default"
        );
    }

    eprintln!(
        "[mcptracer] WARNING: optimize will inspect unredacted payloads from session(s): {}",
        unredacted.join(", ")
    );
    Ok(())
}
