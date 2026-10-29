#!/usr/bin/env python3
"""Major-version upgrade with pg_upgrade (DESIGN.md §5.1, §13 0.9.0
"Lifecycle"): installs pg_warren into the old and the new major's trees
(unless --no-install), and for each transfer mode (default: copy, then
link) and each shared-cache mode (default: preload, then on) starts a
scratch cluster of the old major, builds graphs, versions and vectors
there, reads them and loads them into the shared cache, upgrades it with
pg_upgrade (after `pg_upgrade --check`) to a cluster of the new major,
copying its files or hard-linking them (`--link`, as managed services
do; the old cluster is not started again), and checks there that every
catalog table holds the same rows and every read answers as before, that
the shared cache works, that `ALTER EXTENSION ... UPDATE` changes nothing, that the
graphs can be refreshed and vectors written, and the server logs
(harness/major_upgrade.py). Holds /tmp/pgrx-test.lock for the whole
run."""

import argparse
import os
import sys

from harness import extension, graph_state, major_upgrade, replicas, server_log, shared_cache
from harness.cluster import TRANSFERS, Cluster, major
from harness.session import Session

DATABASE = "upgraded"


def main(argv):
    options = argparse.ArgumentParser(description=__doc__)
    options.add_argument("--old-pg-config", required=True, help="pg_config of the major to upgrade from")
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"),
                         help="pg_config of the major to upgrade to")
    options.add_argument("--no-install", action="store_true", help="use the pg_warren already installed in both")
    options.add_argument("--runas", default="postgres", help="cluster user when run as root (default postgres)")
    options.add_argument("--keep", action="store_true", help="keep the cluster directories for inspection")
    options.add_argument("--shared-cache", nargs="+", choices=sorted(shared_cache.MODES), default=["preload", "on"],
                         help="the shared-cache modes to run in (default: preload on)")
    options.add_argument("--transfer", nargs="+", choices=sorted(TRANSFERS), default=["copy", "link"],
                         help="pg_upgrade's transfer modes to run in (default: copy link)")
    args = options.parse_args(argv)
    failed = False
    with extension.shared_lock():
        if not args.no_install:
            extension.install(args.old_pg_config)
            extension.install(args.pg_config)
        for transfer in args.transfer:
            for mode in args.shared_cache:
                failures = run_mode(args, transfer, mode)
                failed = failed or bool(failures)
                for failure in failures:
                    print(f"  {failure}")
                print(f"major upgrade ({transfer}, {mode}): {'FAILED' if failures else 'ok'}", flush=True)
    return 1 if failed else 0


def run_mode(args, transfer, mode):
    settings = (*shared_cache.settings(mode), *server_log.SETTINGS)
    with Cluster(args.old_pg_config, args.runas, args.keep, settings=settings) as old:
        old.database().execute(f"CREATE DATABASE {DATABASE}")
        build = old.database(DATABASE)
        build.execute("CREATE EXTENSION pg_warren")
        build.execute(replicas.setup_sql())
        with Session(old, DATABASE) as s:
            before = graph_state.capture(s)
            failures = [f"before the upgrade: {f}" for f in replicas.read_failures(before.reads)]
            # Loaded, shared where the old cluster shares, when it stops.
            graph_state.checked(s, "SELECT warren.prewarm('social'), warren.prewarm('kg')")
        majors = f"PG {old.major()} to {major(args.pg_config)} ({transfer})"
        try:
            # Linked, the old cluster's files are the new one's: it is not
            # started or read again, only its server log.
            with Cluster(args.pg_config, args.runas, args.keep, settings=settings, upgrade=old,
                         transfer=transfer) as new:
                failures += check(new, before, majors)
                failures += [f"new {problem}" for problem in major_upgrade.log_problems(new.log.read_text())]
        finally:
            failures += [f"old {problem}" for problem in major_upgrade.log_problems(old.log.read_text())]
    return failures


def check(new, before, majors):
    with Session(new, DATABASE) as s:
        failures = graph_state.differences(before, graph_state.capture(s))
        print(f"{'ok  ' if not failures else 'FAIL'} {majors}: the catalog and every read as before", flush=True)
        failures += upgraded(s, before.extension)
    found = replicas.shared_cache_works(new, DATABASE, "the upgraded cluster")
    with Session(new, DATABASE) as s:
        found += graph_state.writes_work(s)
    print(f"{'ok  ' if not found else 'FAIL'} {majors}: the shared cache, refreshes and vector writes", flush=True)
    return failures + found


def upgraded(s, version):
    """`ALTER EXTENSION ... UPDATE` in session `s` leaves pg_warren at `version`."""
    done = s.run(major_upgrade.UPDATE)
    if not done.ok:
        return [f"{major_upgrade.UPDATE}: {done.sqlstate} {done.message}"]
    found = s.run(graph_state.EXTENSION_SQL).value()
    return [] if found == version else [f"after {major_upgrade.UPDATE}, pg_warren is {found}, not {version}"]


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
