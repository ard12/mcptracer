//! CI-format adapters (T-74, deferred half): JUnit XML and GitHub Actions
//! workflow-command annotations for `assert`, SARIF for `diff`'s security
//! findings. No new crate dependency — JUnit XML and a minimal SARIF 2.1.0
//! log are simple enough to render directly against this project's existing
//! result types, matching how the rest of this codebase already prefers a
//! few dozen lines of direct code over a dependency for something this
//! bounded.

use mcptracer_model::assertions::AssertionResult;
use mcptracer_model::diff::{DiffReport, SecurityFindingKind};

/// Escape the five XML predefined entities. Required for any text or
/// attribute content by the XML spec itself, not just fussy JUnit readers.
fn escape_xml(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Render `assert --spec` results as a single JUnit XML `<testsuite>`, one
/// `<testcase>` per assertion. `suite_name` is typically the checked session
/// id, since JUnit has no other natural identifier here.
pub fn assertion_results_to_junit(results: &[AssertionResult], suite_name: &str) -> String {
    let failures = results.iter().filter(|r| !r.passed).count();
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!(
        "<testsuite name=\"{}\" tests=\"{}\" failures=\"{failures}\">\n",
        escape_xml(suite_name),
        results.len(),
    ));
    for result in results {
        xml.push_str(&format!(
            "  <testcase name=\"{}\" classname=\"mcptracer.assert\">\n",
            escape_xml(&result.description)
        ));
        if !result.passed {
            xml.push_str(&format!(
                "    <failure message=\"{}\"/>\n",
                escape_xml(result.reason.as_deref().unwrap_or("assertion failed"))
            ));
        }
        xml.push_str("  </testcase>\n");
    }
    xml.push_str("</testsuite>\n");
    xml
}

/// Render a single golden-snapshot `assert --golden` outcome as a one-test
/// JUnit `<testsuite>`: golden mode has no per-rule breakdown, the whole
/// comparison either matches or it doesn't.
pub fn golden_snapshot_to_junit(golden_session: &str, report: &DiffReport) -> String {
    let passed = report.is_empty();
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!(
        "<testsuite name=\"golden-snapshot\" tests=\"1\" failures=\"{}\">\n",
        i32::from(!passed)
    ));
    xml.push_str(&format!(
        "  <testcase name=\"matches golden session {}\" classname=\"mcptracer.assert\">\n",
        escape_xml(golden_session)
    ));
    if !passed {
        xml.push_str(&format!(
            "    <failure message=\"{} changed, {} added, {} removed, {} security finding(s)\"/>\n",
            report.changed.len(),
            report.added.len(),
            report.removed.len(),
            report.security.len()
        ));
    }
    xml.push_str("  </testcase>\n</testsuite>\n");
    xml
}

/// Render `assert --spec` results as GitHub Actions workflow-command
/// annotations — one `::notice::`/`::error::` line per assertion, meant to
/// be printed to stdout so GitHub's log UI surfaces them inline on the
/// workflow run. See
/// <https://docs.github.com/actions/using-workflows/workflow-commands-for-github-actions>.
pub fn assertion_results_to_github_annotations(results: &[AssertionResult]) -> String {
    let mut out = String::new();
    for result in results {
        if result.passed {
            out.push_str(&format!(
                "::notice::PASS {}\n",
                github_escape(&result.description)
            ));
        } else {
            out.push_str(&format!(
                "::error::FAIL {} - {}\n",
                github_escape(&result.description),
                github_escape(result.reason.as_deref().unwrap_or("assertion failed"))
            ));
        }
    }
    out
}

/// Render a single golden-snapshot `assert --golden` outcome as one GitHub
/// Actions annotation line, mirroring `golden_snapshot_to_junit`'s
/// one-test-case framing for the same reason: golden mode has no per-rule
/// breakdown to annotate individually.
pub fn golden_snapshot_to_github_annotation(golden_session: &str, report: &DiffReport) -> String {
    if report.is_empty() {
        format!(
            "::notice::PASS snapshot matches golden session {}\n",
            github_escape(golden_session)
        )
    } else {
        format!(
            "::error::FAIL snapshot differs from golden session {} - {} changed, {} added, {} removed, {} security finding(s)\n",
            github_escape(golden_session),
            report.changed.len(),
            report.added.len(),
            report.removed.len(),
            report.security.len()
        )
    }
}

/// GitHub workflow-command message text uses `%25`/`%0D`/`%0A` escaping —
/// a literal newline in the message would otherwise be read as the start of
/// a new workflow command.
fn github_escape(input: &str) -> String {
    input
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Render a `DiffReport`'s security findings as a minimal SARIF 2.1.0 log.
/// Only the security section becomes SARIF "results": SARIF is a findings
/// format, and tool drift (a rug-pull changing a tool's description, schema,
/// or annotations after approval) is the one part of a diff that is a
/// finding in that sense. Ordinary behavioral differences (response/latency/
/// status changes) belong in `diff`'s own text/JSON output, not a
/// static-analysis report.
pub fn diff_security_findings_to_sarif(report: &DiffReport) -> String {
    const RULE_IDS: [&str; 7] = [
        "tool_added",
        "tool_removed",
        "tool_title_changed",
        "tool_description_changed",
        "tool_schema_changed",
        "tool_output_schema_changed",
        "tool_annotations_changed",
    ];
    let rules: Vec<serde_json::Value> = RULE_IDS
        .iter()
        .map(|id| serde_json::json!({"id": id, "name": id}))
        .collect();
    let results: Vec<serde_json::Value> = report
        .security
        .iter()
        .map(|finding| {
            serde_json::json!({
                "ruleId": finding_kind_str(finding.kind),
                "level": "error",
                "message": {"text": finding.detail},
                "properties": {"tool": finding.tool},
            })
        })
        .collect();
    let sarif = serde_json::json!({
        "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "mcptracer",
                    "informationUri": "https://github.com/ard12/mcptracer",
                    "version": env!("CARGO_PKG_VERSION"),
                    "rules": rules,
                }
            },
            "results": results,
        }]
    });
    serde_json::to_string_pretty(&sarif).expect("a serde_json::json! value always serializes")
}

/// Render a `DiffReport` as a GitHub "Create a check run" API payload
/// (T-83) — JSON a CI workflow can post directly, e.g.
/// `gh api repos/{owner}/{repo}/check-runs --input payload.json`. This
/// crate never calls GitHub's API itself: posting needs a GitHub App or
/// installation token with `checks:write`, which this environment has no
/// way to exercise live, so MCPTracer's job ends at producing a correct
/// payload — the same "compute the evidence, let the surrounding workflow
/// call GitHub" boundary the existing composite Action already uses.
///
/// "Link evidence without exposing payloads" (the T-83 accept criterion)
/// is enforced deliberately: the summary includes only *counts* of
/// changed/added/removed exchanges (never their before/after content) plus
/// full detail for `security` findings, which describe tool-*contract*
/// metadata (name/description/schema changes) rather than message
/// traffic — the same distinction `diff_security_findings_to_sarif` already
/// draws. Anyone who needs the actual traffic must follow `details_url` to
/// the artifact itself (typically a registry URL gated by T-82's RBAC), not
/// read it off the check run PR reviewers and CI logs make visible to a
/// wider audience.
pub fn diff_report_to_github_check_run(
    report: &DiffReport,
    head_sha: &str,
    details_url: Option<&str>,
) -> serde_json::Value {
    let passed = report.is_empty();
    let title = if passed {
        "MCPTracer: no meaningful differences".to_string()
    } else {
        format!(
            "MCPTracer: {} changed, {} added, {} removed, {} security finding(s)",
            report.changed.len(),
            report.added.len(),
            report.removed.len(),
            report.security.len()
        )
    };

    let mut summary = if passed {
        "The candidate matches the approved baseline.".to_string()
    } else {
        format!(
            "{} exchange(s) changed, {} added, {} removed, {} security finding(s) versus the approved baseline. Error rate {:.0}% -> {:.0}%.",
            report.changed.len(),
            report.added.len(),
            report.removed.len(),
            report.security.len(),
            report.error_rate_from * 100.0,
            report.error_rate_to * 100.0
        )
    };
    if !report.security.is_empty() {
        summary.push_str("\n\n**Security findings:**\n");
        for finding in &report.security {
            summary.push_str(&format!(
                "- `{}` **{}**: {}\n",
                finding_kind_str(finding.kind),
                finding.tool,
                finding.detail
            ));
        }
    }

    let mut payload = serde_json::json!({
        "name": "mcptracer evidence check",
        "head_sha": head_sha,
        "status": "completed",
        "conclusion": if passed { "success" } else { "failure" },
        "output": {
            "title": title,
            "summary": summary,
        },
    });
    if let Some(url) = details_url {
        payload["details_url"] = serde_json::json!(url);
    }
    payload
}

fn finding_kind_str(kind: SecurityFindingKind) -> &'static str {
    match kind {
        SecurityFindingKind::ToolAdded => "tool_added",
        SecurityFindingKind::ToolRemoved => "tool_removed",
        SecurityFindingKind::ToolTitleChanged => "tool_title_changed",
        SecurityFindingKind::ToolDescriptionChanged => "tool_description_changed",
        SecurityFindingKind::ToolSchemaChanged => "tool_schema_changed",
        SecurityFindingKind::ToolOutputSchemaChanged => "tool_output_schema_changed",
        SecurityFindingKind::ToolAnnotationsChanged => "tool_annotations_changed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcptracer_model::diff::SecurityFinding;

    fn result(description: &str, passed: bool, reason: Option<&str>) -> AssertionResult {
        AssertionResult {
            description: description.to_string(),
            passed,
            reason: reason.map(str::to_string),
        }
    }

    #[test]
    fn junit_counts_tests_and_failures_correctly() {
        let results = vec![
            result("a", true, None),
            result("b", false, Some("nope")),
            result("c", false, Some("<also & bad>")),
        ];
        let xml = assertion_results_to_junit(&results, "sess-1");

        assert!(xml.contains("tests=\"3\""));
        assert!(xml.contains("failures=\"2\""));
        assert_eq!(xml.matches("<testcase").count(), 3);
        assert_eq!(xml.matches("<failure").count(), 2);
    }

    #[test]
    fn junit_escapes_xml_special_characters() {
        let results = vec![result("a & b < c", false, Some("reason with \"quotes\""))];
        let xml = assertion_results_to_junit(&results, "sess-1");

        assert!(xml.contains("a &amp; b &lt; c"));
        assert!(xml.contains("&quot;quotes&quot;"));
        assert!(!xml.contains("a & b < c"));
    }

    #[test]
    fn junit_passing_testcase_has_no_failure_element() {
        let results = vec![result("a", true, None)];
        let xml = assertion_results_to_junit(&results, "sess-1");

        assert!(!xml.contains("<failure"));
    }

    #[test]
    fn golden_snapshot_junit_reports_one_test() {
        let report = DiffReport::default();
        let xml = golden_snapshot_to_junit("golden-id", &report);

        assert!(xml.contains("tests=\"1\""));
        assert!(xml.contains("failures=\"0\""));
        assert!(!xml.contains("<failure"));
    }

    #[test]
    fn github_annotations_use_error_for_failures_and_notice_for_passes() {
        let results = vec![result("a", true, None), result("b", false, Some("bad"))];
        let out = assertion_results_to_github_annotations(&results);

        assert!(out.contains("::notice::PASS a"));
        assert!(out.contains("::error::FAIL b - bad"));
    }

    #[test]
    fn github_annotations_escape_newlines_in_the_message() {
        let results = vec![result("a", false, Some("line one\nline two"))];
        let out = assertion_results_to_github_annotations(&results);

        assert!(out.contains("line one%0Aline two"));
        assert!(!out.contains("line one\nline two"));
    }

    #[test]
    fn golden_snapshot_github_annotation_is_notice_on_pass_error_on_fail() {
        let pass = golden_snapshot_to_github_annotation("golden-id", &DiffReport::default());
        assert!(pass.starts_with("::notice::PASS"));

        let mut failing = DiffReport::default();
        failing.security.push(SecurityFinding {
            kind: SecurityFindingKind::ToolRemoved,
            tool: "echo".to_string(),
            detail: "removed".to_string(),
        });
        let fail = golden_snapshot_to_github_annotation("golden-id", &failing);
        assert!(fail.starts_with("::error::FAIL"));
        assert!(fail.contains("1 security finding"));
    }

    #[test]
    fn sarif_includes_one_result_per_security_finding_with_the_snake_case_rule_id() {
        let mut report = DiffReport::default();
        report.security.push(SecurityFinding {
            kind: SecurityFindingKind::ToolDescriptionChanged,
            tool: "echo".to_string(),
            detail: "description changed".to_string(),
        });
        let sarif = diff_security_findings_to_sarif(&report);
        let parsed: serde_json::Value = serde_json::from_str(&sarif).unwrap();

        assert_eq!(parsed["version"], "2.1.0");
        let results = parsed["runs"][0]["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["ruleId"], "tool_description_changed");
        assert_eq!(results[0]["properties"]["tool"], "echo");
    }

    #[test]
    fn sarif_has_no_results_when_there_are_no_security_findings() {
        let report = DiffReport::default();
        let sarif = diff_security_findings_to_sarif(&report);
        let parsed: serde_json::Value = serde_json::from_str(&sarif).unwrap();

        assert_eq!(parsed["runs"][0]["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn github_check_run_reports_success_for_a_matching_report() {
        let report = DiffReport::default();
        let payload = diff_report_to_github_check_run(&report, "abc123", None);

        assert_eq!(payload["head_sha"], "abc123");
        assert_eq!(payload["status"], "completed");
        assert_eq!(payload["conclusion"], "success");
        assert!(payload.get("details_url").is_none());
    }

    #[test]
    fn github_check_run_reports_failure_and_includes_details_url_when_given() {
        let mut report = DiffReport::default();
        report.added.push("tools/call new_tool#0".to_string());
        let payload = diff_report_to_github_check_run(
            &report,
            "abc123",
            Some("https://registry.example.test/v1/orgs/acme/projects/x/artifacts/deadbeef"),
        );

        assert_eq!(payload["conclusion"], "failure");
        assert_eq!(
            payload["details_url"],
            "https://registry.example.test/v1/orgs/acme/projects/x/artifacts/deadbeef"
        );
    }

    #[test]
    fn github_check_run_summary_never_contains_changed_exchange_response_content() {
        // The whole point of "link evidence without exposing payloads": a
        // changed exchange's actual before/after values must never leak
        // into the check summary PR reviewers and CI logs make visible to
        // a wide audience. Only the count belongs there.
        use mcptracer_model::diff::{AlignedExchangeDiff, ExchangeDelta, PointerDiff};

        let mut report = DiffReport::default();
        report.changed.push(AlignedExchangeDiff {
            key: "tools/call charge_card#0".to_string(),
            deltas: vec![ExchangeDelta::ResponseChanged {
                pointer_diffs: vec![PointerDiff {
                    pointer: "/content/0/text".to_string(),
                    from: Some(serde_json::json!("card ending 4242, secret-looking-value")),
                    to: Some(serde_json::json!("card ending 4242, a-different-secret")),
                }],
            }],
        });

        let payload = diff_report_to_github_check_run(&report, "abc123", None);
        let summary = payload["output"]["summary"].as_str().unwrap();

        assert!(summary.contains("1 exchange(s) changed"));
        assert!(!summary.contains("secret-looking-value"));
        assert!(!summary.contains("a-different-secret"));
        assert!(!summary.contains("charge_card"));
    }

    #[test]
    fn github_check_run_summary_includes_security_finding_detail() {
        // Unlike ordinary changed-exchange content, security findings
        // describe tool *contract* metadata (T-73's rug-pull detection),
        // not message traffic, so their detail is safe and useful to
        // surface directly in the check.
        let mut report = DiffReport::default();
        report.security.push(SecurityFinding {
            kind: SecurityFindingKind::ToolDescriptionChanged,
            tool: "send_email".to_string(),
            detail: "description changed from \"Send an email\" to \"Send an email and BCC compliance@evil.example\"".to_string(),
        });

        let payload = diff_report_to_github_check_run(&report, "abc123", None);
        let summary = payload["output"]["summary"].as_str().unwrap();

        assert!(summary.contains("send_email"));
        assert!(summary.contains("BCC compliance@evil.example"));
    }
}
