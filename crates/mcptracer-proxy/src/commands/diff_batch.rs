use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Args;
use mcptracer_model::diff::{diff_sessions, DiffOptions, DiffReport};
use mcptracer_storage::Store;
use serde::{Deserialize, Serialize};

use crate::session_health::require_healthy_session;

#[derive(Args)]
pub struct DiffBatchArgs {
    /// JSON or TOML config listing session pairs to compare (selected by
    /// file extension: `.json` or `.toml`).
    pub config: PathBuf,

    /// Exit non-zero if any pair has a meaningful difference. Without this,
    /// diff-batch always reports and exits 0 (a fan-out summary, not a gate).
    #[arg(long)]
    pub fail_on_breaking: bool,

    /// Print the structured per-pair results as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct DiffPair {
    #[serde(default)]
    label: Option<String>,
    baseline: String,
    candidate: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct DiffBatchConfig {
    #[serde(rename = "pair", alias = "pairs", default)]
    pairs: Vec<DiffPair>,
}

#[derive(Debug, Clone, Serialize)]
struct PairResult {
    label: String,
    baseline: String,
    candidate: String,
    changed: bool,
    report: DiffReport,
}

/// Run `diff` over every pair in a config file and aggregate the results —
/// the CI fan-out story for a suite of golden sessions.
pub async fn run(args: DiffBatchArgs, db_path: PathBuf) -> Result<()> {
    let config = parse_config(&args.config)?;
    if config.pairs.is_empty() {
        bail!(
            "diff-batch config {} contains no session pairs",
            args.config.display()
        );
    }

    let store = Store::open(&db_path)?;
    let mut results = Vec::with_capacity(config.pairs.len());
    let mut breaking = 0usize;

    for pair in &config.pairs {
        let label = pair
            .label
            .clone()
            .unwrap_or_else(|| format!("{} vs {}", pair.baseline, pair.candidate));
        let baseline = require_healthy_session(&store, &pair.baseline, "diff-batch")?;
        let candidate = require_healthy_session(&store, &pair.candidate, "diff-batch")?;
        let report = diff_sessions(
            &baseline.messages,
            &candidate.messages,
            &DiffOptions::default(),
        );
        let changed = !report.is_empty();
        if changed {
            breaking += 1;
        }
        results.push(PairResult {
            label,
            baseline: baseline.session_id,
            candidate: candidate.session_id,
            changed,
            report,
        });
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        for result in &results {
            if result.changed {
                println!(
                    "CHANGED {} — {} changed, {} added, {} removed, {} security finding(s)",
                    result.label,
                    result.report.changed.len(),
                    result.report.added.len(),
                    result.report.removed.len(),
                    result.report.security.len()
                );
            } else {
                println!("PASS {}", result.label);
            }
        }
        println!("{} pair(s), {} changed", results.len(), breaking);
    }

    if args.fail_on_breaking && breaking > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn parse_config(path: &Path) -> Result<DiffBatchConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read diff-batch config {}", path.display()))?;
    let is_json = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
    if is_json {
        serde_json::from_str(&text)
            .with_context(|| format!("invalid JSON diff-batch config {}", path.display()))
    } else {
        toml::from_str(&text)
            .with_context(|| format!("invalid TOML diff-batch config {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mcptracer-diff-batch-{unique}-{name}"));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn parses_toml_config_with_pair_alias() {
        let path = write_temp(
            "config.toml",
            "[[pair]]\nlabel = \"one\"\nbaseline = \"a\"\ncandidate = \"b\"\n\n[[pair]]\nbaseline = \"c\"\ncandidate = \"d\"\n",
        );
        let config = parse_config(&path).unwrap();
        assert_eq!(config.pairs.len(), 2);
        assert_eq!(config.pairs[0].label.as_deref(), Some("one"));
        assert_eq!(config.pairs[1].label, None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parses_json_config_with_pairs_key() {
        let path = write_temp(
            "config.json",
            r#"{"pairs": [{"baseline": "a", "candidate": "b"}]}"#,
        );
        let config = parse_config(&path).unwrap();
        assert_eq!(config.pairs.len(), 1);
        assert_eq!(config.pairs[0].baseline, "a");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_malformed_config() {
        let path = write_temp("bad.toml", "not toml at [[");
        assert!(parse_config(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
