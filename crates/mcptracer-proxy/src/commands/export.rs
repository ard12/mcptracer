use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::Args;
use mcptracer_storage::mtrace::MtraceDocument;
use mcptracer_storage::{mtrace, Store};

#[derive(Args)]
pub struct ExportArgs {
    /// Session id (or unique prefix) to export.
    pub session_id: String,

    /// New `.mtrace` artifact path. Existing files are never overwritten.
    #[arg(long)]
    pub out: PathBuf,

    /// Upgrade an unredacted source session to the default stored-copy
    /// redaction policy before writing the artifact.
    #[arg(long, value_name = "POLICY")]
    pub redact: Option<String>,

    /// Explicitly allow export of a session recorded without redaction.
    #[arg(long)]
    pub allow_unredacted: bool,

    /// Allow export even when the pre-export sensitive-content lint detects
    /// bearer tokens, PEM blocks, URL query secrets, or command-embedded
    /// secrets that key-name redaction cannot catch. A loud warning is printed
    /// to stderr listing each finding's JSON pointer and category; the actual
    /// secret values are never echoed.
    #[arg(long)]
    pub allow_sensitive_content: bool,
}

pub async fn run(args: ExportArgs, db_path: PathBuf) -> Result<()> {
    let store = Store::open(&db_path)?;
    let (_document, bytes) = build_validated_artifact(
        &store,
        &args.session_id,
        args.redact.as_deref(),
        args.allow_unredacted,
        args.allow_sensitive_content,
    )?;

    mtrace::write_file(&args.out, &bytes)?;

    println!(
        "Exported session {} to {}",
        args.session_id,
        args.out.display()
    );
    Ok(())
}

/// Export `session_id` to a validated, encoded `.mtrace` artifact, applying
/// the exact same redaction-upgrade and pre-export sensitive-content-lint
/// safety rules `export` enforces (T-63) — shared so `registry push` can't
/// silently apply a weaker rule than a plain local `export` would. Returns
/// the decoded document too, since callers that need its canonical digest
/// (e.g. to address a registry upload) would otherwise have to re-decode
/// the bytes they just produced.
pub fn build_validated_artifact(
    store: &Store,
    session_id: &str,
    redact: Option<&str>,
    allow_unredacted: bool,
    allow_sensitive_content: bool,
) -> Result<(MtraceDocument, Vec<u8>)> {
    let force_default_redaction = match redact {
        None => false,
        Some("default") => true,
        Some(value) => bail!("unsupported export redaction policy: {value} (expected default)"),
    };
    if force_default_redaction && allow_unredacted {
        bail!("--redact default cannot be combined with --allow-unredacted");
    }

    let source_policy = store.get_redaction_policy(session_id)?;
    if allow_unredacted && source_policy == "none" {
        eprintln!(
            "[mcptracer] WARNING: exporting an unredacted session; the artifact may contain secrets"
        );
    }
    let document = store.export_mtrace_document(
        session_id,
        mtrace::ExportOptions {
            force_default_redaction,
            allow_unredacted,
        },
    )?;

    // Pre-export sensitive-content lint (T-63), run over the document's
    // final post-redaction payloads before it's compressed. Key-name
    // redaction cannot catch bearer tokens in free-text fields, PEM blocks,
    // or URL query secrets — this lint flags those patterns without echoing
    // values.
    let all_findings = mtrace::lint_sensitive_content(&document);

    if !all_findings.is_empty() {
        if allow_sensitive_content {
            eprintln!(
                "[mcptracer] WARNING: sensitive content detected in export \
                 — proceeding because --allow-sensitive-content was passed."
            );
            eprintln!("[mcptracer] Findings (pointer: category):");
            for f in &all_findings {
                eprintln!("  {}: {}", f.pointer, f.category.as_str());
            }
        } else {
            eprintln!("[mcptracer] ERROR: sensitive content detected in export artifact.");
            eprintln!("[mcptracer] Findings (pointer: category):");
            for f in &all_findings {
                eprintln!("  {}: {}", f.pointer, f.category.as_str());
            }
            bail!(
                "export aborted: {} sensitive-content finding(s) detected \
                 (key-name redaction cannot remove these). \
                 Use --allow-sensitive-content to override with a warning.",
                all_findings.len()
            );
        }
    }

    let bytes = mtrace::encode(&document)?;
    Ok((document, bytes))
}
