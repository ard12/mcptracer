use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Args;
use mcptracer_model::assertions::{evaluate, parse_spec};
use mcptracer_model::diff::{diff_sessions, DiffOptions};
use mcptracer_model::manifest::{EvidenceManifest, ManifestDigest, Outcome, SCHEMA_VERSION};
use mcptracer_storage::{mtrace, Store};

use crate::ci_formats::{
    assertion_results_to_github_annotations, assertion_results_to_junit,
    golden_snapshot_to_github_annotation, golden_snapshot_to_junit,
};
use crate::session_health::require_healthy_session;
use crate::session_writer::now_ns;

/// Exit code for assertion failures (tests failed).
const EXIT_FAIL: i32 = 1;
/// Exit code for spec errors (the tests themselves are broken).
const EXIT_SPEC_ERROR: i32 = 2;

/// Bump alongside any breaking change to `assert --golden --json`'s shape
/// (the same `DiffReport` shape `diff --json` emits) and publish a new
/// `schemas/diff-report.vN.schema.json`.
const DIFF_REPORT_SCHEMA_VERSION: u32 = 3;

/// Bump alongside any breaking change to `assert --spec --json`'s shape and
/// publish a new `schemas/assert-results.vN.schema.json`. v1 was a bare JSON
/// array with no version marker; v2 wraps the same array in an object
/// carrying `schema_version`.
const ASSERT_RESULTS_SCHEMA_VERSION: u32 = 2;

#[derive(Args)]
pub struct AssertArgs {
    /// Session id (or unique prefix) to check.
    pub session_id: String,

    /// TOML assertion spec file (rule mode).
    #[arg(long, required_unless_present = "golden", conflicts_with = "golden")]
    pub spec: Option<PathBuf>,

    /// Golden session id (snapshot mode): pass iff `diff golden session`
    /// reports no meaningful differences. Latency is ignored here — two runs
    /// always jitter; use a `latency` assertion for performance gates.
    #[arg(long)]
    pub golden: Option<String>,

    /// Print results as JSON.
    #[arg(long)]
    pub json: bool,

    /// Write an evidence manifest (T-73) recording this outcome alongside a
    /// canonical content digest of the checked session (and, for
    /// `--golden`, the golden session; for `--spec`, the assertion file) —
    /// so `mcptracer verify` can later confirm offline, without a database,
    /// that a given `.mtrace` export is the exact evidence that produced
    /// this result. Never overwrites an existing file.
    #[arg(long, value_name = "PATH")]
    pub manifest: Option<PathBuf>,

    /// Upgrade an unredacted checked (or golden) session to the default
    /// stored-copy redaction policy before computing its manifest digest.
    /// Only meaningful with --manifest; ignored otherwise.
    #[arg(long, value_name = "POLICY")]
    pub redact: Option<String>,

    /// Explicitly allow computing a manifest digest over an unredacted
    /// session. Only meaningful with --manifest; ignored otherwise.
    #[arg(long)]
    pub allow_unredacted: bool,

    /// Write a JUnit XML report (T-74) to this path, for CI systems that
    /// consume test results as a file (e.g. GitHub Actions' test-reporting
    /// actions, GitLab's `artifacts: reports: junit`). Never overwrites an
    /// existing file.
    #[arg(long, value_name = "PATH")]
    pub junit: Option<PathBuf>,

    /// Print GitHub Actions workflow-command annotations (`::notice::`/
    /// `::error::`) to stdout, one per assertion, so a run's PASS/FAIL
    /// results surface directly in the GitHub Actions log UI.
    #[arg(long)]
    pub github: bool,
}

pub async fn run(args: AssertArgs, db_path: PathBuf) -> Result<()> {
    let store = Store::open(&db_path)?;
    let session = require_healthy_session(&store, &args.session_id, "assert")?;

    if let Some(golden) = &args.golden {
        let golden_session = require_healthy_session(&store, golden, "assert")?;
        let opts = DiffOptions {
            ignore_latency: true,
            ..Default::default()
        };
        let report = diff_sessions(&golden_session.messages, &session.messages, &opts);
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
        } else if report.is_empty() {
            println!("PASS snapshot matches golden session {golden}");
        } else {
            println!(
                "FAIL snapshot differs from golden session {golden}: {} changed, {} added, {} removed, {} security finding(s)",
                report.changed.len(),
                report.added.len(),
                report.removed.len(),
                report.security.len()
            );
            println!(
                "Run `mcptracer diff {golden} {}` for details.",
                args.session_id
            );
        }

        if args.github {
            print!("{}", golden_snapshot_to_github_annotation(golden, &report));
        }
        if let Some(junit_path) = &args.junit {
            let xml = golden_snapshot_to_junit(golden, &report);
            mtrace::write_file(junit_path, xml.as_bytes())
                .with_context(|| format!("failed to write {}", junit_path.display()))?;
        }

        let outcome = if report.is_empty() {
            Outcome::Pass
        } else {
            Outcome::Fail
        };
        if let Some(manifest_path) = &args.manifest {
            write_manifest(
                &store,
                manifest_path,
                &args,
                &args.session_id,
                Some(golden),
                None,
                outcome,
            )?;
        }

        if !report.is_empty() {
            std::process::exit(EXIT_FAIL);
        }
        return Ok(());
    }

    let spec_path = args.spec.as_ref().expect("clap enforces spec xor golden");
    let spec_src = match std::fs::read_to_string(spec_path) {
        Ok(src) => src,
        Err(err) => {
            eprintln!("spec error: cannot read {}: {err}", spec_path.display());
            std::process::exit(EXIT_SPEC_ERROR);
        }
    };
    let spec = match parse_spec(&spec_src) {
        Ok(spec) => spec,
        Err(err) => {
            eprintln!("spec error: {err}");
            std::process::exit(EXIT_SPEC_ERROR);
        }
    };

    let results = evaluate(&spec, &session.messages);
    let failed = results.iter().filter(|r| !r.passed).count();

    if args.json {
        let payload = serde_json::json!({
            "schema_version": ASSERT_RESULTS_SCHEMA_VERSION,
            "results": results,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        for result in &results {
            if result.passed {
                println!("PASS {}", result.description);
            } else {
                println!(
                    "FAIL {} — {}",
                    result.description,
                    result.reason.as_deref().unwrap_or("failed")
                );
            }
        }
        println!("{} passed, {} failed", results.len() - failed, failed);
    }

    if args.github {
        print!("{}", assertion_results_to_github_annotations(&results));
    }
    if let Some(junit_path) = &args.junit {
        let xml = assertion_results_to_junit(&results, &args.session_id);
        mtrace::write_file(junit_path, xml.as_bytes())
            .with_context(|| format!("failed to write {}", junit_path.display()))?;
    }

    let outcome = if failed == 0 {
        Outcome::Pass
    } else {
        Outcome::Fail
    };
    if let Some(manifest_path) = &args.manifest {
        write_manifest(
            &store,
            manifest_path,
            &args,
            &args.session_id,
            None,
            Some(spec_src.as_bytes()),
            outcome,
        )?;
    }

    if failed > 0 {
        std::process::exit(EXIT_FAIL);
    }
    Ok(())
}

/// Build and write the evidence manifest for this run. `artifact_session`
/// and `baseline_session` (golden mode) are digested via the exact same
/// `export_mtrace_document` path `export` uses, so a manifest's recorded
/// digest always matches what a subsequent real `export` of the same
/// session would produce — `--redact`/`--allow-unredacted` apply the same
/// safety rule `export` already enforces rather than opening a
/// redaction-free side door.
#[allow(clippy::too_many_arguments)]
fn write_manifest(
    store: &Store,
    manifest_path: &PathBuf,
    args: &AssertArgs,
    artifact_session: &str,
    baseline_session: Option<&str>,
    assertion_spec: Option<&[u8]>,
    outcome: Outcome,
) -> Result<()> {
    let force_default_redaction = match args.redact.as_deref() {
        None => false,
        Some("default") => true,
        Some(value) => {
            return Err(anyhow!(
                "unsupported manifest redaction policy: {value} (expected default)"
            ))
        }
    };
    if force_default_redaction && args.allow_unredacted {
        return Err(anyhow!(
            "--redact default cannot be combined with --allow-unredacted"
        ));
    }
    let options = mtrace::ExportOptions {
        force_default_redaction,
        allow_unredacted: args.allow_unredacted,
    };

    let artifact_document = store
        .export_mtrace_document(artifact_session, options)
        .context("failed to build the manifest's artifact digest")?;
    let artifact = ManifestDigest {
        sha256: mtrace::canonical_digest(&artifact_document),
    };

    let baseline = baseline_session
        .map(|session_id| -> Result<ManifestDigest> {
            let document = store
                .export_mtrace_document(session_id, options)
                .context("failed to build the manifest's baseline digest")?;
            Ok(ManifestDigest {
                sha256: mtrace::canonical_digest(&document),
            })
        })
        .transpose()?;

    let assertion_spec_digest = assertion_spec.map(ManifestDigest::of);

    let manifest = EvidenceManifest {
        schema_version: SCHEMA_VERSION,
        mcptracer_version: env!("CARGO_PKG_VERSION").to_string(),
        generated_at_ns: now_ns(),
        artifact,
        baseline,
        assertion_spec: assertion_spec_digest,
        outcome,
        signed: false,
    };

    // Defense in depth: validate our own construction before writing it,
    // the same way mtrace::encode validates before compressing. A failure
    // here would be a bug in this function, not hostile input, but it
    // should never produce a manifest `verify` would then reject anyway.
    manifest
        .validate()
        .map_err(|error| anyhow!("internal error building evidence manifest: {error}"))?;

    let json = manifest
        .to_json_pretty()
        .context("failed to serialize evidence manifest")?;
    mtrace::write_file(manifest_path, json.as_bytes())
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;
    eprintln!(
        "[mcptracer] wrote evidence manifest {} (unsigned)",
        manifest_path.display()
    );
    Ok(())
}
