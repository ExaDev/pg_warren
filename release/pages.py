#!/usr/bin/env python3
"""Assemble one Pages deployment from the brochure and a verified apt snapshot.

Release metadata is read from GitHub, never inferred from the currently served
site or the `latest` flag. Publication of a newer recorded packaged release
blocks deployment until that release has its complete signed snapshot.
"""

import argparse
import hashlib
import json
import re
import shutil
import stat
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

from assets import AssetError, readme_fingerprint
from pages_archive import restore_apt, snapshot_names

ROOT = Path(__file__).resolve().parents[1]
MAX_SITE_BYTES = 1_000_000_000
SEMVER = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")


class PagesError(ValueError):
    """The complete deployment cannot be assembled safely."""


@dataclass(frozen=True)
class ReleaseSnapshot:
    version: str
    tag: str
    names: tuple


def _run(command, **kwargs):
    try:
        return subprocess.run(command, check=True, capture_output=True, text=True, **kwargs)
    except (subprocess.CalledProcessError, OSError) as error:
        detail = getattr(error, "stderr", None) or str(error)
        raise PagesError(f"{command[0]} failed: {detail.strip()}") from error


def _validate_repository(repository):
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9][A-Za-z0-9_.-]*", repository):
        raise PagesError("repository must be OWNER/REPO")


def fetch_releases(repository):
    """Fetch every API page; an API or schema error is never an empty result."""
    _validate_repository(repository)
    done = _run(["gh", "api", "--paginate", "--slurp",
                 f"repos/{repository}/releases?per_page=100"])
    try:
        batches = json.loads(done.stdout)
    except (TypeError, json.JSONDecodeError) as error:
        raise PagesError("GitHub releases returned invalid JSON") from error
    if not isinstance(batches, list) or any(not isinstance(batch, list) for batch in batches):
        raise PagesError("GitHub releases returned an unexpected paginated response")
    return [release for batch in batches for release in batch]


def select_release(releases, recorded_versions):
    """Choose the highest numeric recorded published stable software release."""
    if not isinstance(releases, list):
        raise PagesError("GitHub releases must be a list")
    candidates = []
    seen = set()
    for release in releases:
        if not isinstance(release, dict) or not isinstance(release.get("tag_name"), str):
            raise PagesError("GitHub release metadata is malformed")
        if not isinstance(release.get("draft"), bool) or not isinstance(release.get("prerelease"), bool):
            raise PagesError("GitHub release publication flags are missing")
        tag = release["tag_name"]
        match = SEMVER.fullmatch(tag[1:]) if tag.startswith("v") else None
        if not match or release["draft"] or release["prerelease"]:
            continue
        version = tag[1:]
        numeric = tuple(map(int, match.groups()))
        if version not in recorded_versions:
            continue
        assets = release.get("assets")
        if not isinstance(assets, list) or any(not isinstance(asset, dict) or not isinstance(asset.get("name"), str)
                                               for asset in assets):
            raise PagesError(f"v{version} has malformed snapshot asset metadata")
        # Packages normally start at 0.7.0, but the publication workflow can
        # explicitly rebuild an earlier frozen release with current tooling.
        # Once any packages or snapshot assets are published there, dropping
        # the apt tree is no longer a valid brochure-only deployment.
        historical_packages = any(asset["name"].endswith(".deb") or asset["name"].startswith("pg_warren-apt-")
                                  for asset in assets)
        if numeric < (0, 7, 0) and not historical_packages:
            continue
        if version in seen:
            raise PagesError(f"duplicate published release v{version}")
        seen.add(version)
        candidates.append((numeric, release))
    if not candidates:
        return None
    _, selected = max(candidates, key=lambda pair: pair[0])
    version = selected["tag_name"][1:]
    names = snapshot_names(version)
    assets = selected.get("assets")
    found = [asset["name"] for asset in assets]
    if any(found.count(name) != 1 for name in names):
        raise PagesError(f"v{version} has no complete, unique signed apt snapshot; wait for release publication to complete")
    return ReleaseSnapshot(version, selected["tag_name"], names)


def download_snapshot(repository, tag, names, destination):
    """Download only this exact tag's three expected snapshot files."""
    command = ["gh", "release", "download", tag, "--repo", repository, "--dir", str(destination)]
    for name in names:
        command.extend(["--pattern", name])
    _run(command)


def _recorded_versions(source_root):
    versions = set()
    for line in (source_root / "sql/released.commits").read_text().splitlines():
        if not line.strip():
            continue
        fields = line.split()
        if len(fields) != 2 or not re.fullmatch(r"[0-9a-fA-F]{40}", fields[0]) or not SEMVER.fullmatch(fields[1]):
            raise PagesError("invalid sql/released.commits entry")
        if fields[1] in versions:
            raise PagesError("duplicate version in sql/released.commits")
        versions.add(fields[1])
    return versions


def _tree_size(directory, max_bytes):
    """Accept only plain directories/files, rejecting symlinks and hardlinks."""
    total = 0
    def visit(entry):
        nonlocal total
        info = entry.lstat()
        if stat.S_ISLNK(info.st_mode):
            raise PagesError(f"symbolic link is not allowed in Pages content: {entry}")
        if stat.S_ISDIR(info.st_mode):
            for child in entry.iterdir():
                visit(child)
        elif stat.S_ISREG(info.st_mode):
            if info.st_nlink != 1:
                raise PagesError(f"hardlink is not allowed in Pages content: {entry}")
            total += info.st_size
            if total > max_bytes:
                raise PagesError(f"combined Pages size exceeds {max_bytes} bytes")
        else:
            raise PagesError(f"non-regular Pages content is not allowed: {entry}")
    visit(directory)
    return total


def _sha256(filename):
    digest = hashlib.sha256()
    with filename.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def assemble(site, output, repository, *, source_root=ROOT, releases=None,
             downloader=None, restorer=None, source_sha=None, max_bytes=MAX_SITE_BYTES):
    """Verify and combine in private staging, exposing output only on success.

    Metadata, download, and restore seams permit offline selection/integration
    checks. Production callers use the GitHub CLI and cryptographic restorer.
    """
    _validate_repository(repository)
    site, output, source_root = Path(site), Path(output), Path(source_root)
    try:
        if not site.is_dir() or site.is_symlink():
            raise PagesError("site must be a plain directory")
        if output.exists() or output.is_symlink():
            raise PagesError("output already exists; choose a fresh deployment directory")
        if output.resolve().is_relative_to(site.resolve()):
            raise PagesError("output must be outside the source website")
        for reserved in ("apt", "deployment.json"):
            if (site / reserved).exists() or (site / reserved).is_symlink():
                raise PagesError(f"website contains reserved deployment path: {reserved}")
        _tree_size(site, max_bytes)
        if not (site / "index.html").is_file():
            raise PagesError("website has no index.html")
        selected = select_release(fetch_releases(repository) if releases is None else releases,
                                  _recorded_versions(source_root))
        if source_sha is None:
            source_sha = _run(["git", "rev-parse", "HEAD"], cwd=source_root).stdout.strip()
        output.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix=".pg-warren-pages-", dir=output.parent) as temporary:
            temporary = Path(temporary)
            assembled = temporary / "assembled"
            shutil.copytree(site, assembled)
            archive_sha = None
            if selected:
                downloads = temporary / "downloads"
                downloads.mkdir()
                (downloader or download_snapshot)(repository, selected.tag, selected.names, downloads)
                if {entry.name for entry in downloads.iterdir()} != set(selected.names):
                    raise PagesError("snapshot download did not produce exactly the required assets")
                for name in selected.names:
                    item = downloads / name
                    if not item.is_file() or item.is_symlink() or item.stat().st_nlink != 1:
                        raise PagesError("snapshot download contains invalid file paths or links")
                fingerprint = readme_fingerprint((source_root / "README.md").read_text())
                archive, checksum, signature = (downloads / name for name in selected.names)
                try:
                    (restorer or restore_apt)(archive, checksum, signature, fingerprint, assembled)
                except ValueError as error:
                    raise PagesError(f"apt snapshot verification failed: {error}") from error
                if not (assembled / "apt").is_dir():
                    raise PagesError("verified snapshot produced no apt directory")
                archive_sha = _sha256(archive)
            metadata = {"source_sha": source_sha, "apt_version": selected.version if selected else None,
                        "apt_archive_sha256": archive_sha}
            (assembled / "deployment.json").write_text(json.dumps(metadata, indent=2) + "\n")
            if not (assembled / ".nojekyll").exists():
                (assembled / ".nojekyll").touch()
            _tree_size(assembled, max_bytes)
            if output.exists() or output.is_symlink():
                raise PagesError("output appeared during assembly")
            assembled.rename(output)
        return metadata
    except (OSError, ValueError, AssetError) as error:
        if isinstance(error, PagesError):
            raise
        raise PagesError(str(error)) from error


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    command = commands.add_parser("assemble")
    command.add_argument("--site", type=Path, required=True)
    command.add_argument("--output", type=Path, required=True)
    command.add_argument("--repository", required=True)
    command.add_argument("--source-sha", help="the checked-out website commit (defaults to git rev-parse HEAD)")
    args = parser.parse_args(argv)
    try:
        metadata = assemble(args.site, args.output, args.repository, source_sha=args.source_sha)
    except PagesError as error:
        print(f"Pages assembly failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(metadata))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
