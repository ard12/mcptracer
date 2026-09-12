use std::path::PathBuf;

use anyhow::Result;
use chrono::{TimeZone, Utc};
use clap::Args;
use mcptracer_storage::{SearchQuery, Store};
use serde_json::json;

#[derive(Args)]
pub struct SearchArgs {
    /// Only messages that are a `tools/call` for this tool name.
    #[arg(long)]
    pub tool: Option<String>,

    /// Only messages with this JSON-RPC method.
    #[arg(long)]
    pub method: Option<String>,

    /// Only error responses.
    #[arg(long)]
    pub errors: bool,

    /// Maximum number of matches to return (newest first).
    #[arg(short, long, default_value = "50")]
    pub limit: usize,

    /// Print matches as a JSON array instead of a table.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: SearchArgs, db_path: PathBuf) -> Result<()> {
    let store = Store::open(&db_path)?;
    let query = SearchQuery {
        tool: args.tool,
        method: args.method,
        errors_only: args.errors,
        limit: args.limit,
    };
    let hits = store.search_messages(&query)?;

    if args.json {
        let value: Vec<_> = hits
            .iter()
            .map(|hit| {
                json!({
                    "session_id": hit.session_id,
                    "seq": hit.seq,
                    "ts_ns": hit.ts_ns,
                    "direction": hit.direction,
                    "method": hit.method,
                    "tool_name": hit.tool_name,
                    "is_error": hit.is_error,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    if hits.is_empty() {
        println!("No matching messages.");
        return Ok(());
    }

    println!(
        "{:<36} {:<5} {:<24} {:<16} {:<6} WHEN",
        "SESSION", "SEQ", "METHOD", "TOOL", "STATUS"
    );
    println!("{}", "-".repeat(100));
    for hit in &hits {
        println!(
            "{:<36} {:<5} {:<24} {:<16} {:<6} {}",
            hit.session_id,
            hit.seq,
            hit.method.as_deref().unwrap_or(""),
            hit.tool_name.as_deref().unwrap_or(""),
            if hit.is_error { "ERROR" } else { "OK" },
            format_ns(hit.ts_ns)
        );
    }
    Ok(())
}

fn format_ns(ns: i64) -> String {
    let secs = ns / 1_000_000_000;
    match Utc.timestamp_opt(secs, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => format!("{secs}s"),
    }
}
