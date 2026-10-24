#!/usr/bin/env python3
"""Publishes a prepared release (CONTRIBUTING.md, "Releases and upgrades"):
checks the frozen install scripts and makes the release's tag, for
.github/workflows/release-publish.yml. Run from the repository on main,
after the release PR has merged.

    release/publish.py latest          # the last version in sql/released.commits
    release/publish.py verify          # every frozen script matches sql/released.sha256
    release/publish.py tag X.Y.Z       # annotated tag vX.Y.Z on X.Y.Z's recorded commit

`tag` makes the tag only on the commit sql/released.commits records for
the version, once that commit is in HEAD's history (the release PR was
merged with a merge commit, not squashed or rebased) and holds the released
install script. A tag already on that commit is success, so a re-run
succeeds; a tag on any other commit is an error, and is left alone.
"""

import hashlib
import re
import subprocess
import sys
from pathlib import Path

SQL = Path("sql")


class Unpublishable(Exception):
    """The release cannot be published as recorded."""


def git(*args):
    return subprocess.run(["git", *args], capture_output=True)


def output(*args):
    done = git(*args)
    if done.returncode != 0:
        raise Unpublishable(done.stderr.decode().strip())
    return done.stdout.decode().strip()


def frozen():
    """`{script: sha256}` from sql/released.sha256."""
    lines = (SQL / "released.sha256").read_text().splitlines()
    return {script: digest for digest, script in (line.split() for line in lines if line.strip())}


def recorded():
    """`[(version, sha)]` from sql/released.commits, in release order."""
    lines = (SQL / "released.commits").read_text().splitlines()
    return [(version, sha) for sha, version in (line.split() for line in lines if line.strip())]


def verify():
    for script, digest in frozen().items():
        if hashlib.sha256((SQL / script).read_bytes()).hexdigest() != digest:
            raise Unpublishable(f"sql/{script} does not match sql/released.sha256: released scripts are frozen")
    print(f"{len(frozen())} frozen install scripts match sql/released.sha256")


def release_commit(version):
    sha = dict(recorded()).get(version)
    if sha is None:
        raise Unpublishable(f"{version} has no commit in sql/released.commits")
    if git("merge-base", "--is-ancestor", sha, "HEAD").returncode != 0:
        raise Unpublishable(f"{version}'s recorded commit {sha} is not in HEAD's history: merge the release "
                            "PR with a merge commit, not a squash or a rebase")
    script = f"pg_warren--{version}.sql"
    shown = git("show", f"{sha}:sql/{script}")
    if shown.returncode != 0 or hashlib.sha256(shown.stdout).hexdigest() != frozen().get(script):
        raise Unpublishable(f"{version}'s recorded commit {sha} does not hold the released sql/{script}")
    return sha


def tag(version):
    sha = release_commit(version)
    name = f"v{version}"
    existing = git("rev-parse", "--verify", "--quiet", f"refs/tags/{name}^{{commit}}")
    if existing.returncode == 0:
        named = existing.stdout.decode().strip()
        if named != sha:
            raise Unpublishable(f"{name} names {named}, not {version}'s recorded commit {sha}")
        print(f"{name} already names {sha}")
        return
    output("tag", "--annotate", "--message", f"pg_warren {version}", name, sha)
    print(f"tagged {name} at {sha}")


def main(argv):
    try:
        if argv == ["latest"]:
            print(recorded()[-1][0])
        elif argv == ["verify"]:
            verify()
        elif len(argv) == 2 and argv[0] == "tag" and re.fullmatch(r"\d+\.\d+\.\d+", argv[1]):
            tag(argv[1])
        else:
            print(__doc__, file=sys.stderr)
            return 2
    except (Unpublishable, OSError) as error:
        print(f"release/publish.py: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
