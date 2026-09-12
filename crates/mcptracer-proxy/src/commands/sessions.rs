use std::path::PathBuf;

use anyhow::Result;
use chrono::{TimeZone, Utc};
use clap::{Args, Subcommand};
use mcptracer_model::{correlate, ExchangeStatus, SessionModel};
use mcptracer_storage::{SessionSummary, Store};
use serde_json::json;

/// Default cap on `sessions show` output. Chosen to comfortably cover a
/// typical handshake-plus-a-few-calls session while still bounding a long
/// agent run; `--all` opts out. `sessions list` caps at 20 by the same logic.
const DEFAULT_SHOW_LIMIT: usize = 200;

#[derive(Args)]
pub struct SessionsArgs {
    #[command(subcommand)]
    pub command: SessionsCommand,
}

#[derive(Subcommand)]
pub enum SessionsCommand {
    List {
        #[arg(short, long, default_value = "20")]
        limit: usize,
        /// Print sessions as a JSON array instead of a table.
        #[arg(long)]
        json: bool,
    },
    Show {
        session_id: String,
        #[arg(long)]
        full: bool,
        /// Print messages as a JSON array instead of a table.
        #[arg(long)]
        json: bool,
        /// Show the correlated request/response view instead of the raw
        /// message log (mutually exclusive with `--full`).
        #[arg(long)]
        calls: bool,
        /// Maximum messages to print, newest-seq-last. A long agent session
        /// is otherwise an unbounded dump into a terminal, a CI log, or a
        /// model's context window.
        #[arg(long, default_value_t = DEFAULT_SHOW_LIMIT)]
        limit: usize,
        /// Skip this many messages before printing, for paging through a
        /// session `--limit` at a time.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Print every message, ignoring `--limit`.
        #[arg(long, conflicts_with = "limit")]
        all: bool,
    },
    /// Export a session as OTLP/JSON spans (best-effort OTel GenAI
    /// semantic conventions). File output only; never overwrites.
    ExportOtel {
        session_id: String,
        #[arg(long)]
        out: PathBuf,
        /// Explicitly allow export of a session recorded without redaction.
        #[arg(long)]
        allow_unredacted: bool,
    },
}

pub async fn run(args: SessionsArgs, db_path: PathBuf) -> Result<()> {
    let store = Store::open(&db_path)?;

    match args.command {
        SessionsCommand::List { limit, json } => {
            let sessions = store.list_sessions(limit)?;

            if json {
                print_sessions_json(&sessions)?;
                return Ok(());
            }

            if sessions.is_empty() {
                println!("No sessions recorded yet.");
                println!("Start recording with: mcptracer record -- <mcp-server-command>");
                return Ok(());
            }

            println!(
                "{:<36} {:<12} {:<9} {:<8} STARTED",
                "SESSION ID", "CLIENT", "MESSAGES", "DROPPED"
            );
            println!("{}", "-".repeat(86));
            for session in sessions {
                println!(
                    "{:<36} {:<12} {:<9} {:<8} {}",
                    session.id,
                    truncate(&session.client, 12),
                    session.total_messages,
                    session.dropped_messages,
                    format_ns(session.started_at_ns)
                );
            }
        }
        SessionsCommand::Show {
            session_id,
            full,
            json,
            calls,
            limit,
            offset,
            all,
        } => {
            let messages = store.get_messages(&session_id)?;

            if calls {
                // The correlated view is a whole-session model: windowing it
                // would pair requests against responses that were merely
                // sliced out, inventing orphans and unanswered exchanges that
                // the session never had.
                if all || offset != 0 || limit != DEFAULT_SHOW_LIMIT {
                    return Err(anyhow::anyhow!(
                        "--limit/--offset/--all cannot be combined with --calls: the correlated \
                         view is computed over the whole session, and windowing it would report \
                         exchanges as orphaned or unanswered when they are neither"
                    ));
                }
                let model = correlate(&messages);
                if json {
                    println!("{}", serde_json::to_string_pretty(&model)?);
                } else {
                    print_calls_table(&session_id, &model);
                }
                return Ok(());
            }

            let total = messages.len();
            let window = if all {
                &messages[..]
            } else {
                let start = offset.min(total);
                let end = start.saturating_add(limit).min(total);
                &messages[start..end]
            };
            report_truncation(total, window.len(), offset, all);

            if json {
                print_messages_json(window)?;
                return Ok(());
            }

            if messages.is_empty() {
                println!("Session has no recorded messages: {}", session_id);
                return Ok(());
            }

            println!(
                "{:<5} {:<4} {:<13} {:<24} {:<20} STATUS",
                "SEQ", "DIR", "KIND", "METHOD", "TOOL"
            );
            println!("{}", "-".repeat(90));
            for msg in window {
                let dir = if msg.direction == "c2s" { "->" } else { "<-" };
                let method = msg.method.as_deref().unwrap_or("");
                let tool = msg.tool_name.as_deref().unwrap_or("");
                let status = if msg.is_error { "ERROR" } else { "OK" };
                println!(
                    "{:<5} {:<4} {:<13} {:<24} {:<20} {}",
                    msg.seq, dir, msg.message_kind, method, tool, status
                );

                if full {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&msg.payload) {
                        println!("{}", serde_json::to_string_pretty(&value)?);
                    }
                }
            }
        }
        SessionsCommand::ExportOtel {
            session_id,
            out,
            allow_unredacted,
        } => {
            if out.exists() {
                anyhow::bail!("refusing to overwrite existing file: {}", out.display());
            }
            let summary = store.get_session_summary(&session_id)?;
            if summary.redaction_policy == "none" && !allow_unredacted {
                anyhow::bail!(
                    "session {} was recorded with redaction policy 'none'; pass --allow-unredacted, or re-record with --redact default",
                    summary.id
                );
            }
            let messages = store.get_messages(&summary.id)?;
            let model = correlate(&messages);
            let document =
                mcptracer_model::otel::session_to_otlp_json(&summary.id, &summary.client, &model);
            std::fs::write(&out, serde_json::to_vec_pretty(&document)?)?;
            println!("Exported session {} to {}", summary.id, out.display());
        }
    }

    Ok(())
}

/// Tell the operator on **stderr** when output was windowed, so stdout stays
/// machine-parseable. Silent when the window already covers everything.
fn report_truncation(total: usize, shown: usize, offset: usize, all: bool) {
    if all || shown == total {
        return;
    }
    eprintln!(
        "[mcptracer] showing {shown} of {total} message(s) (offset {offset}). \
         Use --all for the whole session, or --offset {} to continue.",
        offset + shown
    );
}

fn print_calls_table(session_id: &str, model: &SessionModel) {
    if model.exchanges.is_empty() && model.notifications.is_empty() {
        println!("Session has no recorded messages: {}", session_id);
        return;
    }

    println!(
        "{:<6} {:<6} {:<4} {:<24} {:<20} {:<9} LATENCY",
        "REQ", "RESP", "DIR", "METHOD", "TOOL", "STATUS"
    );
    println!("{}", "-".repeat(90));
    for e in &model.exchanges {
        let dir = if e.origin == mcptracer_protocol::Direction::ClientToServer {
            "->"
        } else {
            "<-"
        };
        println!(
            "{:<6} {:<6} {:<4} {:<24} {:<20} {:<9} {}",
            seq_cell(e.request_seq),
            seq_cell(e.response_seq),
            dir,
            e.method.as_deref().unwrap_or(""),
            e.tool_name.as_deref().unwrap_or(""),
            status_cell(e.status),
            latency_cell(e.latency_ns),
        );
    }

    let stats = &model.stats;
    println!("{}", "-".repeat(90));
    println!(
        "{} exchanges: {} ok, {} error, {} unanswered, {} orphan; {} notifications",
        stats.total_exchanges,
        stats.ok,
        stats.errors,
        stats.unanswered,
        stats.orphan_responses,
        stats.notifications
    );
    if let (Some(p50), Some(p95), Some(max)) = (
        stats.latency_p50_ns,
        stats.latency_p95_ns,
        stats.latency_max_ns,
    ) {
        println!(
            "latency p50={}ms p95={}ms max={}ms",
            p50 / 1_000_000,
            p95 / 1_000_000,
            max / 1_000_000
        );
    }
}

fn seq_cell(seq: Option<u64>) -> String {
    seq.map(|s| s.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn status_cell(status: ExchangeStatus) -> &'static str {
    match status {
        ExchangeStatus::Ok => "OK",
        ExchangeStatus::Error => "ERROR",
        ExchangeStatus::ToolError => "TOOL_ERROR",
        ExchangeStatus::Subscribed => "SUBSCRIBED",
        ExchangeStatus::Unanswered => "UNANSWERED",
        ExchangeStatus::OrphanResponse => "ORPHAN",
    }
}

fn latency_cell(latency_ns: Option<i64>) -> String {
    match latency_ns {
        Some(ns) => format!("{}ms", ns / 1_000_000),
        None => "-".to_string(),
    }
}

fn print_sessions_json(sessions: &[SessionSummary]) -> Result<()> {
    let value: Vec<_> = sessions
        .iter()
        .map(|session| {
            json!({
                "id": session.id,
                "client": session.client,
                "server_command": session.server_command,
                "transport": session.transport,
                "started_at_ns": session.started_at_ns,
                "ended_at_ns": session.ended_at_ns,
                "total_messages": session.total_messages,
                "dropped_messages": session.dropped_messages,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn print_messages_json(messages: &[mcptracer_storage::StoredMessage]) -> Result<()> {
    let value: Vec<_> = messages
        .iter()
        .map(|msg| {
            let payload: serde_json::Value = serde_json::from_str(&msg.payload)
                .unwrap_or_else(|_| serde_json::Value::String(msg.payload.clone()));
            json!({
                "seq": msg.seq,
                "ts_ns": msg.ts_ns,
                "direction": msg.direction,
                "message_kind": msg.message_kind,
                "rpc_id": msg.rpc_id,
                "method": msg.method,
                "tool_name": msg.tool_name,
                "payload_bytes": msg.payload_bytes,
                "is_error": msg.is_error,
                "error_code": msg.error_code,
                "payload": payload,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn format_ns(ns: i64) -> String {
    let secs = ns / 1_000_000_000;
    match Utc.timestamp_opt(secs, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => format!("{}s", secs),
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        value.to_string()
    } else {
        value[..max.saturating_sub(1)].to_string() + "."
    }
}
