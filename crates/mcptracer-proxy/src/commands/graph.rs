use std::path::PathBuf;

use anyhow::{anyhow, Result};
use chrono::DateTime;
use clap::{Args, ValueEnum};
use mcptracer_storage::graph::{Graph, GraphEdge, GraphFilter, GraphNode};
use mcptracer_storage::Store;

#[derive(Args)]
pub struct GraphArgs {
    /// Restrict to a single tool name.
    #[arg(long)]
    pub tool: Option<String>,

    /// Restrict to a single opaque server key (see `index facts --json`).
    #[arg(long)]
    pub server: Option<String>,

    /// Restrict to records at or after this RFC3339 timestamp, e.g.
    /// 2026-07-01T00:00:00Z.
    #[arg(long)]
    pub since: Option<String>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = GraphFormat::Jsonl)]
    pub format: GraphFormat,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum GraphFormat {
    #[default]
    Jsonl,
    Dot,
}

/// Export the temporal tool memory graph (sessions, tools, tool versions,
/// and their calls/has_version/supersedes edges) as JSONL or DOT. See
/// `docs/spec/graph-export.md` for the schema.
pub async fn run(args: GraphArgs, db_path: PathBuf) -> Result<()> {
    let store = Store::open(&db_path)?;
    let since_ns = match &args.since {
        Some(text) => Some(
            DateTime::parse_from_rfc3339(text)
                .map_err(|error| {
                    anyhow!("--since must be RFC3339 (e.g. 2026-07-01T00:00:00Z): {error}")
                })?
                .timestamp_nanos_opt()
                .ok_or_else(|| anyhow!("--since is out of the representable range"))?,
        ),
        None => None,
    };
    let filter = GraphFilter {
        tool: args.tool,
        server_key: args.server,
        since_ns,
    };
    let graph = store.export_graph(&filter)?;

    match args.format {
        GraphFormat::Jsonl => print_jsonl(&graph)?,
        GraphFormat::Dot => print_dot(&graph),
    }
    Ok(())
}

fn print_jsonl(graph: &Graph) -> Result<()> {
    for node in &graph.nodes {
        println!("{}", serde_json::to_string(node)?);
    }
    for edge in &graph.edges {
        println!("{}", serde_json::to_string(edge)?);
    }
    Ok(())
}

fn print_dot(graph: &Graph) {
    println!("digraph mcptracer {{");
    for node in &graph.nodes {
        let (label, shape) = match node {
            GraphNode::Session { id: _, client, .. } => (format!("session\\n{client}"), "box"),
            GraphNode::Tool { name, .. } => (format!("tool\\n{name}"), "ellipse"),
            GraphNode::ToolVersion {
                tool_name,
                description_hash,
                ..
            } => (
                format!(
                    "{tool_name}\\n{}",
                    description_hash
                        .as_deref()
                        .unwrap_or("")
                        .chars()
                        .take(8)
                        .collect::<String>()
                ),
                "note",
            ),
        };
        println!("  {:?} [label={label:?}, shape={shape}];", node.id());
    }
    for edge in &graph.edges {
        match edge {
            GraphEdge::Calls { from, to, .. } => {
                println!("  {from:?} -> {to:?} [label=\"calls\"];");
            }
            GraphEdge::HasVersion { from, to } => {
                println!("  {from:?} -> {to:?} [label=\"has_version\", style=dashed];");
            }
            GraphEdge::Supersedes { from, to } => {
                println!("  {from:?} -> {to:?} [label=\"supersedes\", color=red];");
            }
        }
    }
    println!("}}");
}
