from __future__ import annotations

import hashlib
import os
import platform
import subprocess
import sys
from pathlib import Path

# The upstream Rust binary's release tag (matches Cargo.toml / the git tag,
# e.g. "v0.3.0-rc1"), NOT this package's own PyPI version. Deliberately not
# derived from importlib.metadata: setuptools normalizes PEP 440 pre-release
# identifiers (pyproject.toml's "0.3.0-rc1" installs as "0.3.0rc1", hyphen
# stripped), which would silently break the release download URL below.
VERSION = "0.3.0-rc1"
REPO = "ard12/mcptracer"


def get_platform_triple() -> tuple[str, str, str]:
    system = platform.system().lower()
    machine = platform.machine().lower()

    if system == "windows" and machine in ("amd64", "x86_64"):
        return "x86_64-pc-windows-msvc", "zip", "mcptracer.exe"
    elif system == "linux" and machine in ("x86_64", "amd64"):
        return "x86_64-unknown-linux-gnu", "tar.gz", "mcptracer"
    elif system == "darwin" and machine in ("arm64", "aarch64"):
        return "aarch64-apple-darwin", "tar.gz", "mcptracer"
    elif system == "darwin" and machine in ("x86_64", "amd64"):
        return "x86_64-apple-darwin", "tar.gz", "mcptracer"

    raise RuntimeError(f"Unsupported platform/architecture: {system} {machine}")


def find_binary() -> Path | None:
    if "MCPTRACER_BIN" in os.environ and os.path.exists(os.environ["MCPTRACER_BIN"]):
        return Path(os.environ["MCPTRACER_BIN"])

    target, _, exe = get_platform_triple()

    # Only look for a dev build inside an actual cargo workspace checkout of
    # this repo (identified by a Cargo.toml at that level), and only a few
    # levels up -- not an unbounded walk to the filesystem root, which would
    # execute a binary planted at target/{release,debug}/<exe> under any
    # ancestor directory with zero validation.
    cur = Path(__file__).resolve()
    for parent in list(cur.parents)[:6]:
        if not (parent / "Cargo.toml").is_file():
            continue
        dev_release = parent / "target" / "release" / exe
        if dev_release.exists():
            return dev_release
        dev_debug = parent / "target" / "debug" / exe
        if dev_debug.exists():
            return dev_debug

    cache_bin = Path.home() / ".mcptracer" / "bin" / f"mcptracer-v{VERSION}-{target}" / exe
    if cache_bin.exists():
        return cache_bin

    return None


def _verify_checksum(cache_dir: Path, archive_name: str, archive_path: Path) -> None:
    """Verify archive_path against the release's SHA256SUMS.txt. Raises on
    any mismatch or missing entry -- there is no "proceed anyway" path."""
    import urllib.request

    sums_url = f"https://github.com/{REPO}/releases/download/v{VERSION}/SHA256SUMS.txt"
    sums_path = cache_dir / f"SHA256SUMS-v{VERSION}.txt"
    urllib.request.urlretrieve(sums_url, sums_path)

    line = None
    for candidate in sums_path.read_text().splitlines():
        if candidate.strip().endswith(archive_name):
            line = candidate.strip()
            break
    if line is None:
        raise RuntimeError(f"No checksum entry for {archive_name} in {sums_url}")
    expected = line.split()[0].lower()

    digest = hashlib.sha256()
    with open(archive_path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            digest.update(chunk)
    actual = digest.hexdigest()

    if actual != expected:
        raise RuntimeError(
            f"Checksum mismatch for {archive_name}: expected {expected}, got {actual}. "
            "Refusing to run an unverified binary."
        )


def ensure_binary() -> Path:
    existing = find_binary()
    if existing:
        return existing

    target, ext, exe = get_platform_triple()
    cache_dir = Path.home() / ".mcptracer" / "bin"
    cache_dir.mkdir(parents=True, exist_ok=True)

    archive_name = f"mcptracer-v{VERSION}-{target}.{ext}"
    archive_path = cache_dir / archive_name
    extract_dir = cache_dir / f"mcptracer-v{VERSION}-{target}"
    target_bin = extract_dir / exe

    url = f"https://github.com/{REPO}/releases/download/v{VERSION}/{archive_name}"
    print(f"[mcptracer] Downloading precompiled binary from {url}...", file=sys.stderr)

    import shutil
    import urllib.request

    try:
        urllib.request.urlretrieve(url, archive_path)

        print("[mcptracer] Verifying SHA256 checksum...", file=sys.stderr)
        _verify_checksum(cache_dir, archive_name, archive_path)

        shutil.unpack_archive(archive_path, cache_dir)
    except Exception:
        # Never leave an unverified or partially-extracted archive/binary
        # behind for a future run to pick up without re-checking.
        archive_path.unlink(missing_ok=True)
        shutil.rmtree(extract_dir, ignore_errors=True)
        raise

    if not target_bin.exists():
        raise RuntimeError(f"Extracted binary not found at {target_bin}")

    if os.name != "nt":
        target_bin.chmod(0o755)

    return target_bin


def main() -> None:
    try:
        bin_path = ensure_binary()
    except Exception as exc:
        print(f"[mcptracer] Error: {exc}", file=sys.stderr)
        sys.exit(1)

    result = subprocess.run([str(bin_path), *sys.argv[1:]])
    sys.exit(result.returncode)


if __name__ == "__main__":
    main()
