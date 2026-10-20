"""Released builds of pg_warren, for the upgrade check (DESIGN.md §13): each
release's source, from the commit sql/released.commits records for it
(checked against the released install script's sha256 in
sql/released.sha256 and, if the repository has it, the tag `vX.Y.Z`), built
and installed with its
own `cargo pgrx install` into the Postgres tree of a `pg_config`, so that
state written "under a release" is written by that release's library as
well as its SQL. Then the version in development is put back, byte for
byte, before `ALTER EXTENSION pg_warren UPDATE`.

Builds are kept under one directory, one target directory per (release,
Postgres major), which CI caches. Callers hold the machine-wide install
lock (`extension.shared_lock`): installs overwrite the one shared
`pg_warren.so`."""

import hashlib
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
from pathlib import Path

from .extension import REPOSITORY

SQL = REPOSITORY / "sql"
COMMITS = SQL / "released.commits"


class ReleaseError(Exception):
    """A released version whose commit is not recorded, not in the clone,
    or not the release."""


def git(*args):
    return subprocess.run(["git", *args], cwd=REPOSITORY, capture_output=True)


def recorded(path):
    """`{version: sha}` from a file of `<sha>  <version>` lines."""
    lines = path.read_text().splitlines() if path.exists() else []
    return {version: sha for sha, version in (line.split() for line in lines if line.strip())}


def release_commit(version, commits=COMMITS):
    """The commit recorded for release `version` in `commits`, once it is
    shown to be the release: in this clone, holding the released install
    script (its sha256 as sql/released.sha256 freezes it), and named by tag
    `v<version>` if that tag exists."""
    sha = recorded(commits).get(version)
    if sha is None:
        raise ReleaseError(f"release {version} has no commit in {commits}: record it (CONTRIBUTING.md, "
                           f"\"Releases and upgrades\")")
    if git("cat-file", "-e", f"{sha}^{{commit}}").returncode != 0:
        raise ReleaseError(f"release {version}'s commit {sha} is not in this clone: fetch the full history "
                           f"(git fetch --unshallow; CI checks out with fetch-depth: 0)")
    script = f"pg_warren--{version}.sql"
    frozen = recorded(SQL / "released.sha256").get(script)
    shown = git("show", f"{sha}:sql/{script}")
    if shown.returncode != 0 or hashlib.sha256(shown.stdout).hexdigest() != frozen:
        raise ReleaseError(f"release {version}'s commit {sha}: its sql/{script} does not match "
                           f"sql/released.sha256")
    tag = git("rev-parse", "--verify", "--quiet", f"v{version}^{{commit}}")
    if tag.returncode == 0 and tag.stdout.decode().strip() != sha:
        raise ReleaseError(f"release {version}: tag v{version} names {tag.stdout.decode().strip()}, not "
                           f"the recorded commit {sha}")
    return sha


def config(pg_config, option):
    return subprocess.run([pg_config, option], capture_output=True, text=True, check=True).stdout.strip()


def major(pg_config):
    """The Postgres major of `pg_config`, e.g. "16"."""
    return re.search(r"(\d+)", config(pg_config, "--version")).group(1)


class Installed:
    """The installed library and control file of the version in
    development, copied aside so that they can be put back after a
    release's install overwrote them."""

    def __init__(self, pg_config, directory):
        self.files = [
            Path(config(pg_config, "--pkglibdir")) / "pg_warren.so",
            Path(config(pg_config, "--sharedir")) / "extension" / "pg_warren.control",
        ]
        self.copies = [Path(directory) / f"current-{path.name}" for path in self.files]
        for path, copy in zip(self.files, self.copies):
            shutil.copy2(path, copy)

    def restore_control(self):
        """Puts the version in development's control file back: the
        releases before 0.3.0 were not trusted extensions."""
        self.put_back(self.files[1:], self.copies[1:])

    def restore(self):
        """Puts the version in development's files back, replacing the
        release's (removed first, so a failed copy cannot leave them)."""
        self.put_back(self.files, self.copies)

    @staticmethod
    def put_back(files, copies):
        for path, copy in zip(files, copies):
            path.unlink(missing_ok=True)
            shutil.copy2(copy, path)
            if path.read_bytes() != copy.read_bytes():
                raise RuntimeError(f"{path} was not restored")


class Releases:
    """Builds and installs released versions for one `pg_config`."""

    def __init__(self, pg_config, builds):
        self.pg_config = pg_config
        self.major = major(pg_config)
        self.extension_directory = Path(config(pg_config, "--sharedir")) / "extension"
        self.builds = Path(builds)
        self.builds.mkdir(parents=True, exist_ok=True)

    def source(self, version):
        """The release's source tree (`git archive` of its recorded commit,
        extracted once and kept with the builds)."""
        commit = release_commit(version)
        tree = self.builds / f"src-{version}-{commit[:12]}"
        if not (tree / "Cargo.toml").exists():
            with tempfile.TemporaryFile() as archive:
                subprocess.run(["git", "archive", "--format=tar", commit], cwd=REPOSITORY,
                               stdout=archive, check=True)
                archive.seek(0)
                with tarfile.open(fileobj=archive) as tar:
                    tar.extractall(tree, filter="tar")
        return tree

    def install(self, version):
        """Builds release `version` (its own target directory per major)
        and installs it, library, control file and scripts."""
        tree = self.source(version)
        environment = dict(os.environ, CARGO_TARGET_DIR=str(self.builds / f"target-{version}-pg{self.major}"))
        subprocess.run(["cargo", "pgrx", "install", "--pg-config", self.pg_config],
                       cwd=tree / "crates" / "pg_warren", env=environment, check=True)
