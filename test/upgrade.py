#!/usr/bin/env python3
"""The upgrade-path check (DESIGN.md §13): installs pg_warren (unless
--no-install) and the released install scripts, then checks that
ALTER EXTENSION pg_warren UPDATE from every released version gives the
catalog of a fresh install of the version in development. With --trusted,
a non-superuser that owns each database installs and updates pg_warren.
Holds /tmp/pgrx-test.lock for the whole run."""

import argparse
import os
import re
import subprocess
import sys

from harness import extension, upgrade
from harness.cluster import Cluster
from harness.installer import Installer


def main(argv):
    options = argparse.ArgumentParser(description=__doc__)
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"))
    options.add_argument("--no-install", action="store_true", help="use the pg_warren already installed")
    options.add_argument("--runas", default="postgres", help="cluster user when run as root (default postgres)")
    options.add_argument("--trusted", action="store_true",
                         help="install and update pg_warren as a non-superuser that owns the database")
    args = options.parse_args(argv)
    manifest = (extension.REPOSITORY / "Cargo.toml").read_text()
    current = re.search(r'^version = "(.+)"$', manifest, re.M).group(1)
    sharedir = subprocess.run([args.pg_config, "--sharedir"], capture_output=True, text=True, check=True)
    with extension.shared_lock():
        if not args.no_install:
            extension.install(args.pg_config)
        upgrade.install_released_scripts(os.path.join(sharedir.stdout.strip(), "extension"))
        with Cluster(args.pg_config, args.runas) as cluster:
            failures = upgrade.check(cluster, current, Installer.trusted() if args.trusted else Installer())
    for failure in failures:
        print(failure)
    versions = ", ".join(upgrade.released_versions())
    print(f"upgrade paths from {versions} to {current}: {'FAILED' if failures else 'ok'}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
