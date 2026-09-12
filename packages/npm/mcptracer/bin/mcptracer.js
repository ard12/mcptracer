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

// True only when this file is running out of a checkout of the mcptracer
// repository itself (not a package published to npm and installed into some
// unrelated project). Published packages must never fall through to the dev
// paths below: `../../../../target/{release,debug}` resolves outside the
// package root once installed, and if a file happens to exist there, `npx
// mcptracer` would execute it without going through the SHA256-verified
// download cache in ensureBinary()/verifyChecksum(). Presence of a
// `Cargo.toml` alone isn't enough proof (an unrelated project could have
// one), so also require it to declare this workspace by name.
function isSourceCheckout() {
  const cargoTomlPath = path.resolve(__dirname, '../../../../Cargo.toml');
  if (!fs.existsSync(cargoTomlPath)) return false;

  let contents;
  try {
    contents = fs.readFileSync(cargoTomlPath, 'utf8');
  } catch {
    return false;
  }

  return contents.includes('[workspace]') && contents.includes('crates/mcptracer-proxy');
}

function findBinary() {
  if (process.env.MCPTRACER_BIN && fs.existsSync(process.env.MCPTRACER_BIN)) {
    return process.env.MCPTRACER_BIN;
  }

  const { target, exe } = getPlatformTriple();

  if (isSourceCheckout()) {
    const devRelease = path.resolve(__dirname, '../../../../target/release', exe);
    if (fs.existsSync(devRelease)) return devRelease;

    const devDebug = path.resolve(__dirname, '../../../../target/debug', exe);
    if (fs.existsSync(devDebug)) return devDebug;
  }

  const homeDir = os.homedir();
  const cachedBin = path.join(homeDir, '.mcptracer', 'bin', `mcptracer-v${VERSION}-${target}`, exe);
  if (fs.existsSync(cachedBin)) return cachedBin;

  return null;
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
      execFileSync('tar.exe', ['-xf', archivePath, '-C', cacheDir], { stdio: 'inherit' });
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

main();