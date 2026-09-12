//! Offline derived-memory index control plane.
//!
//! `index` reads already-recorded sessions and (re)builds the derived memory
//! tables. It never touches the record/replay forwarding path and requires no
//! network or model — it is a deterministic, local index over stored artifacts.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use mcptracer_intel::{
    compute_version_supersessions, derive_server_key, extract_session_memory, ExtractionInput,
};
use mcptracer_model::correlate;
use mcptracer_storage::Store;

#[derive(Args)]
pub struct IndexArgs {
    #[command(subcommand)]
    pub command: IndexCommand,
}

#[derive(Subcommand)]
pub enum IndexCommand {
    /// Rebuild derived memory facts, edges, and tool versions from recordings.
    Rebuild {
        /// Rebuild only this session id (or unique prefix); default is all.
        #[arg(long)]
        session: Option<String>,
    },
    /// List derived memory facts.
    Facts {
        /// Limit to one session id (or unique prefix).
        #[arg(long)]
        session: Option<String>,
        /// Print facts as a JSON array instead of a table.
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(args: IndexArgs, db_path: PathBuf) -> Result<()> {
    let mut store = Store::open(&db_path)?;
    match args.command {
        IndexCommand::Rebuild { session } => rebuild(&mut store, session.as_deref()),
        IndexCommand::Facts { session, json } => facts(&store, session.as_deref(), json),
    }
}

fn rebuild(store: &mut Store, session: Option<&str>) -> Result<()> {
    let session_ids = match session {
        Some(prefix) => vec![store.resolve_session(prefix)?],
        None => store.all_session_ids()?,
    };

    for session_id in &session_ids {
        let messages = store.get_messages(session_id)?;
        let model = correlate(&messages);
        let server_command = store.get_server_command(session_id)?;
        let server_key = derive_server_key(&server_command);
        let redaction_policy = store.get_redaction_policy(session_id)?;

        let extraction = extract_session_memory(ExtractionInput {
            session_id,
            server_key: &server_key,
            redaction_policy: &redaction_policy,
            messages: &messages,
            model: &model,
        });

        store.rebuild_memory_for_session(
            session_id,
            &extraction.facts,
            &extraction.edges,
            &extraction.tool_versions,
            &extraction.tool_version_observations,
        )?;
    }

    // Supersession spans sessions, so recompute it from the full version set
    // after any rebuild. Delete-and-replace keeps the edge set duplicate-free.
    let versions = store.list_tool_versions()?;
    let supersessions = compute_version_supersessions(&versions);
    store.replace_supersession_edges(&supersessions)?;

    let fact_count = store.list_memory_facts(None)?.len();
    println!(
        "Rebuilt derived memory for {} session(s).",
        session_ids.len()
    );
    println!("facts: {fact_count}");
    println!("tool versions: {}", versions.len());
    println!("version_supersedes edges: {}", supersessions.len());
    Ok(())
}

fn facts(store: &Store, session: Option<&str>, json: bool) -> Result<()> {
    let facts = store.list_memory_facts(session)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&facts)?);
        return Ok(());
    }

    if facts.is_empty() {
        println!("No derived facts. Run: mcptracer index rebuild");
        return Ok(());
    }

    println!("{:<32} {:<20} {:<24} SESSION", "FACT", "SUBJECT", "OBJECT");
    println!("{}", "-".repeat(96));
    for fact in &facts {
        let subject = format!("{}:{}", fact.subject_type, fact.subject_key);
        let object = match (&fact.object_type, &fact.object_key) {
            (Some(kind), Some(key)) => format!("{kind}:{key}"),
            _ => String::new(),
        };
        println!(
            "{:<32} {:<20} {:<24} {}",
            truncate(&fact.fact_type, 32),
            truncate(&subject, 20),
            truncate(&object, 24),
            fact.session_id.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        value.to_string()
    } else {
        format!("{}…", &value[..max.saturating_sub(1)])
    }
}
