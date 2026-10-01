# Install the mcptracer CLI from a GitHub release.
#
#   irm https://raw.githubusercontent.com/ard12/mcptracer/main/scripts/install.ps1 | iex
#
# Env vars:
#   MCPTRACER_VERSION      release tag to install, e.g. "v0.2.0" or "0.2.0"
#                           (default: latest release)
#   MCPTRACER_INSTALL_DIR  directory to install the binary into
#                           (default: "$env:LOCALAPPDATA\mcptracer\bin")
$ErrorActionPreference = "Stop"

$Repo = "ard12/mcptracer"
$BinName = "mcptracer"
$Target = "x86_64-pc-windows-msvc"
$InstallDir = if ($env:MCPTRACER_INSTALL_DIR) { $env:MCPTRACER_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "mcptracer\bin" }

function Write-InstallLog($msg) {
    Write-Host "[mcptracer-install] $msg"
}

if ([Environment]::Is64BitOperatingSystem -eq $false) {
    throw "mcptracer only publishes 64-bit Windows binaries (x86_64-pc-windows-msvc)."
}

$NoStableReleaseMsg = "no stable version tag is available for automatic installation. This installer requires an explicit version for preview tags -- install it explicitly: `$env:MCPTRACER_VERSION = 'v0.3.0-rc1'; irm https://raw.githubusercontent.com/$Repo/main/scripts/install.ps1 | iex"

$Version = $env:MCPTRACER_VERSION
if (-not $Version) {
    Write-InstallLog "resolving latest release..."
    # /releases/latest follows GitHub's mutable prerelease flag. We do not
    # fall back to "newest release including prereleases" on a 404, and the
    # tag check below also rejects preview-style tags if that flag was
    # cleared. Preview installation must be explicit via MCPTRACER_VERSION.
    try {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest"
        $Version = $release.tag_name
    } catch {
        $Version = $null
    }
    # GitHub's /latest endpoint is controlled by the mutable prerelease flag.
    # Keep prerelease-style tags such as v0.3.0-rc1 explicitly opted in even
    # if that flag is accidentally cleared.
    if ($Version -match "-") { $Version = $null }
    if (-not $Version) { throw $NoStableReleaseMsg }
}
$Tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
$ExpectedVersion = $Version -replace "^v", ""

$Asset = "$BinName-$Tag-$Target.zip"
$BaseUrl = "https://github.com/$Repo/releases/download/$Tag"

$WorkDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
$StagedBinary = $null
$BackupBinary = $null
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
try {
    $AssetPath = Join-Path $WorkDir $Asset
    $ChecksumPath = "$AssetPath.sha256"

    Write-InstallLog "downloading $Asset ($Tag)..."
    Invoke-WebRequest -Uri "$BaseUrl/$Asset" -OutFile $AssetPath
    Invoke-WebRequest -Uri "$BaseUrl/$Asset.sha256" -OutFile $ChecksumPath

    Write-InstallLog "verifying checksum..."
    $expected = (Get-Content $ChecksumPath -Raw).Trim().Split(" ")[0].ToLower()
    $actual = (Get-FileHash -Algorithm SHA256 $AssetPath).Hash.ToLower()
    if ($expected -ne $actual) {
        throw "checksum mismatch for ${Asset}: expected $expected, got $actual"
    }

    Write-InstallLog "extracting..."
    Expand-Archive -Path $AssetPath -DestinationPath $WorkDir -Force
    $Staging = Join-Path $WorkDir "$BinName-$Tag-$Target"
    $SourceBinary = Join-Path $Staging "$BinName.exe"
    if (-not (Test-Path $SourceBinary)) {
        throw "extracted archive did not contain $BinName.exe"
    }

    $VersionCheckFailure = "downloaded binary does not run or does not match $Version (likely causes: wrong CPU architecture, a missing runtime dependency such as the VC++ redistributable, or a corrupted extraction)"
    try {
        $VersionOutput = (& $SourceBinary --version 2>&1 | Out-String).Trim()
        $VersionExitCode = $LASTEXITCODE
    } catch {
        throw "${VersionCheckFailure}: $($_.Exception.Message)"
    }
    if ($VersionExitCode -ne 0 -or $VersionOutput -ne "$BinName $ExpectedVersion") {
        throw "$VersionCheckFailure (exit $VersionExitCode; output: $VersionOutput)"
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $InstalledBinary = Join-Path $InstallDir "$BinName.exe"
    $StagedBinary = Join-Path $InstallDir ".${BinName}.$([guid]::NewGuid().ToString('N')).tmp.exe"
    Copy-Item $SourceBinary $StagedBinary
    try {
        $StagedOutput = (& $StagedBinary --version 2>&1 | Out-String).Trim()
        $StagedExitCode = $LASTEXITCODE
    } catch {
        throw "${VersionCheckFailure}: $($_.Exception.Message)"
    }
    if ($StagedExitCode -ne 0 -or $StagedOutput -ne "$BinName $ExpectedVersion") {
        throw "$VersionCheckFailure (exit $StagedExitCode; output: $StagedOutput)"
    }

    # Promote on the install volume only after verification; File.Replace keeps
    # an existing binary if this step cannot complete.
    if (Test-Path -LiteralPath $InstalledBinary) {
        $BackupBinary = Join-Path $InstallDir ".${BinName}.$([guid]::NewGuid().ToString('N')).bak"
        [System.IO.File]::Replace($StagedBinary, $InstalledBinary, $BackupBinary)
        Remove-Item -LiteralPath $BackupBinary -Force -ErrorAction SilentlyContinue
        $BackupBinary = $null
    } else {
        [System.IO.File]::Move($StagedBinary, $InstalledBinary)
    }
    $StagedBinary = $null
    Write-InstallLog "installed $Tag to $InstallDir\$BinName.exe"

    try {
        $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
        if (($userPath -split ";") -notcontains $InstallDir) {
            [Environment]::SetEnvironmentVariable("Path", "$userPath;$InstallDir", "User")
            Write-InstallLog "added $InstallDir to your user PATH (restart your terminal to pick it up)"
        }
    } catch {
        Write-Warning "installed successfully, but could not update your user PATH: $($_.Exception.Message)"
    }
} finally {
    if ($StagedBinary -and (Test-Path -LiteralPath $StagedBinary)) {
        Remove-Item -LiteralPath $StagedBinary -Force -ErrorAction SilentlyContinue
    }
    if ($BackupBinary -and (Test-Path -LiteralPath $BackupBinary)) {
        Remove-Item -LiteralPath $BackupBinary -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -Recurse -Force $WorkDir -ErrorAction SilentlyContinue
}
