//! Local, dependency-free lexical search with domain-vocabulary query
//! expansion over the derived index.
//!
//! **This is not a neural embedding model.** No model weights are bundled or
//! downloaded, and nothing here makes a network call — both hard
//! requirements for a project that treats recorded MCP traffic as sensitive
//! and forbids intelligence features from touching the protocol hot path.
//! "Semantic-ish" recall (a query like "rug pull" finding tool-description
//! drift, despite sharing no literal words) comes entirely from
//! [`DOMAIN_GLOSSARY`]: a small, explicit, auditable table mapping this
//! project's own security vocabulary onto the descriptive keywords
//! [`build_corpus`] actually writes into indexed documents. See
//! `docs/spec/semantic-search.md`.
//!
//! Feature-gated (`semantic-search`, off by default) and marked experimental.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use mcptracer_storage::Store;
use serde::Serialize;

/// Identifies which provider produced a hit's score. A future remote
/// provider must use its own distinct value here — this is the "embedding
/// provenance" the design calls for, recorded on every result rather than
/// bolted on later.
pub const LOCAL_PROVIDER: &str = "local-lexical-v1";

/// A normalized term-frequency vector: this module's entire "embedding".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SparseVector(BTreeMap<String, f64>);

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(|token| token.to_ascii_lowercase())
        .filter(|token| token.len() > 1)
        .collect()
}

pub fn embed_text(text: &str) -> SparseVector {
    let mut counts: BTreeMap<String, f64> = BTreeMap::new();
    let mut total = 0.0;
    for token in tokenize(text) {
        *counts.entry(token).or_insert(0.0) += 1.0;
        total += 1.0;
    }
    if total > 0.0 {
        for weight in counts.values_mut() {
            *weight /= total;
        }
    }
    SparseVector(counts)
}

pub fn cosine_similarity(a: &SparseVector, b: &SparseVector) -> f64 {
    let dot: f64 =
        a.0.iter()
            .filter_map(|(term, weight)| b.0.get(term).map(|other| weight * other))
            .sum();
    let norm_a = a.0.values().map(|v| v * v).sum::<f64>().sqrt();
    let norm_b = b.0.values().map(|v| v * v).sum::<f64>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// concept -> related keywords used in indexed documents. Deliberately small
/// and specific to this project's own domain (tool-version drift, the
/// "rug pull" story), not a general-purpose thesaurus.
const DOMAIN_GLOSSARY: &[(&str, &[&str])] = &[
    (
        "rug",
        &[
            "description",
            "changed",
            "schema",
            "drift",
            "supersedes",
            "security",
            "version",
        ],
    ),
    (
        "pull",
        &[
            "description",
            "changed",
            "schema",
            "drift",
            "supersedes",
            "security",
            "version",
        ],
    ),
    (
        "drift",
        &["changed", "supersedes", "version", "description", "schema"],
    ),
    ("security", &["supersedes", "changed", "drift"]),
    ("changed", &["supersedes", "drift"]),
];

pub fn expand_query(query: &str) -> SparseVector {
    let tokens = tokenize(query);
    let mut expanded = tokens.clone();
    for token in &tokens {
        for (concept, synonyms) in DOMAIN_GLOSSARY {
            if token == concept {
                expanded.extend(synonyms.iter().map(|s| s.to_string()));
            }
        }
    }
    embed_text(&expanded.join(" "))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchDocument {
    pub id: String,
    pub kind: &'static str,
    pub session_id: Option<String>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub document: SearchDocument,
    pub score: f64,
    pub provider: &'static str,
}

/// Build the searchable corpus from every recorded session's derived facts
/// and tool versions. Refuses if any session was recorded with redaction
/// policy `none` unless `allow_unredacted` is set — the same gate
/// `.mtrace` export uses. No raw payload text is ever included: derived
/// facts and tool versions never carry it (see `mcptracer-intel`'s crate
/// docs), so this check is defense-in-depth, not a response to anything
/// actually unsafe in the current corpus.
pub fn build_corpus(
    store: &Store,
    allow_unredacted: bool,
) -> Result<Vec<(SearchDocument, SparseVector)>> {
    let session_ids = store.all_session_ids()?;
    let mut corpus = Vec::new();

    for session_id in &session_ids {
        let summary = store.get_session_summary(session_id)?;
        if summary.redaction_policy == "none" && !allow_unredacted {
            bail!(
                "session {} was recorded with redaction policy 'none'; pass --allow-unredacted to include it in the semantic index, or re-record with --redact default",
                summary.id
            );
        }
        let text = format!(
            "session {} client {} transport {}",
            summary.id, summary.client, summary.transport
        );
        corpus.push((
            SearchDocument {
                id: format!("session:{}", summary.id),
                kind: "session",
                session_id: Some(summary.id.clone()),
                summary: text.clone(),
            },
            embed_text(&text),
        ));
    }

    let tool_versions = store.list_tool_versions()?;
    let edges = store.list_memory_edges(None)?;
    let drifted: std::collections::HashSet<String> = edges
        .iter()
        .filter(|edge| edge.edge_type == "version_supersedes")
        .flat_map(|edge| [edge.from_key.clone(), edge.to_key.clone()])
        .collect();

    for version in &tool_versions {
        let description_hash = version.description_hash.as_deref().unwrap_or("");
        let schema_hash = version.schema_hash.as_deref().unwrap_or("");
        let contract_hash = version.contract_hash.as_deref().unwrap_or("");
        let key = format!(
            "{}::{}::{description_hash}::{schema_hash}::{contract_hash}",
            version.server_key, version.tool_name
        );
        let mut text = format!("tool {} version", version.tool_name);
        if drifted.contains(&key) {
            text.push_str(
                " description changed schema drift version supersedes security finding update",
            );
        }
        corpus.push((
            SearchDocument {
                id: format!("toolversion:{key}"),
                kind: "tool_version",
                session_id: Some(version.first_session_id.clone()),
                summary: text.clone(),
            },
            embed_text(&text),
        ));
    }

    Ok(corpus)
}

pub fn search(
    corpus: &[(SearchDocument, SparseVector)],
    query: &str,
    limit: usize,
) -> Vec<SearchHit> {
    let query_vector = expand_query(query);
    let mut hits: Vec<SearchHit> = corpus
        .iter()
        .map(|(document, vector)| SearchHit {
            document: document.clone(),
            score: cosine_similarity(&query_vector, vector),
            provider: LOCAL_PROVIDER,
        })
        .filter(|hit| hit.score > 0.0)
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    hits
}

#[cfg(test)]
mod tests {
    use mcptracer_storage::{MemoryEdgeRecord, ToolVersionObservationRecord, ToolVersionRecord};

    use super::*;

    #[test]
    fn identical_text_has_cosine_similarity_one() {
        let a = embed_text("tool echo version description changed");
        let b = embed_text("tool echo version description changed");
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn disjoint_text_has_cosine_similarity_zero() {
        let a = embed_text("alpha beta");
        let b = embed_text("gamma delta");
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn query_expansion_adds_domain_keywords() {
        let expanded = expand_query("rug pull");
        let drift_doc = embed_text("description changed schema drift supersedes");
        assert!(cosine_similarity(&expanded, &drift_doc) > 0.0);
    }

    fn seed_drifted_tool(store: &mut Store) {
        let session_a = store.create_session("baseline", "cmd", "stdio", 0).unwrap();
        store
            .set_redaction_policy(&session_a, "default", &[])
            .unwrap();
        store.close_session(&session_a, 1).unwrap();
        let session_b = store.create_session("changed", "cmd", "stdio", 10).unwrap();
        store
            .set_redaction_policy(&session_b, "default", &[])
            .unwrap();
        store.close_session(&session_b, 11).unwrap();

        let old_version = ToolVersionRecord {
            server_key: "srv".to_string(),
            tool_name: "echo".to_string(),
            description_hash: Some("old-hash".to_string()),
            schema_hash: Some("schema".to_string()),
            contract_hash: Some("contract".to_string()),
            description_redacted: None,
            input_schema_redacted: None,
            first_session_id: session_a.clone(),
            first_seen_at_ns: 0,
            last_seen_at_ns: 0,
        };
        let old_observation = ToolVersionObservationRecord {
            session_id: session_a.clone(),
            server_key: "srv".to_string(),
            tool_name: "echo".to_string(),
            version_key: "srv::echo::old-hash::schema::contract".to_string(),
            description_hash: Some("old-hash".to_string()),
            schema_hash: Some("schema".to_string()),
            contract_hash: Some("contract".to_string()),
            seq: Some(0),
            observed_at_ns: 0,
        };
        store
            .rebuild_memory_for_session(&session_a, &[], &[], &[old_version], &[old_observation])
            .unwrap();

        let new_version = ToolVersionRecord {
            server_key: "srv".to_string(),
            tool_name: "echo".to_string(),
            description_hash: Some("new-hash".to_string()),
            schema_hash: Some("schema".to_string()),
            contract_hash: Some("contract".to_string()),
            description_redacted: None,
            input_schema_redacted: None,
            first_session_id: session_b.clone(),
            first_seen_at_ns: 10,
            last_seen_at_ns: 10,
        };
        let new_observation = ToolVersionObservationRecord {
            session_id: session_b.clone(),
            server_key: "srv".to_string(),
            tool_name: "echo".to_string(),
            version_key: "srv::echo::new-hash::schema::contract".to_string(),
            description_hash: Some("new-hash".to_string()),
            schema_hash: Some("schema".to_string()),
            contract_hash: Some("contract".to_string()),
            seq: Some(0),
            observed_at_ns: 10,
        };
        store
            .rebuild_memory_for_session(&session_b, &[], &[], &[new_version], &[new_observation])
            .unwrap();

        store
            .replace_supersession_edges(&[MemoryEdgeRecord {
                edge_type: "version_supersedes".to_string(),
                from_type: "tool_version".to_string(),
                from_key: "srv::echo::new-hash::schema::contract".to_string(),
                to_type: "tool_version".to_string(),
                to_key: "srv::echo::old-hash::schema::contract".to_string(),
                value_json: "{}".to_string(),
                session_id: None,
                source: "test".to_string(),
                observed_at_ns: 10,
                created_at_ns: 10,
            }])
            .unwrap();
    }

    #[test]
    fn rug_pull_query_finds_tool_description_drift() {
        let mut store = Store::open_in_memory().unwrap();
        seed_drifted_tool(&mut store);

        let corpus = build_corpus(&store, false).unwrap();
        let hits = search(&corpus, "rug pull", 5);
        let drifted_versions: Vec<_> = corpus
            .iter()
            .filter(|(document, _)| document.kind == "tool_version")
            .collect();

        assert!(!hits.is_empty(), "expected at least one hit");
        assert_eq!(drifted_versions.len(), 2);
        assert!(drifted_versions.iter().all(|(document, _)| {
            document
                .summary
                .contains("description changed schema drift")
        }));
        assert!(hits
            .iter()
            .any(|hit| hit.document.kind == "tool_version" && hit.document.id.contains("echo")));
        assert_eq!(hits[0].provider, LOCAL_PROVIDER);
    }

    #[test]
    fn unrelated_query_does_not_match_drifted_tool() {
        let mut store = Store::open_in_memory().unwrap();
        seed_drifted_tool(&mut store);

        let corpus = build_corpus(&store, false).unwrap();
        let hits = search(&corpus, "banana smoothie recipe", 5);

        assert!(!hits.iter().any(|hit| hit.document.id.contains("echo")));
    }

    #[test]
    fn unredacted_session_refuses_without_override() {
        let store = Store::open_in_memory().unwrap();
        let session_id = store.create_session("plain", "cmd", "stdio", 0).unwrap();
        store.close_session(&session_id, 1).unwrap();

        let refused = build_corpus(&store, false);
        assert!(refused.is_err());
        assert!(refused
            .unwrap_err()
            .to_string()
            .contains("--allow-unredacted"));

        let allowed = build_corpus(&store, true);
        assert!(allowed.is_ok());
    }
}
