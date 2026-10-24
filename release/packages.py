#!/usr/bin/env python3
"""Builds and checks pg_warren's packages (CONTRIBUTING.md, "Packages"):
the .deb packages of the apt repository, one per PostgreSQL major
(`postgresql-NN-pg-warren`, PGDG's naming), built from packaging/debian in
each distribution's own container against PGDG's server headers; the
tarballs, laid out for `pg_config`, made from a built package; and the
Docker image, PGDG's `postgres` image with the package installed.

    release/packages.py deb CODENAME SOURCE OUT [--majors 15 16 17 18]
    release/packages.py check DEB              # installs it with apt in a clean container
    release/packages.py tarball DEB OUT
    release/packages.py image DEB TAG          # builds the image and checks it

SOURCE is a checkout of the release (its Cargo.lock pins the cargo-pgrx to
build with); the tooling, packaging/ and this script, is the checkout this
runs from. Run from the repository root; each command needs Docker but
`tarball`, which needs dpkg-deb.
"""

import argparse
import datetime
import email.utils
import gzip
import io
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PACKAGING = ROOT / "packaging"
MAJORS = ("15", "16", "17", "18")
MAINTAINER = "Will Faithfull <wfaithfull@users.noreply.github.com>"
RELEASES = "https://github.com/ExaDev/pg_warren/releases/tag"
MACHINES = {"amd64": "x86_64", "arm64": "aarch64"}


class PackagingError(Exception):
    """A package cannot be built or checked as asked."""


@dataclass(frozen=True)
class Distribution:
    codename: str
    name: str
    image: str
    # Our revision's suffix: after a tilde, so it sorts below an official
    # build of the same release (PGDG's -1.pgdg120+1).
    suffix: str


DISTRIBUTIONS = (
    Distribution("bookworm", "Debian 12", "debian:bookworm", "deb12"),
    Distribution("trixie", "Debian 13", "debian:trixie", "deb13"),
    Distribution("jammy", "Ubuntu 22.04", "ubuntu:22.04", "ubuntu22.04"),
    Distribution("noble", "Ubuntu 24.04", "ubuntu:24.04", "ubuntu24.04"),
)


def distribution(codename):
    for known in DISTRIBUTIONS:
        if known.codename == codename:
            return known
    raise PackagingError(f"no packages are built for {codename}: {', '.join(d.codename for d in DISTRIBUTIONS)}")


def debian_version(version, codename):
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise PackagingError(f"a packaged version is X.Y.Z, not {version!r}")
    return f"{version}-1~{distribution(codename).suffix}"


def changelog(version, codename, epoch):
    """debian/changelog for the release, dated by its commit (`epoch`)."""
    date = email.utils.format_datetime(datetime.datetime.fromtimestamp(epoch, datetime.UTC)).replace("-0000", "+0000")
    return (f"pg-warren ({debian_version(version, codename)}) {codename}; urgency=medium\n\n"
            f"  * pg_warren {version}: {RELEASES}/v{version}\n\n"
            f" -- {MAINTAINER}  {date}\n")


def pgrx_version(lock):
    for package in tomllib.loads(lock).get("package", []):
        if package.get("name") == "pgrx":
            return package["version"]
    raise PackagingError("Cargo.lock pins no pgrx")


def workspace_version(source):
    return tomllib.loads((source / "Cargo.toml").read_text())["workspace"]["package"]["version"]


def commit_time(source):
    epoch = os.environ.get("SOURCE_DATE_EPOCH")
    if epoch:
        return int(epoch)
    done = subprocess.run(["git", "-C", str(source), "log", "-1", "--format=%ct"], capture_output=True, text=True)
    return int(done.stdout.strip()) if done.returncode == 0 and done.stdout.strip() else 0


def docker(*args):
    if subprocess.run(["docker", *map(str, args)]).returncode != 0:
        raise PackagingError(f"docker {args[0]} failed")


def build_debs(codename, source, out, majors):
    """Builds postgresql-NN-pg-warren for each major in `codename`'s image."""
    target = distribution(codename)
    source, out = Path(source).resolve(), Path(out).resolve()
    version = workspace_version(source)
    out.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as meta:
        (Path(meta) / "changelog").write_text(changelog(version, codename, commit_time(source)))
        docker("run", "--rm",
               "-v", f"{source}:/source:ro", "-v", f"{PACKAGING}:/packaging:ro", "-v", f"{meta}:/meta:ro",
               "-v", f"{out}:/out",
               "-e", f"PG_MAJORS={' '.join(majors)}",
               "-e", f"PGRX_VERSION={pgrx_version((source / 'Cargo.lock').read_text())}",
               "-e", f"SOURCE_DATE_EPOCH={commit_time(source)}",
               target.image, "bash", "/packaging/build-deb.sh")


def fields(deb):
    done = subprocess.run(["dpkg-deb", "--field", str(deb), "Package", "Version", "Architecture"],
                          capture_output=True, text=True)
    if done.returncode != 0:
        raise PackagingError(f"{deb} is not a package: {done.stderr.strip()}")
    return dict(line.split(": ", 1) for line in done.stdout.splitlines())


def major_and_codename(deb):
    """The PostgreSQL major and the distribution a pg_warren package is for."""
    control = fields(deb)
    major = re.fullmatch(r"postgresql-(\d+)-pg-warren", control["Package"])
    revision = control["Version"].rpartition("~")[2]
    codenames = [d.codename for d in DISTRIBUTIONS if d.suffix == revision]
    if not major or not codenames:
        raise PackagingError(f"{deb} is not a pg_warren package of a known distribution")
    return major.group(1), codenames[0], control


def tarball_name(version, major, arch):
    return f"pg_warren-{version}-pg{major}-linux-{MACHINES[arch]}"


INSTALL = """pg_warren {version} for PostgreSQL {major} on Linux ({machine}).

Install with the pg_config of that PostgreSQL:

    cp pkglibdir/* "$(pg_config --pkglibdir)"
    cp sharedir/extension/* "$(pg_config --sharedir)/extension"

Then add pg_warren to shared_preload_libraries in postgresql.conf (so that
loaded graphs are shared between backends), restart PostgreSQL, and in each
database run CREATE EXTENSION pg_warren;

Built on {built}; https://github.com/ExaDev/pg_warren
"""


def tarball(deb, out, epoch):
    """The package's files as pg_warren-X.Y.Z-pgNN-linux-MACHINE.tar.gz, in
    `pkglibdir/` and `sharedir/extension/`, with LICENSE, NOTICE and INSTALL."""
    major, codename, control = major_and_codename(deb)
    version = control["Version"].partition("-")[0]
    name = tarball_name(version, major, control["Architecture"])
    with tempfile.TemporaryDirectory() as unpacked:
        unpacked = Path(unpacked)
        subprocess.run(["dpkg-deb", "-x", str(deb), str(unpacked)], check=True)
        library = unpacked / f"usr/lib/postgresql/{major}/lib/pg_warren.so"
        if not library.is_file():
            raise PackagingError(f"{deb} has no pg_warren.so")
        files = {f"pkglibdir/{library.name}": library.read_bytes()}
        for path in sorted((unpacked / f"usr/share/postgresql/{major}/extension").glob("pg_warren*")):
            files[f"sharedir/extension/{path.name}"] = path.read_bytes()
        files["LICENSE"] = (ROOT / "LICENSE").read_bytes()
        files["NOTICE"] = (ROOT / "NOTICE").read_bytes()
        machine = MACHINES[control["Architecture"]]
        files["INSTALL"] = INSTALL.format(version=version, major=major, machine=machine,
                                          built=distribution(codename).name).encode()
    made = Path(out) / f"{name}.tar.gz"
    with open(made, "wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", mtime=epoch, filename="") as zipped, \
            tarfile.open(fileobj=zipped, mode="w", format=tarfile.PAX_FORMAT) as archive:
        directories = {name} | {f"{name}/{p}" for p in ("pkglibdir", "sharedir", "sharedir/extension")}
        entries = sorted([(d, None) for d in directories] + [(f"{name}/{p}", b) for p, b in files.items()])
        for path, data in entries:
            info = tarfile.TarInfo(path)
            info.mtime, info.uid, info.gid, info.uname, info.gname = epoch, 0, 0, "root", "root"
            if data is None:
                info.type, info.mode = tarfile.DIRTYPE, 0o755
                archive.addfile(info)
            else:
                info.size, info.mode = len(data), 0o755 if path.endswith(".so") else 0o644
                archive.addfile(info, fileobj=io.BytesIO(data))
    return made


def check(deb):
    """Installs the package with apt in a clean container of its
    distribution, from PGDG, preloads it, restarts and runs the smoke test."""
    _, codename, _ = major_and_codename(deb)
    deb = Path(deb).resolve()
    docker("run", "--rm", "-v", f"{deb.parent}:/pkg:ro", "-v", f"{PACKAGING}:/packaging:ro",
           distribution(codename).image, "bash", "/packaging/check-deb.sh", f"/pkg/{deb.name}")


def image(deb, tag):
    """PGDG's postgres:NN-CODENAME image with the package, and its check."""
    major, codename, _ = major_and_codename(deb)
    deb = Path(deb).resolve()
    with tempfile.TemporaryDirectory() as context:
        shutil.copy(deb, Path(context) / "pg_warren.deb")
        docker("build", "-f", PACKAGING / "docker" / "Dockerfile", "--build-arg", f"PG_MAJOR={major}",
               "--build-arg", f"CODENAME={codename}", "-t", tag, context)
    subprocess.run(["bash", str(PACKAGING / "check-image.sh"), tag], check=True)


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    deb = commands.add_parser("deb")
    deb.add_argument("codename")
    deb.add_argument("source", type=Path)
    deb.add_argument("out", type=Path)
    deb.add_argument("--majors", nargs="+", default=list(MAJORS), choices=MAJORS)
    commands.add_parser("check").add_argument("deb", type=Path)
    made = commands.add_parser("tarball")
    made.add_argument("deb", type=Path)
    made.add_argument("out", type=Path)
    built = commands.add_parser("image")
    built.add_argument("deb", type=Path)
    built.add_argument("tag")
    args = parser.parse_args(argv)
    try:
        if args.command == "deb":
            build_debs(args.codename, args.source, args.out, args.majors)
        elif args.command == "check":
            check(args.deb)
        elif args.command == "tarball":
            print(tarball(args.deb, args.out, commit_time(ROOT)))
        else:
            image(args.deb, args.tag)
    except (PackagingError, subprocess.CalledProcessError, OSError) as error:
        print(f"release/packages.py: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
