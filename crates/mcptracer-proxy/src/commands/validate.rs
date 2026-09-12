use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::session_health::inspect_session;
use mcptracer_model::SessionIntegrityReport;
use mcptracer_storage::Store;

/// Exit code used when a session exists but is not eligible as a trusted gate.
const EXIT_INVALID_SESSION: i32 = 1;

/// Bump alongside any breaking change to `ValidateOutput`'s JSON shape and
/// publish a new `schemas/session-integrity-report.vN.schema.json`.
const SESSION_INTEGRITY_REPORT_SCHEMA_VERSION: u32 = 2;

#[derive(Args)]
pub struct ValidateArgs {
    /// Session id (or unique prefix) to validate.
    pub session_id: String,

    /// Print a stable machine-readable integrity report.
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct ValidateOutput<'a> {
    schema_version: u32,
    session_id: &'a str,
    healthy: bool,
    #[serde(flatten)]
    report: &'a SessionIntegrityReport,
}

pub async fn run(args: ValidateArgs, db_path: PathBuf) -> Result<()> {
    let store = Store::open(&db_path)?;
    let health = inspect_session(&store, &args.session_id)?;
    let healthy = health.report.is_healthy();

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&ValidateOutput {
                schema_version: SESSION_INTEGRITY_REPORT_SCHEMA_VERSION,
                session_id: &health.session_id,
                healthy,
                report: &health.report,
            })?
        );
    } else if healthy {
        println!("PASS session {} is capture-complete", health.session_id);
    } else {
        println!(
            "FAIL session {} has {} integrity issue(s)",
            health.session_id,
            health.report.issues.len()
        );
        for issue in &health.report.issues {
            match issue.seq {
                Some(seq) => println!("  {:?} at seq {seq}", issue.kind),
                None => println!("  {:?}", issue.kind),
            }
        }
    }

    if !healthy {
        std::process::exit(EXIT_INVALID_SESSION);
    }
    Ok(())
}
