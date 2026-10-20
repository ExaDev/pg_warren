#!/usr/bin/env python3
"""Runs the SQL tests in test/sql/ (or the given files) against a scratch
Postgres cluster, after installing pg_warren with `cargo pgrx install`.

The shared lock /tmp/pgrx-test.lock is held for the whole run, so another
checkout's install cannot replace pg_warren.so under the tests. See
test/README.md for the test file format."""

import argparse
import os
import shutil
import sys
import tempfile
from pathlib import Path

from harness import extension, runner
from harness.cluster import Cluster
from harness.installer import Installer
from harness.sql_test import SqlTest

TESTS = Path(__file__).resolve().parent / "sql"


def main(argv):
    options = argparse.ArgumentParser(description=__doc__)
    options.add_argument("tests", nargs="*", type=Path, help="test files (default: test/sql/*.sql)")
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"))
    options.add_argument("--no-install", action="store_true", help="use the pg_warren already installed")
    options.add_argument("--runas", default="postgres", help="cluster user when run as root (default postgres)")
    options.add_argument("--keep", action="store_true", help="keep the cluster directory for inspection")
    options.add_argument("--trusted", action="store_true",
                         help="install pg_warren as a non-superuser that owns each test database")
    args = options.parse_args(argv)
    tests = [SqlTest.load(p) for p in (args.tests or sorted(TESTS.glob("*.sql")))]
    installer = Installer.trusted() if args.trusted else Installer()

    with extension.shared_lock():
        if not args.no_install:
            extension.install(args.pg_config)
        fixtures = tempfile.mkdtemp(prefix="warren-fixtures-")
        try:
            with Cluster(args.pg_config, args.runas, args.keep) as cluster:
                installer.prepare(cluster)
                outcomes = []
                for test in tests:
                    outcome = runner.run(cluster, test, runner.Fixtures(fixtures), installer)
                    outcomes.append(outcome)
                    print(f"{'ok  ' if outcome.passed else 'FAIL'} {outcome.name} ({outcome.seconds:.1f}s)", flush=True)
                    if not outcome.passed:
                        print(outcome.output)
        finally:
            shutil.rmtree(fixtures, ignore_errors=True)
    failed = [o.name for o in outcomes if not o.passed]
    print(f"{len(outcomes) - len(failed)} passed, {len(failed)} failed" + (f": {', '.join(failed)}" if failed else ""))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
