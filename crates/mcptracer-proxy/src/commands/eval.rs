use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use mcptracer_model::eval::{evaluate, parse_spec};
use mcptracer_storage::Store;

/// Exit code for spec errors (the eval spec itself is broken).
const EXIT_SPEC_ERROR: i32 = 2;

/// Bump alongside any breaking change to `eval --json`'s shape and publish a
/// new `schemas/eval-report.vN.schema.json`. `EvalReport` itself lives in
/// `mcptracer_model::eval` and has no room for this field, so it is spliced
/// into the serialized JSON here rather than added to the struct.
const EVAL_REPORT_SCHEMA_VERSION: u32 = 2;

#[derive(Args)]
pub struct EvalArgs {
    /// Session id (or unique prefix) to score.
    pub session_id: String,

    /// TOML eval spec (expected + forbidden tool calls).
    #[arg(long)]
    pub spec: PathBuf,

    /// Print the structured report as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Score a session against an expected-tool-call spec: an accuracy metric,
/// not just pass/fail, so it can be tracked over time. Offline against a
/// recorded artifact; never calls a live model.
pub async fn run(args: EvalArgs, db_path: PathBuf) -> Result<()> {
    let store = Store::open(&db_path)?;
    let messages = store.get_messages(&args.session_id)?;

    let spec_src = match std::fs::read_to_string(&args.spec) {
        Ok(src) => src,
        Err(err) => {
            eprintln!("spec error: cannot read {}: {err}", args.spec.display());
            std::process::exit(EXIT_SPEC_ERROR);
        }
    };
    let spec = match parse_spec(&spec_src) {
        Ok(spec) => spec,
        Err(err) => {
            eprintln!("spec error: {err}");
            std::process::exit(EXIT_SPEC_ERROR);
        }
    };

    let report = evaluate(&spec, &messages);

    if args.json {
        let mut payload = serde_json::to_value(&report)?;
        payload
            .as_object_mut()
            .expect("EvalReport always serializes as a JSON object")
            .insert(
                "schema_version".to_string(),
                serde_json::json!(EVAL_REPORT_SCHEMA_VERSION),
            );
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        for result in &report.results {
            if result.satisfied {
                println!("PASS {}", result.description);
            } else {
                println!(
                    "FAIL {} — {}",
                    result.description,
                    result.reason.as_deref().unwrap_or("failed")
                );
            }
        }
        println!("accuracy: {:.2}", report.accuracy);
    }

    Ok(())
}
