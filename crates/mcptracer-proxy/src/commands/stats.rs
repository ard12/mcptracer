use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use mcptracer_model::{correlate, SessionStats};
use mcptracer_storage::Store;

#[derive(Args)]
pub struct StatsArgs {
    /// Session id (or unique prefix) to summarize.
    pub session_id: String,

    /// Print the stats as JSON instead of a human summary.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: StatsArgs, db_path: PathBuf) -> Result<()> {
    let store = Store::open(&db_path)?;
    let messages = store.get_messages(&args.session_id)?;
    let stats = correlate(&messages).stats;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        print_stats(&stats);
    }
    Ok(())
}

fn print_stats(stats: &SessionStats) {
    println!("exchanges     {}", stats.total_exchanges);
    println!(
        "  ok {}  errors {}  unanswered {}  orphan {}",
        stats.ok, stats.errors, stats.unanswered, stats.orphan_responses
    );
    println!("notifications {}", stats.notifications);

    let answered = stats.ok + stats.errors;
    if answered > 0 {
        let rate = stats.errors as f64 / stats.total_exchanges.max(1) as f64 * 100.0;
        println!("error rate    {rate:.1}%");
    }

    println!(
        "latency       p50 {}  p95 {}  max {}",
        fmt_ms(stats.latency_p50_ns),
        fmt_ms(stats.latency_p95_ns),
        fmt_ms(stats.latency_max_ns)
    );

    if !stats.tool_call_counts.is_empty() {
        println!("tools");
        for (tool, count) in &stats.tool_call_counts {
            println!("  {count:>4}  {tool}");
        }
    }
}

fn fmt_ms(ns: Option<i64>) -> String {
    match ns {
        Some(ns) => format!("{:.1}ms", ns as f64 / 1_000_000.0),
        None => "-".to_string(),
    }
}
