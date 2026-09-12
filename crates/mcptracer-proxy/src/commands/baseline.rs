use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::{Args, Subcommand};
use mcptracer_storage::{mtrace, Baseline, Store};

use crate::session_writer::now_ns;

#[derive(Args)]
pub struct BaselineArgs {
    #[command(subcommand)]
    pub command: BaselineCommand,
}

#[derive(Subcommand)]
pub enum BaselineCommand {
    /// Register a recorded session as a candidate baseline for
    /// (project, scenario, environment). Multiple simultaneous candidates
    /// for the same triple are allowed.
    Candidate {
        project: String,
        scenario: String,
        environment: String,
        session_id: String,
    },
    /// Promote a registered candidate to approved, superseding any prior
    /// approved baseline for the same (project, scenario, environment).
    /// Records a canonical content digest of the session at promotion time.
    Promote {
        project: String,
        scenario: String,
        environment: String,
        session_id: String,
        /// Actor performing the promotion (a name, username, or CI identity).
        #[arg(long)]
        by: String,
        #[arg(long)]
        reason: String,
        /// Upgrade an unredacted session to the default stored-copy
        /// redaction policy before computing its digest.
        #[arg(long, value_name = "POLICY")]
        redact: Option<String>,
        /// Explicitly allow computing a digest over an unredacted session.
        #[arg(long)]
        allow_unredacted: bool,
    },
    /// Revoke the current approved baseline for
    /// (project, scenario, environment). Resolve then finds nothing for
    /// that triple until a new baseline is promoted.
    Revoke {
        project: String,
        scenario: String,
        environment: String,
        #[arg(long)]
        reason: String,
    },
    /// Print the session id of the approved baseline for
    /// (project, scenario, environment), for scripting -- e.g.
    /// `mcptracer assert "$SESSION" --golden "$(mcptracer baseline resolve p s e)"`.
    /// Plain-text mode prints only the session id and nothing else, so it is
    /// safe to capture directly.
    Resolve {
        project: String,
        scenario: String,
        environment: String,
        #[arg(long)]
        json: bool,
    },
    /// List baselines in every state, optionally filtered by project.
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(args: BaselineArgs, db_path: PathBuf) -> Result<()> {
    let mut store = Store::open(&db_path)?;

    match args.command {
        BaselineCommand::Candidate {
            project,
            scenario,
            environment,
            session_id,
        } => {
            store.create_baseline_candidate(
                &project,
                &scenario,
                &environment,
                &session_id,
                now_ns(),
            )?;
            println!(
                "registered candidate baseline: project={project} scenario={scenario} \
                 environment={environment} session={session_id}"
            );
            Ok(())
        }
        BaselineCommand::Promote {
            project,
            scenario,
            environment,
            session_id,
            by,
            reason,
            redact,
            allow_unredacted,
        } => {
            let force_default_redaction = match redact.as_deref() {
                None => false,
                Some("default") => true,
                Some(value) => {
                    return Err(anyhow!(
                        "unsupported baseline redaction policy: {value} (expected default)"
                    ))
                }
            };
            if force_default_redaction && allow_unredacted {
                return Err(anyhow!(
                    "--redact default cannot be combined with --allow-unredacted"
                ));
            }
            let options = mtrace::ExportOptions {
                force_default_redaction,
                allow_unredacted,
            };
            let document = store.export_mtrace_document(&session_id, options)?;
            let digest = mtrace::canonical_digest(&document);

            store.promote_baseline(
                &project,
                &scenario,
                &environment,
                &session_id,
                &digest,
                &by,
                &reason,
                now_ns(),
            )?;
            println!(
                "approved baseline: project={project} scenario={scenario} \
                 environment={environment} session={session_id}"
            );
            println!("  digest: {digest}");
            Ok(())
        }
        BaselineCommand::Revoke {
            project,
            scenario,
            environment,
            reason,
        } => {
            store.revoke_baseline(&project, &scenario, &environment, &reason, now_ns())?;
            println!(
                "revoked baseline: project={project} scenario={scenario} environment={environment}"
            );
            Ok(())
        }
        BaselineCommand::Resolve {
            project,
            scenario,
            environment,
            json,
        } => {
            let baseline = store.resolve_approved_baseline(&project, &scenario, &environment)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&baseline)?);
            } else {
                println!("{}", baseline.session_id);
            }
            Ok(())
        }
        BaselineCommand::List { project, json } => {
            let baselines = store.list_baselines(project.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&baselines)?);
            } else if baselines.is_empty() {
                println!("No baselines registered yet.");
            } else {
                print_baselines_table(&baselines);
            }
            Ok(())
        }
    }
}

fn print_baselines_table(baselines: &[Baseline]) {
    println!(
        "{:<16} {:<12} {:<12} {:<10} SESSION",
        "PROJECT", "SCENARIO", "ENVIRONMENT", "STATE"
    );
    println!("{}", "-".repeat(90));
    for baseline in baselines {
        println!(
            "{:<16} {:<12} {:<12} {:<10} {}",
            baseline.project,
            baseline.scenario,
            baseline.environment,
            baseline.state,
            baseline.session_id
        );
    }
}
