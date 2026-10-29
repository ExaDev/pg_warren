#!/usr/bin/env python3
"""Crash recovery during refresh, compute and loads (DESIGN.md §13, M11
"Robustness" and 0.9.0 "Lifecycle"): installs pg_warren (unless
--no-install), and for each shared-cache mode (default: preload, then
on) starts a scratch cluster, builds a graph big enough to stop a call
mid-way, and stops calls at each point (a refresh waiting to read edges,
writing segments and before commit; `pagerank` iterating; a read
loading the current version) in each way (pg_terminate_backend,
pg_cancel_backend where a statement runs, SIGKILL of the backend, an
immediate shutdown), checking after each recovery that the catalog and
the graph's answers are as before, that nothing of the stopped call or
backend is left, that a cancelled backend goes on, that a reader waiting
on a stopped load reads, that the shared cache works, and the server
log; after each way, that a refresh succeeds (harness/crash_recovery.py).
Holds /tmp/pgrx-test.lock for the whole run."""

import argparse
import os
import sys

from harness import crash_recovery, extension, server_log, shared_cache
from harness.cluster import Cluster
from harness.session import Session

DATABASE = "crash"


def main(argv):
    options = argparse.ArgumentParser(description=__doc__)
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"))
    options.add_argument("--no-install", action="store_true", help="use the pg_warren already installed")
    options.add_argument("--runas", default="postgres", help="cluster user when run as root (default postgres)")
    options.add_argument("--keep", action="store_true", help="keep the cluster directories for inspection")
    options.add_argument("--nodes", type=int, default=200_000, help="nodes in the graph (default 200000)")
    options.add_argument("--edges", type=int, default=2_000_000, help="edge rows in the graph (default 2000000)")
    options.add_argument("--kinds", nargs="+", choices=crash_recovery.KINDS, default=list(crash_recovery.KINDS))
    options.add_argument("--points", nargs="+", choices=crash_recovery.POINTS, default=list(crash_recovery.POINTS))
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
            print(f"crash recovery ({mode}): {'FAILED' if failures else 'ok'}", flush=True)
    return 1 if failed else 0


def run_mode(args, mode):
    # Without autovacuum, space freed by a stopped refresh is not reused, so
    # the segments table grows as the next refresh writes (the signal for
    # "writing segments").
    settings = (*shared_cache.settings(mode), *server_log.SETTINGS, "autovacuum = off")
    failures = []
    with Cluster(args.pg_config, args.runas, args.keep, settings=settings) as cluster:
        cluster.database().execute(f"CREATE DATABASE {DATABASE}")
        database = cluster.database(DATABASE)
        database.execute("CREATE EXTENSION pg_warren")
        database.execute(crash_recovery.setup_sql(args.nodes, args.edges))
        database.execute(f"SELECT warren.refresh('{crash_recovery.GRAPH}'); "
                         f"SELECT warren.vector_const('{crash_recovery.GRAPH}', 'ones', 1)")
        for kind in args.kinds:
            for point in (p for p in args.points if crash_recovery.applies(p, kind)):
                found, seen = scenario(cluster, point, kind)
                print(f"{'ok  ' if not found else 'FAIL'} {kind} at {point}{seen}", flush=True)
                failures += [f"{kind} at {point}: {failure}" for failure in found]
            with Session(cluster, DATABASE) as s:
                found = crash_recovery.refresh_builds_the_source(s)
            print(f"{'ok  ' if not found else 'FAIL'} a refresh after {kind}", flush=True)
            failures += found
    for failure in failures:
        print(f"  {failure}")
    return failures


def scenario(cluster, point, kind):
    """What is wrong after `kind` stops a refresh at `point`, and what
    `warren.progress()` showed of it then, if anything."""
    run = crash_recovery.Scenario(cluster, DATABASE, point, kind)
    try:
        found = run.run()
    except RuntimeError as error:
        run.recover()
        found = [str(error)]
    seen = run.interrupted
    if seen is None or seen.phase is None:
        return found, ""
    doing = "building" if crash_recovery.statement(point) == crash_recovery.REFRESH else "on"
    return found, f" (stopped {seen.phase}" + (f", {doing} version {seen.version})" if seen.version else ")")


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
