#!/usr/bin/env python3
"""A soak behind a connection pooler, watching for memory growth
(DESIGN.md §13, M11 "Robustness"): installs pg_warren (unless
--no-install), starts a scratch cluster with the shared cache in
--shared-cache's mode, and puts PgBouncer in front of it in each pool
mode (default: transaction and session at once, each its own login role
and pool of --pool-size server connections for --clients client
connections). It first probes what each pool mode keeps of a client's
session state (harness/pooler_soak/probes.py), then runs the clients'
mix of reads, compute, persisted vectors, introspection, timeouts,
cancels, disconnects in transactions and writes that retire versions
(harness/pooler_soak/workload.py) for --minutes or --hours, sampling
every pooled backend's memory, pg_warren's accounting, the DSM segments,
the catalog and the poolers (harness/pooler_soak/samples.py). It fails
on growth in the steady state after the warm-up (a robust slope,
harness/pooler_soak/verdict.py), on anything left behind once the
clients and then the poolers have stopped (harness/pooler_soak/settled.py),
on a client failure with an SQLSTATE its operation does not allow, on
session state that session pooling loses or leaks, and on trouble in
the server log. Writes samples.csv and summary.json to --out.

Holds /tmp/pgrx-test.lock for the whole run, unless --no-lock (for a
--pg-config tree nobody else installs into; implies --no-install), which
a long soak should use:

    test/pooler_soak.py --no-install --pg-config "$(command -v pg_config)"   # CI: 2 min
    test/pooler_soak.py --no-lock --hours 48 --pg-config $T/usr/lib/postgresql/16/bin/pg_config --out /var/tmp/soak"""

import argparse
import math
import os
import signal
import sys
import tempfile
import threading
import time
from contextlib import ExitStack, nullcontext
from pathlib import Path

from harness import extension, server_log, shared_cache
from harness.cluster import Cluster
from harness.pooler import MODES, PGBOUNCER, Pooler
from harness.pooler_soak import probes, report, settled, verdict
from harness.pooler_soak.fixture import DATABASE, GRAPHS, role, setup_sql
from harness.pooler_soak.samples import Sampler
from harness.pooler_soak.workload import Chaos, Tally, Workload
from harness.session import Session

PROBE_PORT = 6439
FIRST_PORT = 6440
# What the server log may show: cancels and statement timeouts, a version
# retired under a call that named it, a lock timeout, a staged version a
# compaction superseded, a vector a compaction moved, a client gone in a
# call; and, from the transaction pool's probes, a transient vector or a
# prepared statement not found on the server the next transaction got.
ALLOWED_STATES = {"57014", "42704", "55P03", "55000", "40001", "08006", "08P01", "26000"}
JUDGED = {*verdict.BACKEND_TOLERANCES, *verdict.CLUSTER_TOLERANCES}
# Autovacuum wakes every 5 s (the default is 1 min), so that the dead
# tuples of the catalog's churn (vectors written and dropped, versions
# retired) are reclaimed within even a short run's steady state.
SETTINGS = ("autovacuum_naptime = '5s'",)


def options(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"))
    parser.add_argument("--no-install", action="store_true", help="use the pg_warren already installed")
    parser.add_argument("--no-lock", action="store_true",
                        help="do not hold /tmp/pgrx-test.lock (a private --pg-config tree; implies --no-install)")
    parser.add_argument("--runas", default="postgres", help="cluster and pgbouncer user when run as root")
    parser.add_argument("--keep", action="store_true", help="keep the cluster directory for inspection")
    parser.add_argument("--pgbouncer", default=PGBOUNCER)
    length = parser.add_mutually_exclusive_group()
    length.add_argument("--minutes", type=float, help="how long the clients run (default 2, as CI runs it)")
    length.add_argument("--hours", type=float, help="how long the clients run, for a long soak")
    parser.add_argument("--modes", nargs="+", choices=MODES, default=list(MODES), help="pool modes, run at once")
    parser.add_argument("--shared-cache", choices=sorted(shared_cache.MODES), default="preload")
    parser.add_argument("--clients", type=int, default=48, help="client connections per pool (default 48)")
    parser.add_argument("--pool-size", type=int, default=8, help="server connections per pool (default 8)")
    parser.add_argument("--writers", type=int, default=1, help="of each pool's clients, how many write")
    parser.add_argument("--nodes", type=int, default=5000)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--sample-seconds", type=float, help="default: the run / 120, from 5 to 60")
    parser.add_argument("--warmup-seconds", type=float, help="default: a fifth of the run, from 30 s to 30 min")
    parser.add_argument("--chaos-seconds", type=float, help="between cancels and disconnects (default: the run / 12, "
                                                             "from 5 s to 5 min)")
    parser.add_argument("--context-seconds", type=float, default=600,
                        help="how often each backend logs its memory contexts, at most (default 600)")
    parser.add_argument("--think-seconds", type=float, default=0.05, help="a client's pause between transactions")
    parser.add_argument("--write-seconds", type=float, default=2, help="a writer's pause between writes")
    parser.add_argument("--out", type=Path, help="where samples.csv and summary.json go (default: a new temporary "
                                                 "directory)")
    args = parser.parse_args(argv)
    args.seconds = 3600 * args.hours if args.hours else 60 * (args.minutes or 2)
    args.sample_seconds = args.sample_seconds or min(60.0, max(5.0, args.seconds / 120))
    if args.warmup_seconds is None:
        args.warmup_seconds = min(1800.0, max(30.0, args.seconds / 5))
    args.chaos_seconds = args.chaos_seconds or min(300.0, max(5.0, args.seconds / 12))
    args.context_every = max(1, math.floor(min(args.context_seconds, args.seconds / 10) / args.sample_seconds))
    return args


def main(argv):
    args = options(argv)
    # A long soak stopped with SIGTERM (kill, a job's timeout) still stops
    # its poolers and its cluster: the exit unwinds their context managers.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit("pooler soak: terminated"))
    out = args.out or Path(tempfile.mkdtemp(prefix="warren-pooler-soak-"))
    out.mkdir(parents=True, exist_ok=True)
    print(f"pooler soak: {args.seconds:.0f} s, pools {' '.join(args.modes)}, {args.clients} clients over "
          f"{args.pool_size} server connections each, shared cache {args.shared_cache}; a sample every "
          f"{args.sample_seconds:.0f} s, warm-up {args.warmup_seconds:.0f} s; writing to {out}", flush=True)
    with nullcontext() if args.no_lock else extension.shared_lock():
        if not (args.no_install or args.no_lock):
            extension.install(args.pg_config)
        failures = soak(args, out)
    for failure in failures:
        print(f"  {failure}")
    print(f"pooler soak: {'FAILED' if failures else 'ok'}", flush=True)
    return 1 if failures else 0


def soak(args, out):
    roles = {mode: role(mode) for mode in args.modes}
    connections = len(args.modes) * args.pool_size + probes.POOL_SIZE + 20
    settings = (*shared_cache.settings(args.shared_cache), *server_log.SETTINGS, *SETTINGS,
                f"max_connections = {connections}")
    with Cluster(args.pg_config, args.runas, args.keep, settings=settings) as cluster:
        cluster.database().execute(f"CREATE DATABASE {DATABASE}")
        cluster.database(DATABASE).execute("CREATE EXTENSION pg_warren")
        with Session(cluster, DATABASE) as admin:
            for sql in setup_sql(args.nodes, args.modes):
                done = admin.run(sql)
                if not done.ok:
                    raise RuntimeError(f"setup: {sql[:200]}: {done.sqlstate} {done.message}")
            found = []
            for mode in args.modes:
                with Pooler(cluster, mode, DATABASE, [roles[mode]], probes.POOL_SIZE, 8, PROBE_PORT,
                            args.pgbouncer) as pooler:
                    found += probes.probe(pooler, mode, roles[mode])
            for graph in GRAPHS:
                admin.run(f"SELECT warren.evict('{graph}')").value()
            tally, rows, kept = Tally(), [], {}
            with ExitStack() as poolers:
                pools = {mode: (poolers.enter_context(Pooler(cluster, mode, DATABASE, [roles[mode]], args.pool_size,
                                                             args.clients + 8, FIRST_PORT + i, args.pgbouncer)),
                                roles[mode]) for i, mode in enumerate(args.modes)}
                sampler = Sampler(cluster, admin, pools, args.context_every)
                dsm_before = sampler.dsm_count()
                rows = run_clients(args, cluster, pools, sampler, tally, out)
                failures, kept = settled.after_clients(admin, roles)
            failures += settled.after_poolers(admin, roles, args.clients * len(args.modes), dsm_before,
                                              sampler.dsm_count)
        log = cluster.log.read_text(errors="replace")
    judged = verdict.judge_rows(rows, args.warmup_seconds)
    failures += verdict.failures(judged)
    failures += probes.failures(found)
    failures += [f"{mode} {operation} failed with {sqlstate}, which is not expected: {message}"
                 for mode, operation, sqlstate, message in tally.unexpected]
    failures += tally.mismatches
    failures += [f"server log: {line}" for line in server_log.problems(log, ALLOWED_STATES)]
    failures += [f"no {mode} {operation} succeeded" for mode in args.modes
                 for operation in ("traverse", "stats", "transient", "persist", "prepared", "introspect", "write")
                 if not tally.outcomes[(mode, operation, "00000")]]
    for line in report.lines(judged, found, tally, kept):
        print(line)
    report.write_summary(out / "summary.json", {
        "settings": {key: str(value) for key, value in vars(args).items()},
        "slopes": report.slopes(judged), "probes": [vars(p) for p in found],
        "outcomes": [[*key, n] for key, n in sorted(tally.outcomes.items())],
        "chaos": [[*key, n] for key, n in sorted(tally.chaos.items())],
        "transient_bytes_kept": kept, "failures": failures})
    return failures


def run_clients(args, cluster, pools, sampler, tally, out):
    """Runs every pool's clients and chaos for the run's length, sampling
    as they go; the samples of judged metrics."""
    stop = threading.Event()
    workloads = [Workload(pooler, mode, role_name, args.clients, args.writers, args.nodes, args.seed, tally, stop,
                          args.think_seconds, args.write_seconds) for mode, (pooler, role_name) in pools.items()]
    kept = []
    with Session(cluster, DATABASE) as chaos_admin, report.SampleWriter(out / "samples.csv") as writer:
        chaos = Chaos(chaos_admin, workloads, args.chaos_seconds, args.seed, stop)
        for workload in workloads:
            workload.start()
        chaos.start()
        started = time.monotonic()
        try:
            while (elapsed := time.monotonic() - started) < args.seconds:
                sample = sampler.sample(round(elapsed, 1))
                writer.write(sample)
                kept += [row for row in sample if row[1] in JUDGED]
                progress(sample, tally)
                stop.wait(max(0.0, args.sample_seconds - (time.monotonic() - started - elapsed)))
        finally:
            stop.set()
            chaos.join()
            for workload in workloads:
                workload.join()
    return kept


def progress(sample, tally):
    """One line of the run so far: the pools' backends, their memory, and the catalog."""
    by_metric = {}
    for _, metric, subject, value in sample:
        by_metric.setdefault(metric, {})[subject] = value
    anonymous = by_metric.get("anonymous", {})
    pools = sorted({subject.split("/")[0] for subject in anonymous})
    memory = ", ".join(f"{mode} {sum(1 for s in anonymous if s.startswith(mode))} backends "
                       f"{max(v for s, v in anonymous.items() if s.startswith(mode)) / 2**20:.1f} MiB anon at most"
                       for mode in pools)
    cluster = by_metric.get("segments", {}).get("cluster", 0)
    with tally.lock:
        done, failed = sum(n for (_, _, s), n in tally.outcomes.items() if s == "00000"), \
            sum(n for (_, _, s), n in tally.outcomes.items() if s != "00000")
    print(f"t={sample[0][0]:.0f}s: {done} ok, {failed} failed, {len(tally.unexpected)} unexpected; {memory}; "
          f"{cluster:.0f} segments, {by_metric.get('shared_bytes', {}).get('cluster', 0) / 2**20:.1f} MiB shared, "
          f"{by_metric.get('dsm_segments', {}).get('cluster', 0):.0f} DSM segments", flush=True)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
