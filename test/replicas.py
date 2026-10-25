#!/usr/bin/env python3
"""Physical streaming replicas (DESIGN.md §13, M11 "Robustness"; §5.1):
installs pg_warren (unless --no-install), and for each shared-cache mode
(default: preload, then on) starts a scratch primary, a hot standby of it
(`pg_basebackup -R`) and a cascading standby of the standby, builds graphs,
versions and vectors on the primary and checks, once the standbys have
replayed them, that every read answers there as on the primary, that
every write is refused with 25006, that the shared cache and the activity
table work, that a version retired under a standby query is read
consistently or the query cancelled (40001), that the standby, promoted,
writes and its cascade follows it, and the server logs
(harness/replicas.py). Holds /tmp/pgrx-test.lock for the whole run."""

import argparse
import os
import sys

from harness import extension, replicas, server_log, shared_cache
from harness.cluster import Cluster
from harness.session import Session

DATABASE = "replicas"


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
            print(f"replicas ({mode}): {'FAILED' if failures else 'ok'}", flush=True)
    return 1 if failed else 0


def run_mode(args, mode):
    settings = (*shared_cache.settings(mode), *server_log.SETTINGS, *replicas.SETTINGS)
    failures = []
    with Cluster(args.pg_config, args.runas, args.keep, settings=settings) as primary:
        primary.database().execute(f"CREATE DATABASE {DATABASE}")
        build = primary.database(DATABASE)
        build.execute("CREATE EXTENSION pg_warren")
        build.execute(replicas.setup_sql())
        with Cluster(args.pg_config, args.runas, args.keep, settings=settings, primary=primary) as standby, \
                Cluster(args.pg_config, args.runas, args.keep, settings=settings, primary=standby) as cascade:
            servers = {"primary": primary, "standby": standby, "cascade": cascade}
            try:
                failures += check(primary, standby, cascade)
            finally:
                # Read before teardown deletes the logs, even when a check raised.
                for name, server in servers.items():
                    failures += [f"{name} {problem}" for problem in replicas.log_problems(server.log.read_text())]
                if sys.exc_info()[0] is not None:
                    print("\n".join(failures), file=sys.stderr, flush=True)
    return failures


def check(primary, standby, cascade):
    with Session(primary, DATABASE) as p:
        names = replicas.versions(p.run(replicas.VERSIONS_SQL).rows[0])
        expected = {read: replicas.outcomes(p, statements, names) for read, statements in replicas.READS.items()}
        failures = replicas.read_failures(expected)
        lsn = replicas.flush_lsn(p)
    for name, server in (("standby", standby), ("cascade", cascade)):
        with Session(server, DATABASE) as s:
            replicas.wait_for_replay(s, lsn)
            failures += [f"{name}: {f}" for f in replicas.compare_reads(expected, s, names)]
            failures += [f"{name}: {f}" for f in replicas.refusals(s, names)]
            failures += [f"{name}: {f}" for f in replicas.server_state(s)]
        failures += [f"{name}: {f}" for f in replicas.shared_cache_works(server, DATABASE)]
        print(f"{'ok  ' if not failures else 'FAIL'} {name}: reads, refused writes, shared cache", flush=True)
    for feedback in (False, True):
        retirement = replicas.Retirement(primary, standby, DATABASE, feedback)
        found = retirement.run()
        print(f"{'ok  ' if not found else 'FAIL'} a version retired under standby queries, "
              f"{'with' if feedback else 'without'} hot_standby_feedback ({'; '.join(retirement.seen)})", flush=True)
        failures += found
    found = promoted(primary, standby, cascade)
    print(f"{'ok  ' if not found else 'FAIL'} the standby, promoted, writes and its cascade follows", flush=True)
    return failures + found


def promoted(primary, standby, cascade):
    """Promotes `standby` once it has replayed all of `primary`'s WAL: its
    graphs and vectors read as the primary's, it refreshes and persists a
    vector, and `cascade` replays those."""
    with Session(primary, DATABASE) as p:
        names = replicas.versions(p.run(replicas.VERSIONS_SQL).rows[0])
        expected = {read: replicas.outcomes(p, statements, names) for read, statements in replicas.READS.items()}
        lsn = replicas.flush_lsn(p)
    with Session(standby, DATABASE) as s:
        replicas.wait_for_replay(s, lsn)
    standby.promote()
    with Session(standby, DATABASE) as s:
        failures = [f"promoted: {f}" for f in replicas.compare_reads(expected, s, names)]
        done = s.run("SELECT warren.refresh('social'), warren.vector_const('social', 'after', 3), "
                     "warren.refresh('kg', promote := false)")
        if not done.ok:
            return failures + [f"writing on the promoted standby: {done.sqlstate} {done.message}"]
        follow = "SELECT warren.info('social') ->> 'current_version', warren.vector_reduce('social', 'after', 'sum')"
        wanted = s.run(follow)
        if not wanted.ok or not wanted.rows:
            return failures + [f"reading on the promoted standby: {wanted.sqlstate} {wanted.message or wanted.rows}"]
        wanted = wanted.rows
        lsn = replicas.flush_lsn(s)
    with Session(cascade, DATABASE) as c:
        replicas.wait_for_replay(c, lsn)
        found = c.run(follow)
        if not found.ok or found.rows != wanted:
            failures.append(f"the cascade read {found.sqlstate} {found.message or found.rows} after the promotion, "
                            f"the promoted standby {wanted}")
    return failures


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
