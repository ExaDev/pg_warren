"""Released builds of pg_warren, for the upgrade check (DESIGN.md §13): each
release's source, from the commit sql/released.commits records for it
(checked against the released install script's sha256 in
sql/released.sha256 and, if the repository has it, the tag `vX.Y.Z`), built
and installed with its
own `cargo pgrx install` into the Postgres tree of a `pg_config`, so that
state written "under a release" is written by that release's library as
well as its SQL. Then the version in development is put back, byte for
byte, before `ALTER EXTENSION pg_warren UPDATE`.

A release builds with the cargo-pgrx of the pgrx its Cargo.lock pins
(cargo-pgrx refuses to build a pgrx of another version), under the Rust
toolchain its rust-toolchain.toml pins. Builds are kept under one
directory: each release's source, one target directory per (release,
Postgres major), and each cargo-pgrx version that is not the development
one, with its own PGRX_HOME per major; CI caches it. Callers hold the machine-wide install
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


def pgrx_version_of(lock):
    """The version of package `pgrx` in the text of a Cargo.lock."""
    for package in lock.split("[[package]]"):
        if re.search(r'^name = "pgrx"$', package, re.M):
            return re.search(r'^version = "(.+)"$', package, re.M).group(1)
    raise ReleaseError("the lock file has no pgrx package")


def release_pgrx_version(version):
    """The pgrx version release `version` builds with."""
    shown = git("show", f"{release_commit(version)}:Cargo.lock")
    if shown.returncode != 0:
        raise ReleaseError(f"release {version} has no Cargo.lock")
    return pgrx_version_of(shown.stdout.decode())


def development_cargo_pgrx():
    """The version of the `cargo pgrx` on PATH."""
    found = subprocess.run(["cargo", "pgrx", "--version"], capture_output=True, text=True, check=True)
    return found.stdout.split()[-1]


def cargo_pgrx(pgrx, builds, development):
    """The command that runs cargo-pgrx `pgrx`: the development one if it
    is that version, else one installed under `builds`."""
    if pgrx == development:
        return ["cargo", "pgrx"]
    return [str(Path(builds) / f"cargo-pgrx-{pgrx}" / "bin" / "cargo-pgrx"), "pgrx"]


def extract(commit, tree):
    """`git archive` of `commit` at `tree`, unless already there: extracted
    beside it and renamed into place, so an interrupted extraction leaves no
    tree that looks complete."""
    tree = Path(tree)
    if tree.exists():
        return
    partial = Path(tempfile.mkdtemp(prefix=f".{tree.name}.", dir=tree.parent))
    try:
        with tempfile.TemporaryFile() as archive:
            subprocess.run(["git", "archive", "--format=tar", commit], cwd=REPOSITORY,
                           stdout=archive, check=True)
            archive.seek(0)
            with tarfile.open(fileobj=archive) as tar:
                tar.extractall(partial, filter="tar")
        partial.rename(tree)
    finally:
        shutil.rmtree(partial, ignore_errors=True)


class Releases:
    """Builds and installs released versions for one `pg_config`."""

    def __init__(self, pg_config, builds):
        self.pg_config = pg_config
        self.major = major(pg_config)
        self.extension_directory = Path(config(pg_config, "--sharedir")) / "extension"
        self.builds = Path(builds)
        self.builds.mkdir(parents=True, exist_ok=True)
        self.development = development_cargo_pgrx()

    def source(self, version):
        """The release's source tree (`git archive` of its recorded commit,
        extracted once and kept with the builds), with its Rust toolchain
        installed."""
        commit = release_commit(version)
        tree = self.builds / f"src-{version}-{commit[:12]}"
        extract(commit, tree)
        # The toolchain the release pins (a no-op when it is installed).
        subprocess.run(["rustup", "toolchain", "install"], cwd=tree, check=True)
        return tree

    def cargo_pgrx(self, pgrx, tree):
        """The cargo-pgrx command for pgrx `pgrx`, installed if need be (under
        the release's toolchain), and the environment it runs in: the
        development PGRX_HOME, or for another version its own, initialised
        for this major's pg_config (a cargo-pgrx reads only its own
        version's config)."""
        command = cargo_pgrx(pgrx, self.builds, self.development)
        if command == ["cargo", "pgrx"]:
            return command, dict(os.environ)
        root = self.builds / f"cargo-pgrx-{pgrx}"
        if not Path(command[0]).exists():
            subprocess.run(["cargo", "install", "cargo-pgrx", "--version", pgrx, "--locked",
                            "--root", str(root)], cwd=tree, check=True)
        home = self.builds / f"pgrx-home-{pgrx}-pg{self.major}"
        environment = dict(os.environ, PGRX_HOME=str(home))
        if not (home / "config.toml").exists():
            home.mkdir(parents=True, exist_ok=True)
            subprocess.run([*command, "init", f"--pg{self.major}={self.pg_config}"],
                           cwd=tree, env=environment, check=True)
        return command, environment

    def install(self, version):
        """Builds release `version` (its own target directory per major)
        with its own cargo-pgrx and toolchain, and installs it: library,
        control file and scripts."""
        tree = self.source(version)
        command, environment = self.cargo_pgrx(release_pgrx_version(version), tree)
        environment["CARGO_TARGET_DIR"] = str(self.builds / f"target-{version}-pg{self.major}")
        subprocess.run([*command, "install", "--pg-config", self.pg_config],
                       cwd=tree / "crates" / "pg_warren", env=environment, check=True)
