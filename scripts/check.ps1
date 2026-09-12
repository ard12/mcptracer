# Full verification gate. Run this before opening a PR — CI runs the same steps.
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

# Gates not run because their tool is missing. This script printed
# "All checks passed" even when it had skipped some, which is exactly the kind
# of gate that reports healthy while proving less than it appears to. Pass
# -Strict (or set MCPTRACER_CHECK_STRICT=1) to treat a skip as a failure,
# which is what a release candidate should use.
$Skipped = New-Object System.Collections.Generic.List[string]
$Strict = ($env:MCPTRACER_CHECK_STRICT -eq "1") -or ($args -contains "-Strict")

function Add-Skip {
    param([string]$Label, [string]$Hint)
    $Skipped.Add($Label)
    Write-Host "    SKIPPED: $Label ($Hint)"
}

function Invoke-Gate {
    param([string]$Label, [scriptblock]$Command)
    Write-Host "==> $Label"
    & $Command
    if ($LASTEXITCODE -ne 0) {
        Write-Error "$Label failed (exit $LASTEXITCODE)"
        exit $LASTEXITCODE
    }
}

Invoke-Gate "cargo fmt --all -- --check" { cargo fmt --all -- --check }
Invoke-Gate "cargo test --workspace --all-features --locked" { cargo test --workspace --all-features --locked }
Invoke-Gate "cargo clippy --workspace --all-targets --all-features --locked -- -D warnings" { cargo clippy --workspace --all-targets --all-features --locked -- -D warnings }
Invoke-Gate "python tests/test_proxy_integration.py" { python tests/test_proxy_integration.py }

Invoke-Gate "scripts/generate-cli-reference.sh" { bash scripts/generate-cli-reference.sh }
Invoke-Gate "scripts/generate-capability-reference.sh" { bash scripts/generate-capability-reference.sh }
Write-Host "==> Checking generated CLI/capability references are not stale"
git diff --exit-code -- docs-site/src/reference/cli.md docs-site/src/reference/capabilities.md
if ($LASTEXITCODE -ne 0) {
    Write-Error "Generated docs are stale. Commit the regenerated files: docs-site/src/reference/cli.md docs-site/src/reference/capabilities.md"
    exit $LASTEXITCODE
}

if (Get-Command mdbook -ErrorAction SilentlyContinue) {
    Invoke-Gate "mdbook build docs-site" { mdbook build docs-site }
} else {
    Write-Host "==> mdbook build docs-site"
    Add-Skip "mdbook build docs-site" "cargo install mdbook --locked --version ^0.5"
}

if (Get-Command cargo-audit -ErrorAction SilentlyContinue) {
    Invoke-Gate "cargo audit --deny warnings" { cargo audit --deny warnings }
} else {
    Write-Host "==> cargo audit --deny warnings"
    Add-Skip "cargo audit --deny warnings" "cargo install cargo-audit --locked"
}

if ($Skipped.Count -eq 0) {
    Write-Host "All checks passed."
    exit 0
}

Write-Host ""
Write-Warning "Checks passed, but $($Skipped.Count) gate(s) did NOT run:"
foreach ($gate in $Skipped) { Write-Warning "  - $gate" }
Write-Warning "CI runs these; a green local run here is weaker evidence than a green CI run."
if ($Strict) {
    Write-Error "-Strict was set: treating skipped gates as failure."
    exit 1
}
