#!/usr/bin/env python3
"""pg_warren added to shared_preload_libraries and removed again (DESIGN.md
§5.6, §13 0.9.0 "Lifecycle"): installs pg_warren (unless --no-install),
starts a scratch cluster without the preload, builds graphs, versions and
vectors and loads them, then adds the preload and restarts, and removes it
and restarts, checking after each restart that every catalog table holds
the same rows and every read answers as before it, that the shared cache
works (and shares, preloaded), that the graphs can be refreshed and
vectors written, and the server log (harness/preload.py). Holds
/tmp/pgrx-test.lock for the whole run."""

import argparse
import os
import sys

from harness import extension, graph_state, preload, replicas, server_log, shared_cache
from harness.cluster import Cluster
from harness.session import Session

DATABASE = "preloaded"


def main(argv):
    options = argparse.ArgumentParser(description=__doc__)
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"))
    options.add_argument("--no-install", action="store_true", help="use the pg_warren already installed")
    options.add_argument("--runas", default="postgres", help="cluster user when run as root (default postgres)")
    options.add_argument("--keep", action="store_true", help="keep the cluster directory for inspection")
    args = options.parse_args(argv)
    with extension.shared_lock():
        if not args.no_install:
            extension.install(args.pg_config)
        failures = run(args)
    for failure in failures:
        print(f"  {failure}")
    print(f"preload added and removed: {'FAILED' if failures else 'ok'}", flush=True)
    return 1 if failures else 0


def run(args):
    with Cluster(args.pg_config, args.runas, args.keep, settings=server_log.SETTINGS) as cluster:
        cluster.database().execute(f"CREATE DATABASE {DATABASE}")
        build = cluster.database(DATABASE)
        build.execute("CREATE EXTENSION pg_warren")
        build.execute(replicas.setup_sql())
        failures = []
        for name, settings in preload.STEPS:
            with Session(cluster, DATABASE) as s:
                before = graph_state.capture(s)
                graph_state.checked(s, "SELECT warren.prewarm('social'), warren.prewarm('kg')")
            cluster.configure(settings)
            cluster.restart()
            found = step(cluster, before, name)
            print(f"{'ok  ' if not found else 'FAIL'} {name}: the catalog and every read as before, "
                  "the shared cache, refreshes and vector writes", flush=True)
            failures += [f"{name}: {f}" for f in found]
        return failures + preload.log_problems(cluster.log.read_text())


def step(cluster, before, name):
    with Session(cluster, DATABASE) as s:
        failures = graph_state.differences(before, graph_state.capture(s))
        shares = s.run(shared_cache.SHARES).value() == "t"
    if name == "preloaded" and not shares:
        failures.append("preloaded with a cap, the cluster does not share versions")
    failures += replicas.shared_cache_works(cluster, DATABASE, f"the cluster {name}")
    with Session(cluster, DATABASE) as s:
        failures += graph_state.writes_work(s)
    return failures


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
