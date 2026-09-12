use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use mcptracer_model::manifest::EvidenceManifest;
use mcptracer_storage::mtrace;

/// Exit code when a checked digest does not match the manifest, or a
/// manifest-recorded input was not verified and `--allow-partial` was not
/// given.
const EXIT_MISMATCH: i32 = 1;

#[derive(Args)]
pub struct VerifyArgs {
    /// Evidence manifest JSON file to verify (written by `assert --manifest`).
    pub manifest: PathBuf,

    /// Local `.mtrace` artifact to check against the manifest's recorded
    /// artifact digest.
    #[arg(long)]
    pub artifact: PathBuf,

    /// Local `.mtrace` artifact to check against the manifest's recorded
    /// baseline digest. Required if the manifest records one, unless
    /// --allow-partial is given.
    #[arg(long)]
    pub baseline: Option<PathBuf>,

    /// Local assertion TOML file to check against the manifest's recorded
    /// assertion-spec digest. Required if the manifest records one, unless
    /// --allow-partial is given.
    #[arg(long = "assert-spec")]
    pub assert_spec: Option<PathBuf>,

    /// Allow verifying fewer inputs than the manifest recorded (e.g. an
    /// artifact-only check when the manifest also has a baseline or
    /// assertion-spec digest). Without this flag, a manifest-recorded digest
    /// with no corresponding file is a hard error: verification must be
    /// complete by default, and a partial check must be requested
    /// explicitly rather than silently passing.
    #[arg(long)]
    pub allow_partial: bool,
}

pub async fn run(args: VerifyArgs) -> Result<()> {
    let manifest_json = std::fs::read_to_string(&args.manifest)
        .with_context(|| format!("failed to read {}", args.manifest.display()))?;
    // from_json validates as part of parsing: schema version, signed must be
    // false, exactly one of baseline/assertion_spec, and every digest is a
    // well-formed 64-char lowercase hex SHA-256. A manifest that fails any
    // of these is rejected outright, before any file comparison happens.
    let manifest = EvidenceManifest::from_json(&manifest_json).map_err(|error| {
        anyhow!(
            "{} is not a valid evidence manifest: {error}",
            args.manifest.display()
        )
    })?;

    // Enforced by EvidenceManifest::validate() above; restated here as a
    // hard invariant so nothing below this line can ever be reached with
    // signed: true, however this function evolves.
    assert!(
        !manifest.signed,
        "EvidenceManifest::from_json must reject signed: true before returning"
    );

    if manifest.baseline.is_some() && args.baseline.is_none() && !args.allow_partial {
        bail!(
            "this manifest records a baseline digest but --baseline was not given; pass \
             --baseline <path> to check it, or --allow-partial to explicitly skip it"
        );
    }
    if manifest.assertion_spec.is_some() && args.assert_spec.is_none() && !args.allow_partial {
        bail!(
            "this manifest records an assertion-spec digest but --assert-spec was not given; \
             pass --assert-spec <path> to check it, or --allow-partial to explicitly skip it"
        );
    }
    if (args.baseline.is_some() && manifest.baseline.is_none())
        || (args.assert_spec.is_some() && manifest.assertion_spec.is_none())
    {
        bail!("a file was given to check against a digest this manifest does not record");
    }

    let mut all_matched = true;
    let mut partial = false;

    let artifact_document = mtrace::read_file(&args.artifact, false)
        .with_context(|| format!("failed to read {}", args.artifact.display()))?;
    let artifact_digest = mtrace::canonical_digest(&artifact_document);
    if manifest.artifact.sha256 == artifact_digest {
        println!("artifact         MATCH     {}", args.artifact.display());
    } else {
        all_matched = false;
        println!("artifact         MISMATCH  {}", args.artifact.display());
        println!("  manifest: {}", manifest.artifact.sha256);
        println!("  actual:   {artifact_digest}");
    }

    match (&manifest.baseline, &args.baseline) {
        (Some(expected), Some(path)) => {
            let document = mtrace::read_file(path, false)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let digest = mtrace::canonical_digest(&document);
            if expected.sha256 == digest {
                println!("baseline         MATCH     {}", path.display());
            } else {
                all_matched = false;
                println!("baseline         MISMATCH  {}", path.display());
                println!("  manifest: {}", expected.sha256);
                println!("  actual:   {digest}");
            }
        }
        (Some(_), None) => {
            partial = true;
            println!(
                "baseline         SKIPPED   (--allow-partial; manifest records a baseline digest that was not checked)"
            );
        }
        (None, _) => {}
    }

    match (&manifest.assertion_spec, &args.assert_spec) {
        (Some(expected), Some(path)) => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            if expected.matches(&bytes) {
                println!("assertion_spec   MATCH     {}", path.display());
            } else {
                all_matched = false;
                println!("assertion_spec   MISMATCH  {}", path.display());
            }
        }
        (Some(_), None) => {
            partial = true;
            println!(
                "assertion_spec   SKIPPED   (--allow-partial; manifest records an assertion-spec digest that was not checked)"
            );
        }
        (None, _) => {}
    }

    let outcome = match manifest.outcome {
        mcptracer_model::manifest::Outcome::Pass => "pass",
        mcptracer_model::manifest::Outcome::Fail => "fail",
    };
    println!();
    if partial {
        println!(
            "WARNING: partial verification — not every digest this manifest records was checked."
        );
    }
    println!("recorded outcome: {outcome}");
    println!(
        "mcptracer version at generation: {}",
        manifest.mcptracer_version
    );
    // manifest.signed is guaranteed false at this point (asserted above), so
    // this is the only reachable branch. Structured as an if/else on the
    // real field, rather than an unconditional print, so a future change to
    // the signed invariant fails loudly here instead of this message
    // silently going stale.
    if manifest.signed {
        unreachable!("signed: true was rejected by EvidenceManifest::from_json");
    } else {
        println!(
            "signature: NONE (unsigned manifest) — a digest match only proves the checked \
             file's content matches what this manifest recorded; it does not prove the \
             manifest itself was not altered alongside a tampered artifact. See \
             docs/spec/artifact-verification.md."
        );
    }

    // `partial` can only be true here if --allow-partial was given: the
    // guards above already turned a manifest-recorded digest with no
    // corresponding file into a hard error otherwise. So a partial run
    // that fully matched what it did check still exits 0 -- that is the
    // whole point of asking for it explicitly.
    if !all_matched {
        std::process::exit(EXIT_MISMATCH);
    }
    Ok(())
}
