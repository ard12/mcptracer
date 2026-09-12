use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

mod commands {
    pub mod assertions;
    pub mod baseline;
    pub mod bench;
    pub mod diff;
    pub mod diff_batch;
    pub mod eval;
    pub mod export;
    pub mod graph;
    pub mod import;
    pub mod index;
    pub mod inspect;
    pub mod merge;
    pub mod optimize;
    pub mod quota;
    pub mod record;
    pub mod record_http;
    pub mod replay;
    pub mod replay_http;
    pub mod route;
    pub mod search;
    #[cfg(feature = "semantic-search")]
    pub mod semantic;
    pub mod serve;
    pub mod sessions;
    pub mod setup;
    pub mod stats;
    pub mod validate;
    pub mod verify;
}
mod ci_formats;
mod session_health;
mod session_writer;
mod sse;

#[derive(Parser)]
#[command(name = "mcptracer")]
#[command(about = "Record and inspect MCP stdio sessions")]
#[command(version)]
struct Cli {
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Wrap an MCP stdio server, forwarding traffic byte-exact and recording
    /// a normalized, correlated copy.
    Record(commands::record::RecordArgs),
    /// Record MCP Streamable HTTP traffic through a local reverse proxy.
    RecordHttp(commands::record_http::RecordHttpArgs),
    /// Safely wrap configured stdio MCP servers for a supported client.
    Setup(commands::setup::SetupArgs),
    /// Re-run a recorded session's client traffic against a (possibly
    /// changed) server, capturing a new session for comparison.
    Replay(commands::replay::ReplayArgs),
    /// Re-run a recorded Streamable HTTP session's client traffic against a
    /// live HTTP target, capturing a new session for comparison.
    ReplayHttp(commands::replay_http::ReplayHttpArgs),
    /// Run an offline stdio MCP mock server from a recorded session.
    Serve(commands::serve::ServeArgs),
    /// Check a recorded session's capture integrity before using it as a gate.
    Validate(commands::validate::ValidateArgs),
    /// Combine N recorded sessions into one, optionally deduplicated.
    Merge(commands::merge::MergeArgs),
    /// List and inspect recorded sessions.
    Sessions(commands::sessions::SessionsArgs),
    /// Compare two recorded sessions (exit 1 on meaningful differences).
    Diff(commands::diff::DiffArgs),
    /// Run diff over many session pairs from one config; fan-out CI gate.
    DiffBatch(commands::diff_batch::DiffBatchArgs),
    /// Score a session against an expected-tool-call spec (accuracy, not
    /// just pass/fail); offline, no live model.
    Eval(commands::eval::EvalArgs),
    /// Check a session against a TOML assertion spec or a golden session.
    Assert(commands::assertions::AssertArgs),
    /// Address an approved baseline session by (project, scenario,
    /// environment) instead of a raw session id, with a
    /// candidate/approved/superseded/revoked lifecycle.
    Baseline(commands::baseline::BaselineArgs),
    /// Replay captured traffic repeatedly and report load-test metrics.
    Bench(commands::bench::BenchArgs),
    /// Summarize one session (exchange counts, error rate, latency, tools).
    Stats(commands::stats::StatsArgs),
    /// Search messages across all recorded sessions.
    Search(commands::search::SearchArgs),
    /// Write one validated, redaction-safe session artifact.
    Export(commands::export::ExportArgs),
    /// Import a local `.mtrace` artifact without executing its contents.
    Import(commands::import::ImportArgs),
    /// Offline-verify a `.mtrace` artifact (and, if recorded, a baseline or
    /// assertion spec) against an evidence manifest's recorded digests.
    Verify(commands::verify::VerifyArgs),
    /// Rebuild or inspect the derived memory index over recorded sessions.
    Index(commands::index::IndexArgs),
    /// Read-only local web UI over recorded sessions (session list + timeline).
    Inspect(commands::inspect::InspectArgs),
    /// Recommend next commands for a session from the derived index and stats.
    Route(commands::route::RouteArgs),
    /// Mine recorded history for latency/assertion/bench suggestions.
    Optimize(commands::optimize::OptimizeArgs),
    /// Export the temporal tool memory graph as JSONL or DOT.
    Graph(commands::graph::GraphArgs),
    /// Evaluate token-burst and rate-limit survival offline, labeling reported vs estimated counts.
    Quota(commands::quota::QuotaArgs),
    /// Experimental local lexical search over the derived index (off by
    /// default at build time; requires the `semantic-search` feature).
    #[cfg(feature = "semantic-search")]
    Semantic(commands::semantic::SemanticArgs),
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("MCPTRACER_LOG")
                .add_directive(tracing::Level::WARN.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    // Parsed before the runtime exists so `--help`/`--version` never pay for
    // one.
    let cli = Cli::parse();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    let result = runtime.block_on(dispatch(cli));

    // Deliberately not a plain drop. `tokio::io::stdin()` reads on the
    // blocking pool, and a blocking read cannot be cancelled: dropping the
    // runtime would join that thread and wait forever for input that is never
    // coming. `record` hits this every time its server exits first — the
    // session is already fully finalized by the time we get here, so there is
    // nothing left to wait for.
    runtime.shutdown_background();
    result
}

async fn dispatch(cli: Cli) -> Result<()> {
    let db_path = cli.db.unwrap_or_else(mcptracer_storage::default_db_path);

    match cli.command {
        Commands::Record(args) => commands::record::run(args, db_path).await,
        Commands::RecordHttp(args) => commands::record_http::run(args, db_path).await,
        Commands::Setup(args) => commands::setup::run(args).await,
        Commands::Replay(args) => commands::replay::run(args, db_path).await,
        Commands::ReplayHttp(args) => commands::replay_http::run(args, db_path).await,
        Commands::Serve(args) => commands::serve::run(args, db_path).await,
        Commands::Validate(args) => commands::validate::run(args, db_path).await,
        Commands::Merge(args) => commands::merge::run(args, db_path).await,
        Commands::Sessions(args) => commands::sessions::run(args, db_path).await,
        Commands::Diff(args) => commands::diff::run(args, db_path).await,
        Commands::DiffBatch(args) => commands::diff_batch::run(args, db_path).await,
        Commands::Eval(args) => commands::eval::run(args, db_path).await,
        Commands::Assert(args) => commands::assertions::run(args, db_path).await,
        Commands::Baseline(args) => commands::baseline::run(args, db_path).await,
        Commands::Stats(args) => commands::stats::run(args, db_path).await,
        Commands::Search(args) => commands::search::run(args, db_path).await,
        Commands::Bench(args) => commands::bench::run(args, db_path).await,
        Commands::Export(args) => commands::export::run(args, db_path).await,
        Commands::Import(args) => commands::import::run(args, db_path).await,
        Commands::Verify(args) => commands::verify::run(args).await,
        Commands::Index(args) => commands::index::run(args, db_path).await,
        Commands::Inspect(args) => commands::inspect::run(args, db_path).await,
        Commands::Route(args) => commands::route::run(args, db_path).await,
        Commands::Optimize(args) => commands::optimize::run(args, db_path).await,
        Commands::Graph(args) => commands::graph::run(args, db_path).await,
        Commands::Quota(args) => commands::quota::run(args, db_path).await,
        #[cfg(feature = "semantic-search")]
        Commands::Semantic(args) => commands::semantic::run(args, db_path).await,
    }
}
