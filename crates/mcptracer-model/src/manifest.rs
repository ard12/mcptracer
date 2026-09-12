//! Evidence manifest (T-73, hardened in T-73A): ties a `.mtrace` artifact's
//! canonical content digest to the outcome of an `assert` comparison, so a
//! later, fully offline pass — no database, no network — can confirm the
//! exact evidence that produced a recorded pass/fail. See
//! `docs/spec/artifact-verification.md` for the full contract, including
//! why `signed` is always `false` in this version.
//!
//! `from_json` is the trust boundary: it always validates before returning,
//! so every caller — `verify`, tests, anything else — gets the same
//! protection against a malformed or hostile manifest without having to
//! remember to call `validate()` separately.

use serde::{Deserialize, Serialize};

/// Bump on any breaking change to this schema. Verifiers must reject a
/// manifest whose `schema_version` they do not understand rather than
/// guessing at a compatible interpretation.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pass,
    Fail,
}

/// One recorded digest. Deliberately holds only the hash, not a filesystem
/// path: the machine that later verifies a manifest is very often not the
/// machine that generated it, so a recorded path would be meaningless or
/// actively misleading. The verifier supplies its own local path to the
/// file it wants to check against this digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestDigest {
    pub sha256: String,
}

impl ManifestDigest {
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            sha256: sha256_hex(bytes),
        }
    }

    pub fn matches(&self, bytes: &[u8]) -> bool {
        self.sha256 == sha256_hex(bytes)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceManifest {
    pub schema_version: u32,
    pub mcptracer_version: String,
    pub generated_at_ns: i64,
    /// Canonical content digest (`mcptracer_storage::mtrace::canonical_digest`)
    /// of the session that was evaluated.
    pub artifact: ManifestDigest,
    /// Present only for golden-snapshot mode (`assert --golden`): the same
    /// canonical content digest, computed over the golden session. Exactly
    /// one of `baseline`/`assertion_spec` must be present — see `validate`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub baseline: Option<ManifestDigest>,
    /// Present only for rule mode (`assert --spec`): a plain SHA-256 over
    /// the assertion TOML file's raw bytes, not the canonical-document
    /// digest (an assertion spec isn't a `.mtrace` document).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub assertion_spec: Option<ManifestDigest>,
    pub outcome: Outcome,
    /// Always `false` in this schema version: no signature scheme is
    /// implemented yet. A digest match on its own proves the checked file's
    /// content matches what this manifest recorded — it does not prove the
    /// manifest itself is trustworthy, since anyone who can edit the
    /// manifest can also recompute a matching digest for altered content.
    /// That guarantee needs a signature over the digest, tying it to a key
    /// the verifier already trusts, which is out of scope here.
    ///
    /// `validate()` rejects any schema-v1 manifest with `signed: true` —
    /// this build has no signature-checking code, so accepting that claim
    /// at face value would let a manifest assert a guarantee nothing here
    /// actually checks. Never gate "signature: verified" output on this
    /// field without first calling `validate()` (which `from_json` always
    /// does) — see `mcptracer verify`.
    pub signed: bool,
}

impl EvidenceManifest {
    pub fn to_json_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// Parse and validate in one step. This is the only supported way to
    /// load a manifest from untrusted input — there is deliberately no
    /// "parse without validating" entry point, so a caller can't
    /// accidentally skip the checks below.
    pub fn from_json(input: &str) -> Result<Self, String> {
        let manifest: Self = serde_json::from_str(input).map_err(|error| error.to_string())?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Reject a manifest whose invariants a hostile or corrupted file could
    /// violate even though it deserializes fine:
    /// - `schema_version` must be the one version this build understands.
    /// - `signed` must be `false` (see the field doc comment).
    /// - Exactly one of `baseline`/`assertion_spec` — matching `assert`'s own
    ///   `--golden`/`--spec` mutual exclusivity; a manifest claiming both (or
    ///   neither) does not correspond to any real `assert` invocation.
    /// - Every recorded digest must be exactly 64 lowercase hex characters —
    ///   the exact shape `ManifestDigest::of` always produces. Anything else
    ///   cannot possibly equal a real digest, so it signals a malformed or
    ///   tampered file rather than a legitimate one that just happens not to
    ///   match.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "unsupported evidence manifest schema version: {} (supported: {SCHEMA_VERSION})",
                self.schema_version
            ));
        }
        if self.signed {
            return Err(
                "schema version 1 evidence manifests must not claim signed: true — this build \
                 has no signature-verification capability, so accepting that claim would let a \
                 manifest assert a guarantee nothing here actually checks"
                    .to_string(),
            );
        }
        match (&self.baseline, &self.assertion_spec) {
            (Some(_), Some(_)) => {
                return Err(
                    "an evidence manifest must not record both a baseline digest and an \
                     assertion-spec digest (assert --golden and --spec are mutually exclusive)"
                        .to_string(),
                )
            }
            (None, None) => {
                return Err(
                    "an evidence manifest must record exactly one of a baseline digest or an \
                     assertion-spec digest"
                        .to_string(),
                )
            }
            _ => {}
        }
        validate_digest(&self.artifact, "artifact")?;
        if let Some(baseline) = &self.baseline {
            validate_digest(baseline, "baseline")?;
        }
        if let Some(assertion_spec) = &self.assertion_spec {
            validate_digest(assertion_spec, "assertion_spec")?;
        }
        Ok(())
    }
}

fn validate_digest(digest: &ManifestDigest, field: &str) -> Result<(), String> {
    if !is_valid_sha256_hex(&digest.sha256) {
        return Err(format!(
            "{field} digest is not a 64-character lowercase hex SHA-256 value: {:?}",
            digest.sha256
        ));
    }
    Ok(())
}

fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> EvidenceManifest {
        EvidenceManifest {
            schema_version: SCHEMA_VERSION,
            mcptracer_version: "0.2.0".to_string(),
            generated_at_ns: 1_000,
            artifact: ManifestDigest::of(b"artifact content"),
            baseline: None,
            assertion_spec: Some(ManifestDigest::of(b"[[assertion]]\nkind = \"ok\"\n")),
            outcome: Outcome::Pass,
            signed: false,
        }
    }

    #[test]
    fn manifest_digest_matches_the_exact_bytes_it_was_built_from() {
        let digest = ManifestDigest::of(b"hello");
        assert!(digest.matches(b"hello"));
        assert!(!digest.matches(b"hello!"));
    }

    #[test]
    fn json_round_trip_preserves_every_field() {
        let source = manifest();
        let json = source.to_json_pretty().unwrap();
        let decoded = EvidenceManifest::from_json(&json).unwrap();

        assert_eq!(decoded.schema_version, source.schema_version);
        assert_eq!(decoded.artifact, source.artifact);
        assert_eq!(decoded.baseline, source.baseline);
        assert_eq!(decoded.assertion_spec, source.assertion_spec);
        assert_eq!(decoded.outcome, source.outcome);
        assert_eq!(decoded.signed, source.signed);
    }

    #[test]
    fn omitted_optional_digests_are_absent_from_the_json_not_null() {
        let mut m = manifest();
        m.assertion_spec = None;
        let json = m.to_json_pretty().unwrap();

        assert!(!json.contains("baseline"));
        assert!(!json.contains("assertion_spec"));
    }

    #[test]
    fn signed_is_always_false_in_schema_v1() {
        // Documents the invariant directly: nothing in this module can set
        // signed to true. If a future schema bump adds real signing, this
        // test's name should change alongside it.
        assert!(!manifest().signed);
    }

    // ── T-73A: hostile-manifest rejection ────────────────────────────────

    #[test]
    fn from_json_rejects_signed_true() {
        let mut m = manifest();
        m.signed = true;
        let json = m.to_json_pretty().unwrap();

        let err = EvidenceManifest::from_json(&json).unwrap_err();
        assert!(err.contains("signed: true"), "{err}");
    }

    #[test]
    fn validate_rejects_signed_true_even_when_constructed_directly() {
        // from_json is the normal entry point, but validate() itself must
        // refuse signed: true regardless of how the value was constructed —
        // it is the actual enforcement point from_json delegates to.
        let mut m = manifest();
        m.signed = true;
        assert!(m.validate().is_err());
    }

    #[test]
    fn from_json_rejects_an_unsupported_schema_version() {
        let mut m = manifest();
        m.schema_version = 999;
        let json = m.to_json_pretty().unwrap();

        let err = EvidenceManifest::from_json(&json).unwrap_err();
        assert!(
            err.contains("unsupported evidence manifest schema version"),
            "{err}"
        );
    }

    #[test]
    fn from_json_rejects_both_baseline_and_assertion_spec_present() {
        let mut m = manifest();
        m.baseline = Some(ManifestDigest::of(b"golden"));
        // assertion_spec is already Some(...) in the base fixture.
        let json = m.to_json_pretty().unwrap();

        let err = EvidenceManifest::from_json(&json).unwrap_err();
        assert!(err.contains("not record both"), "{err}");
    }

    #[test]
    fn from_json_rejects_neither_baseline_nor_assertion_spec_present() {
        let mut m = manifest();
        m.assertion_spec = None;
        let json = m.to_json_pretty().unwrap();

        let err = EvidenceManifest::from_json(&json).unwrap_err();
        assert!(err.contains("exactly one"), "{err}");
    }

    #[test]
    fn from_json_rejects_a_too_short_digest() {
        let mut m = manifest();
        m.artifact = ManifestDigest {
            sha256: "abc123".to_string(),
        };
        let json = m.to_json_pretty().unwrap();

        let err = EvidenceManifest::from_json(&json).unwrap_err();
        assert!(err.contains("artifact digest"), "{err}");
    }

    #[test]
    fn from_json_rejects_uppercase_hex_in_a_digest() {
        let mut m = manifest();
        m.artifact = ManifestDigest {
            sha256: "A".repeat(64),
        };
        let json = m.to_json_pretty().unwrap();

        assert!(EvidenceManifest::from_json(&json).is_err());
    }

    #[test]
    fn from_json_rejects_non_hex_characters_in_a_digest() {
        let mut m = manifest();
        m.artifact = ManifestDigest {
            sha256: "g".repeat(64),
        };
        let json = m.to_json_pretty().unwrap();

        assert!(EvidenceManifest::from_json(&json).is_err());
    }

    #[test]
    fn from_json_accepts_a_well_formed_manifest() {
        let json = manifest().to_json_pretty().unwrap();
        assert!(EvidenceManifest::from_json(&json).is_ok());
    }
}
