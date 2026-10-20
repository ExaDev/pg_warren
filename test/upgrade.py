#!/usr/bin/env python3
"""The upgrade-path check (DESIGN.md §13): installs pg_warren (unless
--no-install), then for every released version builds and installs that
release from its recorded commit (sql/released.commits), writes state with
it, puts the version in development back and checks that ALTER EXTENSION
pg_warren UPDATE gives the catalog of a fresh install, and that the state
reads as it did. With --trusted, a non-superuser that owns each database
installs and updates pg_warren. Holds /tmp/pgrx-test.lock for the whole
run."""

import argparse
import os
import re
import sys
import tempfile

from harness import extension, upgrade
from harness.cluster import Cluster
from harness.installer import Installer
from harness.releases import Installed, ReleaseError, Releases, release_commit


def main(argv):
    options = argparse.ArgumentParser(description=__doc__)
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"))
    options.add_argument("--no-install", action="store_true", help="use the pg_warren already installed")
    options.add_argument("--runas", default="postgres", help="cluster user when run as root (default postgres)")
    options.add_argument("--release-builds", help="keep the releases' sources and builds here, one target "
                         "directory per release and major (default: a temporary directory)")
    options.add_argument("--trusted", action="store_true",
                         help="install and update pg_warren as a non-superuser that owns the database")
    args = options.parse_args(argv)
    manifest = (extension.REPOSITORY / "Cargo.toml").read_text()
    current = re.search(r'^version = "(.+)"$', manifest, re.M).group(1)
    try:
        for version in upgrade.released_versions():
            release_commit(version)
    except ReleaseError as error:
        print(error, file=sys.stderr)
        return 2
    with tempfile.TemporaryDirectory() as scratch, extension.shared_lock():
        if not args.no_install:
            extension.install(args.pg_config)
        installed = Installed(args.pg_config, scratch)
        releases = Releases(args.pg_config, args.release_builds or os.path.join(scratch, "builds"))
        installer = Installer.trusted() if args.trusted else Installer()
        try:
            with Cluster(args.pg_config, args.runas) as cluster:
                failures = upgrade.check(cluster, current, installer, releases, installed)
        finally:
            installed.restore()
    for failure in failures:
        print(failure)
    versions = ", ".join(upgrade.released_versions())
    print(f"upgrade paths from {versions} to {current}: {'FAILED' if failures else 'ok'}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
