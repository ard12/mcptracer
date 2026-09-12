use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use mcptracer_storage::{mtrace, vcr, Store};

#[derive(Args)]
pub struct ImportArgs {
    /// Local `.mtrace` artifact or `.vcr` cassette to import (selected by
    /// extension). Import never executes its contents.
    pub path: PathBuf,

    /// Reject unknown top-level fields instead of allowing forward-compatible
    /// extensions. Only meaningful for `.mtrace`.
    #[arg(long)]
    pub strict: bool,

    /// Client label for a session imported from a `.vcr` cassette (which
    /// carries no client field of its own).
    #[arg(long, default_value = "agent-vcr-import")]
    pub client: String,
}

pub async fn run(args: ImportArgs, db_path: PathBuf) -> Result<()> {
    let mut store = Store::open(&db_path)?;
    let is_vcr = args
        .path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("vcr"));

    if is_vcr {
        let cassette = vcr::read_file(&args.path)?;
        let result = store.import_vcr(&cassette, &args.client)?;
        println!(
            "Imported {} messages as session {}",
            result.total_messages, result.session_id
        );
        return Ok(());
    }

    let document = mtrace::read_file(&args.path, args.strict)?;
    let result = store.import_mtrace_document(document)?;
    println!(
        "Imported {} messages as session {}",
        result.total_messages, result.session_id
    );
    Ok(())
}
