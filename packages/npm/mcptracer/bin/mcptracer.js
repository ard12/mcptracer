#!/usr/bin/env node

const fs = require('fs');
const path = require('path');
const os = require('os');
const crypto = require('crypto');
const { spawn, execFileSync } = require('child_process');

// Single source of truth: package.json's version, not a second hardcoded
// literal that can silently drift from it (and from the actual release tag).
const VERSION = require('../package.json').version;
const REPO = 'ard12/mcptracer';

function getPlatformTriple() {
  const type = os.type();
  const arch = os.arch();

  if (type === 'Windows_NT' && arch === 'x64') {
    return { target: 'x86_64-pc-windows-msvc', ext: 'zip', exe: 'mcptracer.exe' };
  } else if (type === 'Linux' && arch === 'x64') {
    return { target: 'x86_64-unknown-linux-gnu', ext: 'tar.gz', exe: 'mcptracer' };
  } else if (type === 'Linux' && arch === 'arm64') {
    return { target: 'aarch64-unknown-linux-gnu', ext: 'tar.gz', exe: 'mcptracer' };
  } else if (type === 'Darwin' && arch === 'arm64') {
    return { target: 'aarch64-apple-darwin', ext: 'tar.gz', exe: 'mcptracer' };
  } else if (type === 'Darwin' && arch === 'x64') {
    return { target: 'x86_64-apple-darwin', ext: 'tar.gz', exe: 'mcptracer' };
  }

  throw new Error(`Unsupported platform/architecture: ${type} ${arch}`);
}

function findBinary() {
  if (Object.prototype.hasOwnProperty.call(process.env, 'MCPTRACER_BIN')) {
    const override = process.env.MCPTRACER_BIN;
    if (!override || !fs.existsSync(override) || !fs.statSync(override).isFile()) {
      throw new Error('MCPTRACER_BIN must name an existing binary file');
    }
    return override;
  }

  const { target, exe } = getPlatformTriple();

  // Cargo markers cannot authorize execution. Development binaries require
  // the operator's explicit MCPTRACER_BIN selection.

  const homeDir = os.homedir();
  const cachedBin = path.join(homeDir, '.mcptracer', 'bin', `mcptracer-v${VERSION}-${target}`, exe);
  if (fs.existsSync(cachedBin)) return cachedBin;

  return null;
}

// On Windows a bare `tar.exe` resolves through PATH, and under Git Bash that
// is GNU tar, which reads `C:\...` as a remote `host:path` and fails ("Cannot
// connect to C"). The system bsdtar handles both drive letters and .zip.
function windowsTar() {
  const systemRoot = process.env.SystemRoot || process.env.windir || 'C:\\Windows';
  const systemTar = path.join(systemRoot, 'System32', 'tar.exe');
  return fs.existsSync(systemTar) ? systemTar : 'tar.exe';
}

// Verifies `filePath` against the `<hash>  <filename>` line for
// `archiveName` in the release's SHA256SUMS.txt. Throws on any mismatch or
// missing entry -- there is no "proceed anyway" path.
function verifyChecksum(cacheDir, archiveName, archivePath) {
  const sumsPath = path.join(cacheDir, `SHA256SUMS-v${VERSION}.txt`);
  const sumsUrl = `https://github.com/${REPO}/releases/download/v${VERSION}/SHA256SUMS.txt`;
  const curlBin = process.platform === 'win32' ? 'curl.exe' : 'curl';
  execFileSync(curlBin, ['-fsSL', '-o', sumsPath, sumsUrl], { stdio: 'inherit' });

  const sums = fs.readFileSync(sumsPath, 'utf8');
  const line = sums
    .split('\n')
    .find((l) => l.trim().endsWith(archiveName));
  if (!line) {
    throw new Error(`No checksum entry for ${archiveName} in ${sumsUrl}`);
  }
  const expected = line.trim().split(/\s+/)[0].toLowerCase();

  const actual = crypto
    .createHash('sha256')
    .update(fs.readFileSync(archivePath))
    .digest('hex');

  if (actual !== expected) {
    throw new Error(
      `Checksum mismatch for ${archiveName}: expected ${expected}, got ${actual}. ` +
        'Refusing to run an unverified binary.'
    );
  }
}

function ensureBinary() {
  const existing = findBinary();
  if (existing) return existing;

  const { target, ext, exe } = getPlatformTriple();
  const homeDir = os.homedir();
  const cacheDir = path.join(homeDir, '.mcptracer', 'bin');
  fs.mkdirSync(cacheDir, { recursive: true });

  const archiveName = `mcptracer-v${VERSION}-${target}.${ext}`;
  const archivePath = path.join(cacheDir, archiveName);
  const extractDir = path.join(cacheDir, `mcptracer-v${VERSION}-${target}`);
  const targetBin = path.join(extractDir, exe);

  const url = `https://github.com/${REPO}/releases/download/v${VERSION}/${archiveName}`;
  console.error(`[mcptracer] Downloading precompiled binary from ${url}...`);

  try {
    if (process.platform === 'win32') {
      execFileSync('curl.exe', ['-fsSL', '-o', archivePath, url], { stdio: 'inherit' });
    } else {
      execFileSync('curl', ['-fsSL', '-o', archivePath, url], { stdio: 'inherit' });
    }

    console.error('[mcptracer] Verifying SHA256 checksum...');
    verifyChecksum(cacheDir, archiveName, archivePath);

    if (process.platform === 'win32') {
      execFileSync(windowsTar(), ['-xf', archivePath, '-C', cacheDir], { stdio: 'inherit' });
    } else {
      execFileSync('tar', ['-xzf', archivePath, '-C', cacheDir], { stdio: 'inherit' });
      fs.chmodSync(targetBin, 0o755);
    }
  } catch (err) {
    // Never leave an unverified or partially-extracted archive/binary behind
    // for a future run to pick up without re-checking.
    fs.rmSync(archivePath, { force: true });
    fs.rmSync(extractDir, { recursive: true, force: true });
    throw new Error(`Failed to download and verify MCPTracer binary: ${err.message}`);
  }

  if (!fs.existsSync(targetBin)) {
    throw new Error(`Extracted binary not found at ${targetBin}`);
  }

  return targetBin;
}

function main() {
  let binPath;
  try {
    binPath = ensureBinary();
  } catch (err) {
    console.error(`[mcptracer] Error: ${err.message}`);
    process.exit(1);
  }

  const child = spawn(binPath, process.argv.slice(2), {
    stdio: 'inherit',
    env: process.env,
  });

  child.on('error', (err) => {
    console.error(`[mcptracer] Failed to spawn process: ${err.message}`);
    process.exit(1);
  });

  child.on('exit', (code, signal) => {
    if (signal) {
      process.kill(process.pid, signal);
    } else {
      process.exit(code ?? 0);
    }
  });
}

module.exports = { findBinary };
if (require.main === module) main();
