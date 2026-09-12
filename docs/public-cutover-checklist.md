# Public source cutover checklist

This checklist verifies a selected MCPTracer public snapshot without creating a
repository, changing settings, pushing, tagging, or publishing a release.

## Before cutover

Record the exact pair that was reviewed:

- authoritative source commit;
- generated public-only commit;
- export policy hash from `OSS_EXPORT_MANIFEST.json`;
- source-only preview status and any platform evidence still pending.

Verify the generated directory before it is committed:

```bash
python scripts/verify_oss_export.py . \
  --expected-source-sha <source-commit>
python scripts/source_preview_smoke.py . \
  --expected-source-sha <source-commit>
```

The first command validates manifest structure, safe paths, every recorded
SHA-256 value, complete file coverage, and recorded exclusions. It is integrity
evidence only; hashes do not prove ownership or trusted authorship. The second
command installs from the candidate into a temporary prefix and runs the
installed binary and the complete rug-pull tutorial.

## Repository settings

After an approved public cutover, inspect the repository settings in GitHub's
web interface. Do not guess or automate mutation payloads from this document.

1. Confirm the repository is public and `main` is the default branch.
2. Configure a branch ruleset appropriate to the generated-snapshot workflow.
   Block force pushes and deletion. If required status checks are enabled, use
   the exact job names below and configure any maintainer bypass deliberately.
3. Enable private vulnerability reporting and confirm that the **Report a
   vulnerability** link is available under the Security tab.
4. Confirm dependency graph and alert settings match the maintainer's chosen
   security policy.
5. Keep Pages, tags, packages, and release publishing disabled unless each is
   approved and verified separately.

The expected checks from `.github/workflows/ci.yml` are:

- `Test (ubuntu-latest)`
- `Test (windows-latest)`
- `Test (macos-latest)`
- `Lint`
- `Security audit`
- `Docs build`
- `Real-SDK compatibility matrix (ubuntu-latest)`
- `Real-SDK compatibility matrix (windows-latest)`
- `Real-SDK compatibility matrix (macos-latest)`
- `MSRV (Rust 1.85) (ubuntu-latest)`
- `MSRV (Rust 1.85) (windows-latest)`
- `MSRV (Rust 1.85) (macos-latest)`

Use GitHub's current documentation when configuring settings:

- [Repository rulesets](https://docs.github.com/en/rest/repos/rules)
- [Workflow runs](https://docs.github.com/en/rest/actions/workflow-runs)
- [Repository metadata](https://docs.github.com/en/rest/repos/repos)
- [Private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/working-with-repository-security-advisories/configuring-private-vulnerability-reporting-for-a-repository)

## Read-only verification

Run the anonymous verifier with the approved commit pair:

```bash
python scripts/verify_public_cutover.py ard12/mcptracer \
  --expected-public-commit <public-commit> \
  --expected-source-sha <source-commit> \
  --run-outsider-smoke \
  --report-json public-cutover.json
```

The command sends no GitHub authorization header. Its disposable clone removes
`GH_TOKEN`, `GITHUB_TOKEN`, credential helpers, interactive prompts, and the
user/global Git configuration from that child process. It does not alter the
user's Git or GitHub CLI configuration.

Exit meanings:

- `0`: all observable checks passed;
- `1`: a visible state mismatched or a check failed;
- `2`: the repository, CI, or security-reporting evidence is pending.

A 404 is reported as absent or anonymously inaccessible. It is never treated as
proof that a repository name is globally available. Private vulnerability
reporting may be unreadable anonymously; in that case the verifier leaves the
item pending and the maintainer must verify it in repository security settings.

## Evidence to retain

Keep the JSON report, public commit URL, CI run URL, UTC check timestamp, and
the hosted OS results. Compatibility claims must follow executed evidence: a
configured matrix is not proof that a job ran. If the public commit changes,
repeat the entire check against the new commit pair.
