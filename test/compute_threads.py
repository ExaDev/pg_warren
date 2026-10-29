#!/usr/bin/env python3
"""Many backends computing at once stay within the cluster's cap on
compute threads (DESIGN.md §14 item 52): installs pg_warren (unless
--no-install), and for each shared-cache mode (default: preload, then on)
starts a scratch cluster and measures, from /proc, the compute threads' CPU
time of --sessions backends running betweenness centrality with
`warren.threads = --threads`, first without a cap, then under
`warren.cluster_threads_limit = --cap`. Uncapped they must use more than
twice the cap; capped, no more than the cap (and a quarter). Then a wait
for threads is cancelled and timed out, a backend holding the cap's thread
is terminated mid-kernel and gives it back, and the per-call limits bound
a call (harness/compute_threads.py). Throughout, `warren.backends()` must
show the threads held: within the cap, in each backend and the cluster,
and past it uncapped. Where the cap cannot be enforced (PG
15 and 16 without the preload) it checks that the server log says so.
Holds /tmp/pgrx-test.lock for the whole run."""

import argparse
import os
import sys

from harness import compute_threads, extension, server_log, shared_cache
from harness.cluster import Cluster

# A cancel and statement_timeout (57014), a terminated backend (57P01) and
# the compute memory limit (53400).
ALLOWED = {"57014", "57P01", "53400"}


def main(argv):
    options = argparse.ArgumentParser(description=__doc__)
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"))
    options.add_argument("--no-install", action="store_true", help="use the pg_warren already installed")
    options.add_argument("--runas", default="postgres", help="cluster user when run as root (default postgres)")
    options.add_argument("--keep", action="store_true", help="keep the cluster directories for inspection")
    options.add_argument("--sessions", type=int, default=4, help="backends computing at once (default 4)")
    options.add_argument("--threads", type=int, default=4, help="warren.threads of each (default 4)")
    options.add_argument("--cap", type=int, default=1, help="warren.cluster_threads_limit (default 1)")
    options.add_argument("--seconds", type=float, default=8, help="each measured window (default 8)")
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
            print(f"compute threads ({mode}): {'FAILED' if failures else 'ok'}", flush=True)
    return 1 if failed else 0


def run_mode(args, mode):
    settings = (*shared_cache.settings(mode), *server_log.SETTINGS)
    with Cluster(args.pg_config, args.runas, args.keep, settings=settings) as cluster:
        cluster.database().execute(f"CREATE DATABASE {compute_threads.DATABASE}")
        database = cluster.database(compute_threads.DATABASE)
        database.execute("CREATE EXTENSION pg_warren")
        database.execute(compute_threads.SETUP)
        enforced = database.query(
            "SELECT current_setting('server_version_num')::int >= 170000 "
            "OR current_setting('shared_preload_libraries') ~ 'pg_warren'")[0][0] == "t"
        failures = []
        compute_threads.set_limit(cluster, "warren.cluster_threads_limit", "-1")
        uncapped = compute_threads.measure(cluster, args.sessions, args.threads, args.seconds)
        compute_threads.set_limit(cluster, "warren.cluster_threads_limit", str(args.cap))
        capped = compute_threads.measure(cluster, args.sessions, args.threads, args.seconds)
        print(f"uncapped: {uncapped.cores:.2f} cores over {uncapped.seconds:.1f} s, {uncapped.calls} calls; "
              f"cap {args.cap}: {capped.cores:.2f} cores, {capped.calls} calls, "
              f"{capped.waits_seen} waits seen; most threads held (warren.backends()): "
              f"{uncapped.most_held} uncapped, {capped.most_held} capped", flush=True)
        failures += [f"uncapped: {failure}" for failure in uncapped.failures]
        failures += [f"capped: {failure}" for failure in capped.failures]
        if uncapped.cores <= 2 * args.cap:
            failures.append(f"uncapped, the compute threads used {uncapped.cores:.2f} cores, not more than "
                            f"twice the cap ({args.cap}): the measurement cannot tell")
        log = cluster.directory / "server.log"
        if enforced:
            if not compute_threads.within_cap(capped, args.cap):
                failures.append(f"under a cap of {args.cap}, the compute threads used {capped.cores:.2f} cores")
            if capped.waits_seen == 0:
                failures.append("no backend was seen waiting for threads")
            failures += [f"capped: {problem}" for problem in compute_threads.held_problems(capped, args.cap)]
            if uncapped.most_held <= args.cap:
                failures.append(f"uncapped, warren.backends() showed at most {uncapped.most_held} threads held")
            failures += compute_threads.cancels_and_exits(cluster)
        elif compute_threads.NOT_ENFORCED not in log.read_text():
            failures.append("the cap is not enforced, and the server log does not say so")
        failures += compute_threads.per_call_limits(cluster)
        failures += [f"server log: {line}" for line in server_log.problems(log.read_text(), ALLOWED)]
    return failures


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
