use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use mcptracer_model::diff::{diff_sessions, DiffOptions, DiffReport, ExchangeDelta};
use mcptracer_storage::{mtrace, Store};

use crate::ci_formats::{diff_report_to_github_check_run, diff_security_findings_to_sarif};
use crate::session_health::require_healthy_session;

/// Cap on pointer-level diffs printed per exchange in human output.
const MAX_POINTER_DIFFS_SHOWN: usize = 20;

/// Bump alongside any breaking change to `diff --json` / `assert --golden
/// --json`'s shape and publish a new `schemas/diff-report.vN.schema.json`.
/// `DiffReport` itself lives in `mcptracer_model::diff` and has no room for
/// this field, so it is spliced into the serialized JSON here rather than
/// added to the struct.
const DIFF_REPORT_SCHEMA_VERSION: u32 = 3;

#[derive(Args)]
pub struct DiffArgs {
    /// Baseline session id (or unique prefix).
    pub session_a: String,
    /// Session to compare against the baseline.
    pub session_b: String,

    /// Print the structured DiffReport as JSON instead of a human report.
    #[arg(long)]
    pub json: bool,

    /// JSON pointers (comma separated, relative to the response result/error,
    /// e.g. /content/0/text) whose values are ignored.
    #[arg(long, value_delimiter = ',')]
    pub ignore: Vec<String>,

    /// Skip latency comparison entirely.
    #[arg(long)]
    pub ignore_latency: bool,

    /// Report latency deltas only when the relative change exceeds this
    /// percentage (and the absolute change exceeds 1ms).
    #[arg(long, default_value_t = 20.0)]
    pub latency_threshold_pct: f64,

    /// Always exit 0, even when differences are found.
    #[arg(long)]
    pub exit_zero: bool,

    /// Write the security section (tool drift: rug-pull-style description,
    /// schema, or annotation changes) as a SARIF 2.1.0 log (T-74) to this
    /// path, for CI systems that ingest SARIF findings (e.g. GitHub code
    /// scanning). Ordinary behavioral differences are not SARIF findings and
    /// are not included — see `docs/spec/ci-output-contracts.md`. Never
    /// overwrites an existing file.
    #[arg(long, value_name = "PATH")]
    pub sarif: Option<PathBuf>,

    /// Write a GitHub "Create a check run" API payload (T-83) to this path
    /// — JSON a CI workflow can post directly, e.g.
    /// `gh api repos/{owner}/{repo}/check-runs --input payload.json`.
    /// MCPTracer never calls GitHub's API itself; this only produces the
    /// payload. Requires `--github-check-sha`. The summary links evidence
    /// via `--github-check-details-url` rather than embedding response
    /// content — see `docs/spec/ci-output-contracts.md`'s GitHub check-run
    /// section for what is and is not included. Never overwrites an
    /// existing file.
    #[arg(long, value_name = "PATH", requires = "github_check_sha")]
    pub github_check_json: Option<PathBuf>,

    /// The commit SHA the check run applies to (GitHub Actions:
    /// `$GITHUB_SHA`). Required with `--github-check-json`.
    #[arg(long, value_name = "SHA")]
    pub github_check_sha: Option<String>,

    /// A URL the check run links to for the full evidence — typically a
    /// registry artifact URL (T-81/T-82) — rather than embedding traffic
    /// content in the check itself.
    #[arg(long, value_name = "URL")]
    pub github_check_details_url: Option<String>,
}

pub async fn run(args: DiffArgs, db_path: PathBuf) -> Result<()> {
    if !args.latency_threshold_pct.is_finite() || args.latency_threshold_pct < 0.0 {
        anyhow::bail!("--latency-threshold-pct must be a finite non-negative number");
    }

    let store = Store::open(&db_path)?;
    let a = require_healthy_session(&store, &args.session_a, "diff")?;
    let b = require_healthy_session(&store, &args.session_b, "diff")?;

    let mut opts = DiffOptions::default();
    opts.ignore.pointers = args.ignore.clone();
    opts.ignore_latency = args.ignore_latency;
    opts.latency_threshold_pct = args.latency_threshold_pct;

    let report = diff_sessions(&a.messages, &b.messages, &opts);

    if args.json {
        let mut payload = serde_json::to_value(&report)?;
        payload
            .as_object_mut()
            .expect("DiffReport always serializes as a JSON object")
            .insert(
                "schema_version".to_string(),
                serde_json::json!(DIFF_REPORT_SCHEMA_VERSION),
            );
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        print_report(&report);
    }

    if let Some(sarif_path) = &args.sarif {
        let sarif = diff_security_findings_to_sarif(&report);
        mtrace::write_file(sarif_path, sarif.as_bytes())
            .with_context(|| format!("failed to write {}", sarif_path.display()))?;
    }

    if let Some(check_path) = &args.github_check_json {
        let sha = args
            .github_check_sha
            .as_deref()
            .expect("clap enforces --github-check-sha alongside --github-check-json");
        let payload =
            diff_report_to_github_check_run(&report, sha, args.github_check_details_url.as_deref());
        let json = serde_json::to_string_pretty(&payload)?;
        mtrace::write_file(check_path, json.as_bytes())
            .with_context(|| format!("failed to write {}", check_path.display()))?;
    }

    if !report.is_empty() && !args.exit_zero {
        std::process::exit(1);
    }
    Ok(())
}

fn print_report(report: &DiffReport) {
    if report.is_empty() {
        println!("Sessions match: no meaningful differences.");
        return;
    }

    println!(
        "{} changed, {} added, {} removed, {} security finding(s); error rate {:.0}% -> {:.0}%",
        report.changed.len(),
        report.added.len(),
        report.removed.len(),
        report.security.len(),
        report.error_rate_from * 100.0,
        report.error_rate_to * 100.0
    );

    if !report.security.is_empty() {
        println!();
        for finding in &report.security {
            println!(
                "SECURITY [{}] {}: {}",
                security_kind_str(finding.kind),
                finding.tool,
                finding.detail
            );
        }
    }

    if let Some(pod) = &report.point_of_divergence {
        println!();
        println!(
            ">>> POINT OF DIVERGENCE: Step {} ({})",
            pod.step_index, pod.key
        );
        println!("    {}", pod.detail);
    }

    for changed in &report.changed {
        println!();
        println!("~ {}", changed.key);
        for delta in &changed.deltas {
            match delta {
                ExchangeDelta::StatusChanged { from, to } => {
                    println!("    status: {from:?} -> {to:?}");
                }
                ExchangeDelta::ErrorCodeChanged { from, to } => {
                    println!("    error code: {from:?} -> {to:?}");
                }
                ExchangeDelta::RequestChanged { pointer_diffs } => {
                    for diff in pointer_diffs.iter().take(MAX_POINTER_DIFFS_SHOWN) {
                        println!(
                            "    request {}: {} -> {}",
                            if diff.pointer.is_empty() {
                                "(root)"
                            } else {
                                &diff.pointer
                            },
                            render_side(&diff.from),
                            render_side(&diff.to)
                        );
                    }
                    if pointer_diffs.len() > MAX_POINTER_DIFFS_SHOWN {
                        println!(
                            "    ... and {} more request value change(s)",
                            pointer_diffs.len() - MAX_POINTER_DIFFS_SHOWN
                        );
                    }
                }
                ExchangeDelta::ResponseChanged { pointer_diffs } => {
                    for diff in pointer_diffs.iter().take(MAX_POINTER_DIFFS_SHOWN) {
                        println!(
                            "    {}: {} -> {}",
                            if diff.pointer.is_empty() {
                                "(root)"
                            } else {
                                &diff.pointer
                            },
                            render_side(&diff.from),
                            render_side(&diff.to)
                        );
                    }
                    if pointer_diffs.len() > MAX_POINTER_DIFFS_SHOWN {
                        println!(
                            "    ... and {} more value change(s)",
                            pointer_diffs.len() - MAX_POINTER_DIFFS_SHOWN
                        );
                    }
                }
                ExchangeDelta::LatencyChanged {
                    from_ns,
                    to_ns,
                    pct,
                } => {
                    println!(
                        "    latency: {:.1}ms -> {:.1}ms ({:+.0}%)",
                        *from_ns as f64 / 1_000_000.0,
                        *to_ns as f64 / 1_000_000.0,
                        pct
                    );
                }
            }
        }
    }

    for key in &report.added {
        println!("+ only in B: {key}");
    }
    for key in &report.removed {
        println!("- only in A: {key}");
    }
    if !report.tools_added.is_empty() {
        println!("tools added: {}", report.tools_added.join(", "));
    }
    if !report.tools_removed.is_empty() {
        println!("tools removed: {}", report.tools_removed.join(", "));
    }
}

fn security_kind_str(kind: mcptracer_model::diff::SecurityFindingKind) -> &'static str {
    use mcptracer_model::diff::SecurityFindingKind::*;
    match kind {
        ToolAdded => "tool added",
        ToolRemoved => "tool removed",
        ToolTitleChanged => "title changed",
        ToolDescriptionChanged => "description changed",
        ToolSchemaChanged => "schema changed",
        ToolOutputSchemaChanged => "output schema changed",
        ToolAnnotationsChanged => "annotations changed",
    }
}

fn render_side(side: &Option<serde_json::Value>) -> String {
    match side {
        Some(value) => value.to_string(),
        None => "<absent>".to_string(),
    }
}
