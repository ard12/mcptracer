use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use mcptracer_intel::semantic::{build_corpus, search};
use mcptracer_storage::Store;

#[derive(Args)]
pub struct SemanticArgs {
    /// Search query.
    pub query: String,

    /// Include sessions recorded with redaction policy `none` in the index.
    #[arg(long)]
    pub allow_unredacted: bool,

    /// Maximum number of results.
    #[arg(long, default_value_t = 10)]
    pub limit: usize,

    /// Print results as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Experimental local lexical search over the derived index. Not a neural
/// embedding model — see `docs/spec/semantic-search.md` for exactly what
/// this is and isn't.
pub async fn run(args: SemanticArgs, db_path: PathBuf) -> Result<()> {
    let store = Store::open(&db_path)?;
    let corpus = build_corpus(&store, args.allow_unredacted)?;
    let hits = search(&corpus, &args.query, args.limit);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else if hits.is_empty() {
        println!("No matches for {:?}.", args.query);
    } else {
        for hit in &hits {
            println!(
                "{:.3}  [{}] {}",
                hit.score, hit.document.kind, hit.document.summary
            );
        }
    }
    Ok(())
}
