#!/usr/bin/env python3
"""A release's source assets beside the `git archive` tarball
(CONTRIBUTING.md, "Building from source"), each made from that tarball and
the same bytes every time:

    release/source.py vendored SOURCE_TARBALL OUT   # OUT/pg_warren-X.Y.Z-vendored.tar.gz
    release/source.py sbom SOURCE_TARBALL OUT       # OUT/pg_warren-X.Y.Z.cdx.json

`vendored` is the source with every crate Cargo.lock pins (`cargo vendor
--locked --versioned-dirs`, all platforms') in `vendor/` and a
`.cargo/config.toml` that points crates.io there, so `cargo build
--offline` needs no network. `sbom` is pg_warren's CycloneDX 1.5 bill of
materials (cargo-cyclonedx, every target platform and feature): each crate
with its version, licence, Cargo.lock's sha256 and purl, dated by the
release commit, with no path of the machine that made it.

Both need cargo (the source's rust-toolchain.toml), and `sbom`
cargo-cyclonedx, on PATH; both read the crates.io index (or a vendored
source). Run from the repository root. release/assets.py checks both.
"""

import argparse
import datetime
import gzip
import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import uuid
from pathlib import Path

REPOSITORY = "https://github.com/ExaDev/pg_warren"
# cargo vendor's own advice, with the directory relative to the source.
CARGO_CONFIG = """# Written by release/source.py: crates.io's crates come from vendor/,
# so `cargo build --offline` needs no network (CONTRIBUTING.md,
# "Building from source").
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
"""
SBOM_CRATE = Path("crates") / "pg_warren"


class SourceError(Exception):
    """A source asset cannot be made as asked."""


def vendored_name(version):
    return f"pg_warren-{version}-vendored.tar.gz"


def sbom_name(version):
    return f"pg_warren-{version}.cdx.json"


def read_source(tarball):
    """The `git archive` tarball's prefix, commit (its global pax comment),
    date (its members') and members, each `(TarInfo, bytes or None)`."""
    with tarfile.open(tarball) as tar:
        members = [(m, tar.extractfile(m).read() if m.isfile() else None) for m in tar.getmembers()]
        commit = tar.pax_headers.get("comment", "")
    if not members or not members[0][0].isdir():
        raise SourceError(f"{tarball} is not a `git archive --prefix` tarball")
    prefix = members[0][0].name
    return prefix, commit, members[0][0].mtime, members


def workspace_version(members, prefix):
    for info, data in members:
        if info.name == f"{prefix}/Cargo.toml":
            return tomllib.loads(data.decode())["workspace"]["package"]["version"]
    raise SourceError("the source has no Cargo.toml")


def extracted(members, directory):
    """Writes the source's members under `directory` (links as links)."""
    for info, data in members:
        path = Path(directory) / info.name
        if info.isdir():
            path.mkdir(parents=True, exist_ok=True)
        elif info.issym():
            path.symlink_to(info.linkname)
        else:
            path.write_bytes(data)
            path.chmod(info.mode)


def cargo_vendor(root):
    subprocess.run(["cargo", "vendor", "--locked", "--versioned-dirs", "--quiet", "vendor"], cwd=root, check=True,
                   stdout=subprocess.DEVNULL)


def tree_members(root, under, epoch):
    """`(TarInfo, bytes or None)` of every directory and regular file in
    `root/under`, named from `root`'s parent, root-owned and dated `epoch`."""
    found = []
    for path in sorted([root / under, *(root / under).rglob("*")]):
        info = tarfile.TarInfo(str(path.relative_to(root.parent)))
        info.mtime, info.uid, info.gid, info.uname, info.gname = epoch, 0, 0, "root", "root"
        if path.is_symlink():
            raise SourceError(f"{info.name} is a link")
        if path.is_dir():
            info.type, info.mode = tarfile.DIRTYPE, 0o755
            found.append((info, None))
        else:
            data = path.read_bytes()
            info.size, info.mode = len(data), 0o755 if os.access(path, os.X_OK) else 0o644
            found.append((info, data))
    return found


def write_tarball(path, members, commit, epoch):
    """A gzipped PAX tarball of `members` in name order, with `git
    archive`'s global comment: the same bytes for the same members."""
    with open(path, "wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", mtime=epoch, filename="") as zipped, \
            tarfile.open(fileobj=zipped, mode="w", format=tarfile.PAX_FORMAT,
                         pax_headers={"comment": commit} if commit else {}) as tar:
        for info, data in sorted(members, key=lambda member: member[0].name):
            tar.addfile(info, io.BytesIO(data) if data is not None else None)
    return path


def vendored(source_tarball, out, vendor=cargo_vendor):
    """OUT/pg_warren-X.Y.Z-vendored.tar.gz: the source, `vendor/` and
    `.cargo/config.toml`, dated and named as the source tarball."""
    prefix, commit, epoch, members = read_source(source_tarball)
    if any(info.name == f"{prefix}/.cargo/config.toml" for info, _ in members):
        raise SourceError("the source has its own .cargo/config.toml: merge the vendored source into it")
    version = workspace_version(members, prefix)
    with tempfile.TemporaryDirectory() as directory:
        extracted(members, directory)
        root = Path(directory) / prefix
        vendor(root)
        (root / ".cargo").mkdir()
        (root / ".cargo" / "config.toml").write_text(CARGO_CONFIG)
        added = tree_members(root, "vendor", epoch) + tree_members(root, ".cargo", epoch)
    return write_tarball(Path(out) / vendored_name(version), members + added, commit, epoch)


def timestamp(epoch):
    return datetime.datetime.fromtimestamp(epoch, datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def without(value, text):
    """`value` (decoded JSON) with every occurrence of `text` in its
    strings removed."""
    if isinstance(value, dict):
        return {key: without(item, text) for key, item in value.items()}
    if isinstance(value, list):
        return [without(item, text) for item in value]
    return value.replace(text, "") if isinstance(value, str) else value


def normalise_sbom(bom, version, commit, epoch, root=None):
    """cargo-cyclonedx's BOM as the release's: dated by its commit, with a
    serial number derived from the release and commit, the commit as a vcs
    reference, and `root` (where it was made) taken out of every path."""
    if root is not None:
        bom = without(bom, str(root).rstrip("/"))
    serial = uuid.uuid5(uuid.NAMESPACE_URL, f"{REPOSITORY}/releases/tag/v{version}#{commit}")
    metadata = {**bom["metadata"], "timestamp": timestamp(epoch)}
    component = metadata["component"]
    references = [r for r in component.get("externalReferences", []) if r.get("comment") != "the release commit"]
    metadata["component"] = {**component, "externalReferences": [
        *references, {"type": "vcs", "url": f"{REPOSITORY}/tree/{commit}", "comment": "the release commit"}]}
    return {"bomFormat": bom["bomFormat"], "specVersion": bom["specVersion"], "serialNumber": f"urn:uuid:{serial}",
            **{key: value for key, value in bom.items() if key not in ("bomFormat", "specVersion", "serialNumber")},
            "metadata": metadata}


def cyclonedx(root, epoch):
    """pg_warren's BOM as cargo-cyclonedx writes it for the source at
    `root`, which must leave Cargo.lock as it is."""
    lock = (root / "Cargo.lock").read_bytes()
    subprocess.run(["cargo", "cyclonedx", "--manifest-path", str(root / "Cargo.toml"), "--format", "json",
                    "--spec-version", "1.5", "--target", "all", "--all-features", "-q"],
                   check=True, env={**os.environ, "SOURCE_DATE_EPOCH": str(epoch)})
    if (root / "Cargo.lock").read_bytes() != lock:
        raise SourceError("cargo-cyclonedx changed Cargo.lock: the SBOM would not be of the locked crates")
    return json.loads((root / SBOM_CRATE / "pg_warren.cdx.json").read_text())


def sbom(source_tarball, out, generate=cyclonedx):
    """OUT/pg_warren-X.Y.Z.cdx.json, pg_warren's normalised BOM."""
    prefix, commit, epoch, members = read_source(source_tarball)
    version = workspace_version(members, prefix)
    with tempfile.TemporaryDirectory() as directory:
        extracted(members, directory)
        root = Path(directory) / prefix
        bom = normalise_sbom(generate(root, epoch), version, commit, epoch, root)
    made = Path(out) / sbom_name(version)
    made.write_text(json.dumps(bom, indent=2, ensure_ascii=False) + "\n")
    return made


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("asset", choices=["vendored", "sbom"])
    parser.add_argument("source", type=Path, help="the release's `git archive` tarball")
    parser.add_argument("out", type=Path)
    args = parser.parse_args(argv)
    try:
        args.out.mkdir(parents=True, exist_ok=True)
        print((vendored if args.asset == "vendored" else sbom)(args.source, args.out))
    except (SourceError, subprocess.CalledProcessError, OSError, KeyError, ValueError) as error:
        print(f"release/source.py: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
