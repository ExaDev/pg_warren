#!/usr/bin/env python3
"""SQL-surface fuzzing (DESIGN.md §13, M11 "Robustness"): installs
pg_warren (unless --no-install), and for each shared-cache mode (default:
preload, then on) starts a scratch cluster, builds the fixture
(harness/sql_fuzz/fixture.py) in a template database and, for each seed,
calls every warren.* function with generated arguments, as roles of
different privilege, in transactions and savepoints, for --seconds,
judging every outcome (harness/sql_fuzz/verdict.py) and checking what is
left (harness/sql_fuzz/settled.py) and the server log. Each failure is
shrunk and written, with its seed, as a psql script that reproduces it
from a fresh cluster. Holds /tmp/pgrx-test.lock for the whole run (unless
--no-lock, for a --pg-config tree nobody else installs into).

Seeds are integers, or `time` for one taken from the clock (printed)."""

import argparse
import os
import random
import sys
import tempfile
import time
from contextlib import nullcontext
from pathlib import Path

from harness import extension, server_log, shared_cache
from harness.cluster import SUPERUSER, Cluster
from harness.session import Session
from harness.sql_fuzz import fixture, surface
from harness.sql_fuzz.actions import Planner
from harness.sql_fuzz.run import Run
from harness.sql_fuzz.settled import settled
from harness.sql_fuzz.shrink import Shrinker, fixture_script, script
from harness.sql_fuzz.verdict import allowed

TEMPLATE = "fuzz_template"
SETTINGS = (*server_log.SETTINGS, "log_min_error_statement = warning")
# What the server log may show besides errors of allowed SQLSTATEs: the
# library's WARNINGs that an iteration did not converge, and a parallel
# worker that exits after the error it raised (the leader raises it again).
EXPECTED = (r"WARNING:  \w+ did not (converge|stabilise) in \d+ iterations",
            r'background worker "parallel worker" \(PID \d+\) exited with exit code 1')


class AllowedStates:
    """The SQLSTATEs the server log may show: those the oracle allows."""

    def __contains__(self, sqlstate):
        return allowed(sqlstate)


def main(argv):
    options = argparse.ArgumentParser(description=__doc__)
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"))
    options.add_argument("--no-install", action="store_true", help="use the pg_warren already installed")
    options.add_argument("--no-lock", action="store_true",
                         help="do not hold /tmp/pgrx-test.lock (a private --pg-config tree; implies --no-install)")
    options.add_argument("--runas", default="postgres", help="cluster user when run as root (default postgres)")
    options.add_argument("--keep", action="store_true", help="keep the cluster directories for inspection")
    options.add_argument("--seconds", type=float, default=30, help="how long each seed runs, per mode (default 30)")
    options.add_argument("--seeds", nargs="+", default=["1", "time"], help="the seeds (default: 1 time)")
    options.add_argument("--shared-cache", nargs="+", choices=sorted(shared_cache.MODES), default=["preload", "on"],
                         help="the shared-cache modes to run in (default: preload on)")
    options.add_argument("--shrink-seconds", type=float, default=60, help="time to shrink each failure (default 60)")
    options.add_argument("--out", type=Path, help="where to write the failures' scripts (default: a new temporary "
                                                  "directory)")
    args = options.parse_args(argv)
    seeds = [int(time.time()) if seed == "time" else int(seed) for seed in args.seeds]
    out = args.out or Path(tempfile.mkdtemp(prefix="warren-sql-fuzz-"))
    out.mkdir(parents=True, exist_ok=True)
    print(f"seeds {' '.join(map(str, seeds))}; failures are written to {out}", flush=True)
    failed = False
    with nullcontext() if args.no_lock else extension.shared_lock():
        if not (args.no_install or args.no_lock):
            extension.install(args.pg_config)
        for mode in args.shared_cache:
            failures = run_mode(args, mode, seeds, out)
            failed = failed or bool(failures)
            for failure in failures:
                print(f"  {failure}")
            print(f"sql fuzz ({mode}): {'FAILED' if failures else 'ok'}", flush=True)
    return 1 if failed else 0


def run_mode(args, mode, seeds, out):
    failures = []
    with Cluster(args.pg_config, args.runas, args.keep, settings=(*shared_cache.settings(mode), *SETTINGS)) as cluster:
        functions, parallel = build_template(cluster)
        shrinker = Shrinker(cluster, TEMPLATE, args.shrink_seconds)
        for seed in seeds:
            failures += run_seed(cluster, functions, parallel, seed, args.seconds, mode, shrinker, out)
    return failures


def build_template(cluster):
    """Builds the fixture in the template database; the surface, and the
    server's name for forcing parallel plans."""
    cluster.database().execute(f"CREATE DATABASE {TEMPLATE}")
    cluster.database(TEMPLATE).execute("CREATE EXTENSION pg_warren")
    sessions = {}
    try:
        for role, sql in fixture.steps():
            if role not in sessions:
                sessions[role] = Session(cluster, TEMPLATE, user=role)
            done = sessions[role].run(sql)
            if not done.ok:
                raise RuntimeError(f"fixture, as {role}: {done.sqlstate} {done.message}\n{sql[:300]}")
        functions = surface.parse(sessions[SUPERUSER].run(surface.SURFACE_SQL).value())
        version = int(sessions[SUPERUSER].run("SHOW server_version_num").value())
    finally:
        for session in sessions.values():
            session.close()
    return functions, "debug_parallel_query" if version >= 160000 else "force_parallel_mode"


def run_seed(cluster, functions, parallel, seed, seconds, mode, shrinker, out):
    """Fuzzes a copy of the template with `seed` for `seconds`; what failed,
    the server log's trouble in that time included (but not the replays'
    while shrinking)."""
    database = f"fuzz_{seed}"
    log_start = cluster.log.stat().st_size
    cluster.database().execute(f"CREATE DATABASE {database} TEMPLATE {TEMPLATE}")
    planner = Planner(random.Random(seed), functions, parallel)
    run = Run(cluster, database)
    deadline = time.monotonic() + seconds
    try:
        while time.monotonic() < deadline:
            run.refresh_world()
            run.perform(planner.next(run.world))
    finally:
        run.close()
    print(f"seed {seed} ({mode}): {planner.made} actions, {run.statements} statements, "
          f"{len(run.failures)} distinct failures", flush=True)
    failures = [f"seed {seed}: {problem}" for problem in settled(cluster, database)]
    with open(cluster.log, "rb") as log:
        log.seek(log_start)
        text = log.read().decode(errors="replace")
    failures += [f"seed {seed}: server log: {line}" for line in server_log.problems(text, AllowedStates(), EXPECTED)]
    for number, failure in enumerate(run.failures.values()):
        failures.append(report(failure, seed, mode, number, shrinker, out))
    cluster.database().execute(f"DROP DATABASE {database} WITH (FORCE)")
    return failures


def report(failure, seed, mode, number, shrinker, out):
    """Shrinks `failure`, writes its script, and says what failed where."""
    shrunk = shrinker.shrink(failure)
    actions = shrunk or failure.transcript
    path = out / f"{mode}-seed{seed}-{number}.sql"
    path.write_text(f"-- pg_warren SQL fuzz: {failure.finding.kind}: {failure.finding.detail}\n"
                    f"-- seed {seed}, shared cache {mode}, seen {failure.occurrences} times; "
                    f"{'shrunk' if shrunk else 'not reproduced on replay; the whole transcript'}.\n"
                    f"-- Run in a fresh database of a fresh cluster: CREATE EXTENSION pg_warren, then\n"
                    f"-- psql -f this file.\n{fixture_script()}\n-- The failure\n{script(actions)}\n")
    shown = script(actions[-3:]) if shrunk else f"(not reproduced; {len(actions)} actions)"
    return (f"seed {seed}: {failure.finding.kind} in {failure.signature[0]}: {failure.finding.detail[:500]} "
            f"(x{failure.occurrences}; {path})\n{shown}")


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
