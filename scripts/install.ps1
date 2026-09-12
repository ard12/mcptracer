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

$Version = $env:MCPTRACER_VERSION
if (-not $Version) {
    Write-InstallLog "resolving latest release..."
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest"
    $Version = $release.tag_name
    if (-not $Version) { throw "could not resolve the latest release tag" }
}
$Tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }

$Asset = "$BinName-$Tag-$Target.zip"
$BaseUrl = "https://github.com/$Repo/releases/download/$Tag"

$WorkDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
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

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item $SourceBinary (Join-Path $InstallDir "$BinName.exe") -Force

    Write-InstallLog "installed $Tag to $InstallDir\$BinName.exe"
    & (Join-Path $InstallDir "$BinName.exe") --version

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($userPath -split ";") -notcontains $InstallDir) {
        Write-InstallLog "adding $InstallDir to your user PATH (restart your terminal to pick it up)"
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$InstallDir", "User")
    }
} finally {
    Remove-Item -Recurse -Force $WorkDir -ErrorAction SilentlyContinue
}
