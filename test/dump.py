#!/usr/bin/env python3
"""The dump/restore check (DESIGN.md §5.1, §12): installs pg_warren (unless
--no-install), builds graphs in a scratch cluster, dumps them with pg_dump
(plain and custom formats), restores each dump into a fresh database and
checks that pg_warren answers as before. Also checks a trusted install's
installer dump (DESIGN.md §7), and that a text graph restored into a
database of another encoding is refused (DESIGN.md §5.7). Holds /tmp/pgrx-test.lock for the whole
run."""

import argparse
import os
import shutil
import sys
import tempfile
from pathlib import Path

from harness import dump, extension
from harness.cluster import Cluster
from harness.installer import Installer


def main(argv):
    options = argparse.ArgumentParser(description=__doc__)
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"))
    options.add_argument("--no-install", action="store_true", help="use the pg_warren already installed")
    options.add_argument("--runas", default="postgres", help="cluster user when run as root (default postgres)")
    args = options.parse_args(argv)
    directory = Path(tempfile.mkdtemp(prefix="warren-dump-"))
    try:
        with extension.shared_lock():
            if not args.no_install:
                extension.install(args.pg_config)
            with Cluster(args.pg_config, args.runas) as cluster:
                Installer.trusted().prepare(cluster)
                failures = (dump.check(cluster, directory) + dump.check_trusted(cluster, directory)
                            + dump.check_cross_encoding(cluster, directory))
    finally:
        shutil.rmtree(directory, ignore_errors=True)
    for failure in failures:
        print(failure)
    print(f"dump and restore (plain, custom, trusted, cross-encoding): {'FAILED' if failures else 'ok'}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
