#!/usr/bin/env python3
"""A base backup restored and recovered to a point in time (DESIGN.md
§5.1, §13 0.9.0 "Lifecycle"): installs pg_warren (unless --no-install),
and for each shared-cache mode (default: preload, then on) starts a
scratch primary that archives its WAL, builds graphs, versions and
vectors, loads them into the shared cache, takes a base backup
(`pg_basebackup`), then refreshes, promotes a staged version and writes
and drops vectors, marking a restore point after each change. It
restores the backup into a new cluster for each restore point and for
the end of the archive, and checks there that every catalog table holds
the rows and every read gives the answers they had at the target, that
the shared cache works, that the graphs can be written, and the server
logs (harness/point_in_time.py). Holds /tmp/pgrx-test.lock for the whole
run."""

import argparse
import itertools
import os
import sys
import time

from harness import extension, graph_state, point_in_time, replicas, server_log, shared_cache
from harness.cluster import Cluster
from harness.session import Session

DATABASE = "restored"
WAIT_SECONDS = 120


def main(argv):
    options = argparse.ArgumentParser(description=__doc__)
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"))
    options.add_argument("--no-install", action="store_true", help="use the pg_warren already installed")
    options.add_argument("--runas", default="postgres", help="cluster user when run as root (default postgres)")
    options.add_argument("--keep", action="store_true", help="keep the cluster directories for inspection")
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
            print(f"point-in-time recovery ({mode}): {'FAILED' if failures else 'ok'}", flush=True)
    return 1 if failed else 0


def run_mode(args, mode):
    settings = (*shared_cache.settings(mode), *server_log.SETTINGS)
    failures = []
    with Cluster(args.pg_config, args.runas, args.keep, settings=(*settings, *point_in_time.SETTINGS)) as primary:
        try:
            states = history(primary)
            # Each target must be told apart from the one before it, or
            # recovering to the wrong one would pass.
            failures += [f"{a or 'the end'} and {b or 'the end'} hold the same graphs"
                         for a, b in itertools.pairwise(states)
                         if not graph_state.differences(states[a], states[b])]
            for target, expected in states.items():
                found = restored(args, settings, primary, target, expected)
                print(f"{'ok  ' if not found else 'FAIL'} recovered to {target or 'the end of the archive'}",
                      flush=True)
                failures += [f"recovered to {target or 'the end'}: {f}" for f in found]
        finally:
            failures += [f"primary {p}" for p in point_in_time.log_problems(primary.log.read_text())]
    return failures


def history(primary):
    """Builds the graphs on `primary`, backs it up, makes the changes and
    archives the WAL; the state at each restore point, and at the end
    (None)."""
    primary.database().execute(f"CREATE DATABASE {DATABASE}")
    build = primary.database(DATABASE)
    build.execute("CREATE EXTENSION pg_warren")
    build.execute(replicas.setup_sql())
    states = {}
    with Session(primary, DATABASE) as s:
        graph_state.checked(s, "SELECT warren.prewarm('social'), warren.prewarm('kg')")
        primary.base_backup(primary.directory / point_in_time.BACKUP)
        states[point_in_time.BACKED_UP] = mark(s, point_in_time.BACKED_UP)
        for name, statements in point_in_time.CHANGES:
            for sql in statements:
                graph_state.checked(s, sql)
            states[name] = mark(s, name)
        for sql in point_in_time.LATER:
            graph_state.checked(s, sql)
        states[None] = graph_state.capture(s)
        wait(s, point_in_time.archived_sql(graph_state.checked(s, point_in_time.SWITCH).value()),
             "the last WAL file to be archived")
    return states


def mark(s, name):
    """The state now, then a restore point `name` after it."""
    state = graph_state.capture(s)
    graph_state.checked(s, f"SELECT pg_create_restore_point('{name}')")
    return state


def restored(args, settings, primary, target, expected):
    """What differs from `expected` on the backup restored and recovered
    to `target`, and what fails there."""
    recovery = point_in_time.recovery_settings(primary.directory / point_in_time.ARCHIVE, target)
    with Cluster(args.pg_config, args.runas, args.keep, settings=(*settings, *recovery),
                 backup=primary.directory / point_in_time.BACKUP) as cluster:
        with Session(cluster, DATABASE) as s:
            wait(s, point_in_time.PROMOTED, "recovery to end")
            failures = graph_state.differences(expected, graph_state.capture(s))
        failures += replicas.shared_cache_works(cluster, DATABASE, "the restored cluster")
        with Session(cluster, DATABASE) as s:
            failures += graph_state.writes_work(s)
        return failures + point_in_time.log_problems(cluster.log.read_text())


def wait(s, sql, what):
    """Polls `sql`, a boolean, in session `s` until it is true."""
    deadline = time.monotonic() + WAIT_SECONDS
    while not point_in_time.holds(s.run(sql)):
        if time.monotonic() > deadline:
            raise RuntimeError(f"waited {WAIT_SECONDS} s for {what}")
        time.sleep(0.1)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
