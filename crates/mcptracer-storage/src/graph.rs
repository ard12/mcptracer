//! Temporal tool memory graph over the derived index.
//!
//! Exposes sessions, tools, and tool versions as nodes, and `calls`,
//! `has_version`, and `supersedes` as edges — built entirely from data
//! already in `sessions`, `tool_versions`, `tool_version_observations`, and
//! `memory_edges`. See `docs/spec/graph-export.md` for the schema.
//!
//! Node/edge types the wider design sketched (`findings`, `bench runs`,
//! `recommendations`) are deferred: `diff`, `assert`, and `bench` results are
//! not persisted anywhere today, so there is no source of truth to export
//! them from without adding new schema — a separate, larger change.

use anyhow::Result;
use serde::Serialize;

use crate::Store;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GraphNode {
    Session {
        id: String,
        client: String,
        server_command: String,
        transport: String,
        started_at_ns: i64,
        ended_at_ns: Option<i64>,
        redaction_policy: String,
    },
    Tool {
        id: String,
        server_key: String,
        name: String,
    },
    ToolVersion {
        id: String,
        server_key: String,
        tool_name: String,
        description_hash: Option<String>,
        schema_hash: Option<String>,
        contract_hash: Option<String>,
        first_seen_at_ns: i64,
        last_seen_at_ns: i64,
    },
}

impl GraphNode {
    pub fn id(&self) -> &str {
        match self {
            GraphNode::Session { id, .. } => id,
            GraphNode::Tool { id, .. } => id,
            GraphNode::ToolVersion { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GraphEdge {
    /// A session observed/called a tool version.
    Calls {
        from: String,
        to: String,
        observed_at_ns: i64,
    },
    /// A tool version belongs to a tool.
    HasVersion { from: String, to: String },
    /// `from` (newer version) supersedes `to` (older version).
    Supersedes { from: String, to: String },
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// Query filters, all optional and combinable. `since_ns` filters by the
/// relevant per-record timestamp (session start, observation, or edge).
#[derive(Debug, Clone, Default)]
pub struct GraphFilter {
    pub tool: Option<String>,
    pub server_key: Option<String>,
    pub since_ns: Option<i64>,
}

fn tool_id(server_key: &str, tool_name: &str) -> String {
    format!("tool:{server_key}::{tool_name}")
}

fn tool_version_id(
    server_key: &str,
    tool_name: &str,
    description_hash: &str,
    schema_hash: &str,
    contract_hash: &str,
) -> String {
    format!(
        "toolversion:{server_key}::{tool_name}::{description_hash}::{schema_hash}::{contract_hash}"
    )
}

fn session_id_node(id: &str) -> String {
    format!("session:{id}")
}

impl Store {
    pub fn export_graph(&self, filter: &GraphFilter) -> Result<Graph> {
        let tool_versions = self.list_tool_versions()?;
        let observations = self.list_tool_version_observations(None)?;
        let edges = self.list_memory_edges(None)?;

        // version_key (as stored on observations) -> the ToolVersion node id,
        // computed the same way mcptracer-intel does, so both agree.
        let version_id_by_key: std::collections::HashMap<String, String> = tool_versions
            .iter()
            .map(|version| {
                let description_hash = version.description_hash.as_deref().unwrap_or("");
                let schema_hash = version.schema_hash.as_deref().unwrap_or("");
                let contract_hash = version.contract_hash.as_deref().unwrap_or("");
                let key = format!(
                    "{}::{}::{description_hash}::{schema_hash}::{contract_hash}",
                    version.server_key, version.tool_name,
                );
                let id = tool_version_id(
                    &version.server_key,
                    &version.tool_name,
                    description_hash,
                    schema_hash,
                    contract_hash,
                );
                (key, id)
            })
            .collect();

        let retained_observations: Vec<_> = observations
            .into_iter()
            .filter(|observation| {
                filter
                    .tool
                    .as_deref()
                    .is_none_or(|tool| observation.tool_name == tool)
                    && filter
                        .server_key
                        .as_deref()
                        .is_none_or(|server| observation.server_key == server)
                    && filter
                        .since_ns
                        .is_none_or(|since| observation.observed_at_ns >= since)
            })
            .collect();

        let scoped = filter.tool.is_some() || filter.server_key.is_some();

        let retained_version_ids: std::collections::HashSet<String> = retained_observations
            .iter()
            .filter_map(|observation| version_id_by_key.get(&observation.version_key).cloned())
            .collect();

        let mut graph = Graph::default();

        // Sessions: scoped to those with a retained observation when a
        // tool/server filter is active; otherwise every session, still
        // subject to --since on the session's own start time.
        let session_ids: Vec<String> = if scoped {
            let mut ids: Vec<String> = retained_observations
                .iter()
                .map(|observation| observation.session_id.clone())
                .collect();
            ids.sort();
            ids.dedup();
            ids
        } else {
            self.all_session_ids()?
        };
        for session_id in &session_ids {
            let summary = self.get_session_summary(session_id)?;
            if !scoped {
                if let Some(since) = filter.since_ns {
                    if summary.started_at_ns < since {
                        continue;
                    }
                }
            }
            graph.nodes.push(GraphNode::Session {
                id: session_id_node(&summary.id),
                client: summary.client,
                server_command: summary.server_command,
                transport: summary.transport,
                started_at_ns: summary.started_at_ns,
                ended_at_ns: summary.ended_at_ns,
                redaction_policy: summary.redaction_policy,
            });
        }

        // Tool versions + their owning tool node, restricted to versions
        // touched by a retained observation when scoped.
        let mut seen_tools = std::collections::HashSet::new();
        for version in &tool_versions {
            let id = tool_version_id(
                &version.server_key,
                &version.tool_name,
                version.description_hash.as_deref().unwrap_or(""),
                version.schema_hash.as_deref().unwrap_or(""),
                version.contract_hash.as_deref().unwrap_or(""),
            );
            if scoped && !retained_version_ids.contains(&id) {
                continue;
            }
            if let Some(since) = filter.since_ns {
                if version.last_seen_at_ns < since {
                    continue;
                }
            }
            let tool = tool_id(&version.server_key, &version.tool_name);
            if seen_tools.insert(tool.clone()) {
                graph.nodes.push(GraphNode::Tool {
                    id: tool.clone(),
                    server_key: version.server_key.clone(),
                    name: version.tool_name.clone(),
                });
            }
            graph.nodes.push(GraphNode::ToolVersion {
                id: id.clone(),
                server_key: version.server_key.clone(),
                tool_name: version.tool_name.clone(),
                description_hash: version.description_hash.clone(),
                schema_hash: version.schema_hash.clone(),
                contract_hash: version.contract_hash.clone(),
                first_seen_at_ns: version.first_seen_at_ns,
                last_seen_at_ns: version.last_seen_at_ns,
            });
            graph
                .edges
                .push(GraphEdge::HasVersion { from: tool, to: id });
        }

        let included_version_ids: std::collections::HashSet<&str> = graph
            .nodes
            .iter()
            .filter_map(|node| match node {
                GraphNode::ToolVersion { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();

        for observation in &retained_observations {
            let Some(version_node_id) = version_id_by_key.get(&observation.version_key) else {
                continue;
            };
            if !included_version_ids.contains(version_node_id.as_str()) {
                continue;
            }
            graph.edges.push(GraphEdge::Calls {
                from: session_id_node(&observation.session_id),
                to: version_node_id.clone(),
                observed_at_ns: observation.observed_at_ns,
            });
        }

        for edge in &edges {
            if edge.edge_type != "version_supersedes" {
                continue;
            }
            if let Some(since) = filter.since_ns {
                if edge.observed_at_ns < since {
                    continue;
                }
            }
            let from_id = tool_version_id_from_flat_key(&edge.from_key);
            let to_id = tool_version_id_from_flat_key(&edge.to_key);
            if !included_version_ids.contains(from_id.as_str())
                || !included_version_ids.contains(to_id.as_str())
            {
                continue;
            }
            graph.edges.push(GraphEdge::Supersedes {
                from: from_id,
                to: to_id,
            });
        }

        Ok(graph)
    }
}

/// `memory_edges.from_key`/`to_key` for `version_supersedes` edges already
/// use the flat `server_key::tool_name::description_hash::schema_hash::contract_hash`
/// format; just add the graph node prefix.
fn tool_version_id_from_flat_key(key: &str) -> String {
    format!("toolversion:{key}")
}

#[cfg(test)]
mod tests {
    use crate::{MemoryEdgeRecord, ToolVersionObservationRecord, ToolVersionRecord};

    use super::*;

    fn version_key(
        server_key: &str,
        tool: &str,
        description_hash: &str,
        schema_hash: &str,
        contract_hash: &str,
    ) -> String {
        format!("{server_key}::{tool}::{description_hash}::{schema_hash}::{contract_hash}")
    }

    /// One session that called `tool` (server `server_key`, hash `desc_hash`)
    /// at `observed_at_ns`, with the matching tool_version row.
    fn seed_session(
        store: &mut Store,
        client: &str,
        server_key: &str,
        tool: &str,
        desc_hash: &str,
        started_at_ns: i64,
        observed_at_ns: i64,
    ) -> String {
        let session_id = store
            .create_session(client, "irrelevant command", "stdio", started_at_ns)
            .unwrap();
        store.close_session(&session_id, started_at_ns + 1).unwrap();

        let version = ToolVersionRecord {
            server_key: server_key.to_string(),
            tool_name: tool.to_string(),
            description_hash: Some(desc_hash.to_string()),
            schema_hash: Some("schema".to_string()),
            contract_hash: Some("contract".to_string()),
            description_redacted: None,
            input_schema_redacted: None,
            first_session_id: session_id.clone(),
            first_seen_at_ns: observed_at_ns,
            last_seen_at_ns: observed_at_ns,
        };
        let observation = ToolVersionObservationRecord {
            session_id: session_id.clone(),
            server_key: server_key.to_string(),
            tool_name: tool.to_string(),
            version_key: version_key(server_key, tool, desc_hash, "schema", "contract"),
            description_hash: Some(desc_hash.to_string()),
            schema_hash: Some("schema".to_string()),
            contract_hash: Some("contract".to_string()),
            seq: Some(0),
            observed_at_ns,
        };
        store
            .rebuild_memory_for_session(&session_id, &[], &[], &[version], &[observation])
            .unwrap();
        session_id
    }

    #[test]
    fn full_export_includes_sessions_tools_versions_and_calls_edges() {
        let mut store = Store::open_in_memory().unwrap();
        let session_id = seed_session(&mut store, "c", "srv", "echo", "d1", 0, 10);

        let graph = store.export_graph(&GraphFilter::default()).unwrap();

        assert!(graph.nodes.iter().any(
            |n| matches!(n, GraphNode::Session { id, .. } if id == &format!("session:{session_id}"))
        ));
        assert!(graph
            .nodes
            .iter()
            .any(|n| matches!(n, GraphNode::Tool { name, .. } if name == "echo")));
        assert!(graph
            .nodes
            .iter()
            .any(|n| matches!(n, GraphNode::ToolVersion { tool_name, .. } if tool_name == "echo")));
        assert!(graph
            .edges
            .iter()
            .any(|e| matches!(e, GraphEdge::Calls { .. })));
        assert!(graph
            .edges
            .iter()
            .any(|e| matches!(e, GraphEdge::HasVersion { .. })));
    }

    #[test]
    fn tool_filter_prunes_unrelated_tools_and_sessions() {
        let mut store = Store::open_in_memory().unwrap();
        seed_session(&mut store, "c1", "srv", "echo", "d1", 0, 10);
        seed_session(&mut store, "c2", "srv", "other_tool", "d2", 0, 10);

        let filter = GraphFilter {
            tool: Some("echo".to_string()),
            ..Default::default()
        };
        let graph = store.export_graph(&filter).unwrap();

        assert!(graph
            .nodes
            .iter()
            .any(|n| matches!(n, GraphNode::Tool { name, .. } if name == "echo")));
        assert!(!graph
            .nodes
            .iter()
            .any(|n| matches!(n, GraphNode::Tool { name, .. } if name == "other_tool")));
        // Only one session called echo, so only one session node remains.
        assert_eq!(
            graph
                .nodes
                .iter()
                .filter(|n| matches!(n, GraphNode::Session { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn server_filter_prunes_other_servers() {
        let mut store = Store::open_in_memory().unwrap();
        seed_session(&mut store, "c1", "srv-a", "echo", "d1", 0, 10);
        seed_session(&mut store, "c2", "srv-b", "echo", "d2", 0, 10);

        let filter = GraphFilter {
            server_key: Some("srv-a".to_string()),
            ..Default::default()
        };
        let graph = store.export_graph(&filter).unwrap();

        assert!(graph
            .nodes
            .iter()
            .any(|n| matches!(n, GraphNode::Tool { server_key, .. } if server_key == "srv-a")));
        assert!(!graph
            .nodes
            .iter()
            .any(|n| matches!(n, GraphNode::Tool { server_key, .. } if server_key == "srv-b")));
    }

    #[test]
    fn since_filter_prunes_older_sessions() {
        let mut store = Store::open_in_memory().unwrap();
        seed_session(&mut store, "old", "srv", "echo", "d1", 0, 0);
        seed_session(&mut store, "new", "srv", "echo", "d1", 1_000, 1_000);

        let filter = GraphFilter {
            since_ns: Some(500),
            ..Default::default()
        };
        let graph = store.export_graph(&filter).unwrap();

        let session_count = graph
            .nodes
            .iter()
            .filter(|n| matches!(n, GraphNode::Session { .. }))
            .count();
        assert_eq!(session_count, 1);
    }

    #[test]
    fn supersedes_edge_included_when_both_endpoints_retained() {
        let mut store = Store::open_in_memory().unwrap();
        seed_session(&mut store, "c1", "srv", "echo", "old-hash", 0, 10);
        seed_session(&mut store, "c2", "srv", "echo", "new-hash", 20, 20);

        let newer = version_key("srv", "echo", "new-hash", "schema", "contract");
        let older = version_key("srv", "echo", "old-hash", "schema", "contract");
        store
            .replace_supersession_edges(&[MemoryEdgeRecord {
                edge_type: "version_supersedes".to_string(),
                from_type: "tool_version".to_string(),
                from_key: newer,
                to_type: "tool_version".to_string(),
                to_key: older,
                value_json: "{}".to_string(),
                session_id: None,
                source: "test".to_string(),
                observed_at_ns: 20,
                created_at_ns: 20,
            }])
            .unwrap();

        let graph = store.export_graph(&GraphFilter::default()).unwrap();

        assert!(graph
            .edges
            .iter()
            .any(|e| matches!(e, GraphEdge::Supersedes { .. })));
    }
}
