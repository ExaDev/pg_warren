"""Warming each pool's server backends before the pooler soak's clock
starts (DESIGN.md §13 M11), so that the steady state begins at every
backend's working set.

A backend's anonymous memory is a high-water mark: glibc keeps what a
call's peak took, so it rises in steps the first few times the backend
runs each kind of call (a refresh about 4 MB, then less) and then stays
flat. Only one client in a pool writes, and the pooler decides which
backend serves it (session pooling keeps it on one backend for its
connection, transaction pooling hands out its last released connection
first), so without this a backend may first serve a write well into the
steady window, and its warm-up would look like growth. `warm` opens as
many sessions as the pool has server connections, each pinned to its own
backend by an open transaction, and runs every kind of call on each,
`PASSES` times, before the transaction commits."""

import random

from .fixture import edit_sql
from .workload import READS, traversal_sql

PASSES = 3


def calls(rng, nodes, name):
    """One pass of every kind of call the soak's clients make, in one
    transaction; `name` is the persisted vector it writes and drops."""
    graph_reads = [traversal_sql(graph, kind, rng.randint(1, nodes), rng.randint(1, nodes))
                   for graph in ("g", "t") for kind in READS]
    current = "(SELECT (warren.info('g') ->> 'current_version')::bigint)"
    return [edit_sql(rng, nodes), "SELECT warren.refresh('g')", "SELECT warren.refresh('t')",
            "SELECT warren.drop_version('g', warren.refresh('g', promote := false))", "SELECT warren.compact('g')",
            *graph_reads,
            f"SELECT warren.pagerank('g', '{name}', max_iter := 10, version := {current})",
            f"SELECT warren.vector_reduce('g', '{name}', 'sum', {current})",
            f"SELECT warren.drop_vector('g', '{name}', {current})",
            f"SELECT warren.vector_const('g', '{name}_x', 1, persist := false, version := {current})",
            f"SELECT warren.spmv('g', '{name}_x', '{name}_y', weighted := true, persist := false, "
            f"version := {current})",
            *(f"SELECT warren.drop_vector('g', '{name}_{v}', {current})" for v in "xy"),
            "SELECT (SELECT count(*) FROM warren.cache()), (SELECT count(*) FROM warren.backends()), "
            "(SELECT count(*) FROM warren.progress()), (SELECT count(*) FROM warren.shared_cache())"]


def warm(connect, pool_size, nodes, seed):
    """Runs every kind of call `PASSES` times on each of a pool's
    `pool_size` server backends, through sessions `connect()` opens; the
    pids of the backends warmed."""
    rng = random.Random(f"{seed}-warm")
    sessions = []
    try:
        pids = set()
        for _ in range(pool_size):
            sessions.append(connect())
            _run(sessions[-1], "BEGIN")
            pids.add(_run(sessions[-1], "SELECT pg_backend_pid()").value())
        if len(pids) != pool_size:
            raise RuntimeError(f"warming: {len(pids)} server backends for {pool_size} sessions")
        for number, session in enumerate(sessions):
            for _ in range(PASSES):
                for sql in calls(rng, nodes, f"warm{number}"):
                    _run(session, sql)
            _run(session, "COMMIT")
        return pids
    finally:
        for session in sessions:
            session.close()


def _run(session, sql):
    done = session.run(sql)
    if not done.ok:
        raise RuntimeError(f"warming: {sql[:200]}: {done.sqlstate} {done.message}")
    return done
