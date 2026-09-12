use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use mcptracer_storage::Store;

#[derive(Args)]
pub struct MergeArgs {
    /// Source session ids (or unique prefixes) in the desired merge order.
    #[arg(required = true, num_args = 2..)]
    pub session_ids: Vec<String>,

    /// Collapse equivalent client request/response pairs. Requests containing
    /// redacted parameter values are retained to avoid false equivalence.
    #[arg(long)]
    pub deduplicate: bool,

    /// Print the merge result as JSON.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: MergeArgs, db_path: PathBuf) -> Result<()> {
    let mut store = Store::open(&db_path)?;
    let result = store.merge_sessions(&args.session_ids, args.deduplicate)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "Merged {} sessions into {}",
            result.source_session_ids.len(),
            result.session_id
        );
        println!("  messages: {}", result.total_messages);
        if args.deduplicate {
            println!("  deduplicated calls: {}", result.deduplicated_calls);
        }
    }

    Ok(())
}
