#!/usr/bin/env python3
"""Many backends refreshing and querying one graph at once (DESIGN.md §13,
M11 "Robustness"): installs pg_warren (unless --no-install), and for each
shared-cache mode (default: preload, then on) starts a scratch cluster,
builds a graph and runs readers, refreshers and a vector churner on it for
--seconds, checking every read against the oracle of the generation it
read, every failure's SQLSTATE, what is left once they disconnect, and the
server log (harness/concurrent_refresh.py). Holds /tmp/pgrx-test.lock for
the whole run."""

import argparse
import os
import sys

from harness import concurrent_refresh, extension, server_log, shared_cache
from harness.cluster import Cluster

DATABASE = "concurrent"


def main(argv):
    options = argparse.ArgumentParser(description=__doc__)
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"))
    options.add_argument("--no-install", action="store_true", help="use the pg_warren already installed")
    options.add_argument("--runas", default="postgres", help="cluster user when run as root (default postgres)")
    options.add_argument("--keep", action="store_true", help="keep the cluster directories for inspection")
    options.add_argument("--seconds", type=float, default=20, help="how long the workers run, per mode (default 20)")
    options.add_argument("--readers", type=int, default=8, help="reading backends (default 8)")
    options.add_argument("--refreshers", type=int, default=3, help="refreshing backends (default 3)")
    options.add_argument("--nodes", type=int, default=2000, help="nodes in the graph (default 2000)")
    options.add_argument("--seed", type=int, default=1)
    options.add_argument("--shared-cache", nargs="+", choices=sorted(shared_cache.MODES), default=["preload", "on"],
                         help="the shared-cache modes to run in (default: preload on)")
    args = options.parse_args(argv)
    failed = False
    with extension.shared_lock():
        if not args.no_install:
            extension.install(args.pg_config)
        for mode in args.shared_cache:
            failures = run_mode(args, mode)
            failed = failed or bool(failures)
            for failure in failures:
                print(f"  {failure}")
            print(f"concurrent refresh ({mode}): {'FAILED' if failures else 'ok'}", flush=True)
    return 1 if failed else 0


def run_mode(args, mode):
    settings = (*shared_cache.settings(mode), *server_log.SETTINGS)
    with Cluster(args.pg_config, args.runas, args.keep, settings=settings) as cluster:
        cluster.database().execute(f"CREATE DATABASE {DATABASE}")
        cluster.database(DATABASE).execute("CREATE EXTENSION pg_warren")
        run = concurrent_refresh.Run(cluster, DATABASE, args.nodes, args.seed)
        run.prepare()
        unnamed = cluster.database(DATABASE).query(concurrent_refresh.UNNAMED_SHARED_BYTES)[0][0]
        run.go(args.seconds, args.readers, args.refreshers)
        failures = list(run.tally.mismatches)
        failures += [f"{operation} failed with {sqlstate}, which is not expected: {message}"
                     for operation, sqlstate, message in run.tally.unexpected]
        failures += concurrent_refresh.settled(run, unnamed)
        shares = cluster.database(DATABASE).query(shared_cache.SHARES)[0][0] == "t"
        failures += concurrent_refresh.too_little(run.tally, shares)
        allowed = {state for failures in concurrent_refresh.ALLOWED.values() for state, _ in failures}
        failures += [f"server log: {line}" for line in
                     server_log.problems((cluster.directory / "server.log").read_text(), allowed)]
        print(concurrent_refresh.summary(run.tally), flush=True)
    return failures


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
