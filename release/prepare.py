#!/usr/bin/env python3
"""Prepares release X.Y.Z of pg_warren on the current branch, in the two
commits a release is made of (CONTRIBUTING.md, "Releases and upgrades"):

1. `release: X.Y.Z`, the commit to tag vX.Y.Z: CHANGELOG's section in
   development dated (and named, with --title), and README's install
   checking out vX.Y.Z. The version in development is still X.Y.Z, and
   sql/schema-drift.sh must pass on it first.
2. `release: freeze X.Y.Z, develop NEXT`: sql/pg_warren--X.Y.Z.sql frozen
   in sql/released.sha256, commit 1 recorded in sql/released.commits, the
   workspace version (Cargo.toml, Cargo.lock) bumped to NEXT, NEXT's install
   script generated (sql/schema-drift.sh --update), the header-only upgrade
   script sql/pg_warren--X.Y.Z--NEXT.sql, and CHANGELOG's NEXT section
   opened.

The released install script is the one commit 1 holds, byte for byte: the
drift check shows it is what pgrx generates, and the freeze records it.
Prose a person writes (CHANGELOG's notes, README's Status, DESIGN.md §13)
is merged before this runs. test/upgrade.py reads the releases from
sql/released.sha256 and sql/released.commits, so it needs no edit.

It refuses a dirty tree, a version that is released or tagged, and a
version that is not the one in development. Run again once done, it changes
nothing; run again after a failed second step, it resumes from commit 1.

    release/prepare.py 0.7.0 0.8.0 --title "real graphs" [--date YYYY-MM-DD]
"""

import argparse
import datetime
import hashlib
import re
import subprocess
import sys
import tomllib
from pathlib import Path

from changelog import ChangelogError, finalise, open_next

VERSION = re.compile(r"\d+\.\d+\.\d+")
INSTALL = re.compile(r"^(git checkout v)\S+(   # the latest release)", re.M)

# The header-only upgrade script, @START@ -> @END@; release-prepare.yml checks
# a release branch's against it too.
UPGRADE_HEADER = Path(__file__).with_name("upgrade-header.sql.in")


class Refused(Exception):
    """The repository is not in a state to prepare this release from."""


def git(root, *args):
    return subprocess.run(["git", *args], cwd=root, check=True, capture_output=True, text=True).stdout.strip()


def run(root, *command):
    done = subprocess.run(command, cwd=root, capture_output=True, text=True)
    sys.stdout.write(done.stdout)
    if done.returncode != 0:
        raise Refused(f"{' '.join(command)} failed:\n{done.stderr}")


def key(version):
    return tuple(int(part) for part in version.split("."))


def workspace_version(root):
    return tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]


def released(root):
    lines = (root / "sql" / "released.sha256").read_text().splitlines()
    return [re.search(r"pg_warren--(.+)\.sql$", line).group(1) for line in lines if line.strip()]


def subject(root):
    return git(root, "log", "-1", "--format=%s")


def tag_subject(version):
    return f"release: {version}"


def freeze_subject(version, nxt):
    return f"release: freeze {version}, develop {nxt}"


def check(root, version, nxt):
    """Refuses what this release cannot be prepared from; returns the step
    to start at (1 or 2), or None when it is already prepared."""
    for given in (version, nxt):
        if not VERSION.fullmatch(given):
            raise Refused(f"{given} is not a version X.Y.Z")
    if key(nxt) <= key(version):
        raise Refused(f"the next version, {nxt}, must come after {version}")
    if git(root, "status", "--porcelain"):
        raise Refused("the working tree is not clean: commit or remove the changes first")
    if version in released(root):
        if subject(root) == freeze_subject(version, nxt) and workspace_version(root) == nxt:
            return None
        raise Refused(f"{version} is already released (sql/released.sha256)")
    if git(root, "tag", "--list", f"v{version}"):
        raise Refused(f"tag v{version} already exists")
    if workspace_version(root) != version:
        raise Refused(f"the version in development is {workspace_version(root)}, not {version}")
    for path in (f"pg_warren--{nxt}.sql", f"pg_warren--{version}--{nxt}.sql"):
        if (root / "sql" / path).exists():
            raise Refused(f"sql/{path} already exists")
    return 2 if subject(root) == tag_subject(version) else 1


def commit(root, title, body):
    git(root, "add", "--all")
    git(root, "commit", "--quiet", "--message", f"{title}\n\n{body}")


def release_commit(root, version, date, title, drift):
    """Step 1: the commit to tag."""
    run(root, drift)
    changelog = root / "CHANGELOG.md"
    changelog.write_text(finalise(changelog.read_text(), version, date, title))
    readme = root / "README.md"
    readme.write_text(INSTALL.sub(rf"\g<1>{version}\g<2>", readme.read_text(), count=1))
    commit(root, tag_subject(version),
           f"CHANGELOG's {version} section is dated {date}, and README's install checks out\n"
           f"v{version}. No code or SQL changes: this is the commit to tag v{version}\n"
           "(release/prepare.py).")


def bump(root, version, nxt):
    manifest = root / "Cargo.toml"
    manifest.write_text(re.sub(rf'^version = "{re.escape(version)}"$', f'version = "{nxt}"',
                               manifest.read_text(), count=1, flags=re.M))
    members = tomllib.loads(manifest.read_text())["workspace"]["members"]
    names = [tomllib.loads((root / member / "Cargo.toml").read_text())["package"]["name"] for member in members]
    lock = root / "Cargo.lock"
    text = lock.read_text()
    for name in names:
        text = re.sub(rf'(\nname = "{re.escape(name)}"\nversion = "){re.escape(version)}"',
                      rf'\g<1>{nxt}"', text, count=1)
    lock.write_text(text)


def freeze_commit(root, version, nxt, drift):
    """Step 2: freeze the release and develop the next version."""
    tagged = git(root, "rev-parse", "HEAD")
    sql = root / "sql"
    script = f"pg_warren--{version}.sql"
    with (sql / "released.sha256").open("a") as frozen:
        frozen.write(f"{hashlib.sha256((sql / script).read_bytes()).hexdigest()}  {script}\n")
    with (sql / "released.commits").open("a") as commits:
        commits.write(f"{tagged}  {version}\n")
    bump(root, version, nxt)
    (sql / f"pg_warren--{version}--{nxt}.sql").write_text(
        UPGRADE_HEADER.read_text().replace("@START@", version).replace("@END@", nxt))
    changelog = root / "CHANGELOG.md"
    changelog.write_text(open_next(changelog.read_text(), nxt))
    run(root, drift, "--update")
    commit(root, freeze_subject(version, nxt),
           f"sql/{script} is frozen by sql/released.sha256, and sql/released.commits\n"
           f"records the commit to tag v{version}, {tagged[:7]}. The workspace version is {nxt}\n"
           f"(in development) with its generated sql/pg_warren--{nxt}.sql.\n"
           f"sql/pg_warren--{version}--{nxt}.sql is a header only, since the schema has not\n"
           f"changed. CHANGELOG has {nxt} (release/prepare.py).")


def restore(root):
    """Puts the tree back to HEAD after a failed step: it was clean when
    the step started."""
    git(root, "reset", "--quiet", "--hard", "HEAD")
    git(root, "clean", "--quiet", "--force", "-d")


def prepare(root, version, nxt, date, title, drift):
    step = check(root, version, nxt)
    if step is None:
        print(f"{version} is already prepared: HEAD is {freeze_subject(version, nxt)!r}")
        return
    try:
        if step == 1:
            release_commit(root, version, date, title, drift)
        freeze_commit(root, version, nxt, drift)
    except (Refused, ChangelogError, OSError):
        restore(root)
        raise
    print(f"prepared {version}: tag v{version} at {git(root, 'rev-parse', 'HEAD~1')}; developing {nxt}")


def main(argv):
    options = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    options.add_argument("version", help="the release, X.Y.Z: the version in development")
    options.add_argument("next", help="the version to develop next")
    options.add_argument("--date", default=datetime.datetime.now(datetime.UTC).date().isoformat(),
                         help="the release's date in CHANGELOG (default: today, UTC)")
    options.add_argument("--title", help="the release's name in CHANGELOG's heading")
    options.add_argument("--schema-drift", help="the drift check (default: sql/schema-drift.sh)")
    args = options.parse_args(argv)
    try:
        root = Path(git(Path.cwd(), "rev-parse", "--show-toplevel"))
        drift = args.schema_drift or str(root / "sql" / "schema-drift.sh")
        prepare(root, args.version, args.next, args.date, args.title, drift)
    except (Refused, ChangelogError) as error:
        print(f"release/prepare.py: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
