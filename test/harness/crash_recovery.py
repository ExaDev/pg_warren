"""Crash recovery during `refresh` (DESIGN.md §13, M11 "Robustness"; §5.1,
§5.6, §7).

A version is built inside its refresh's transaction, so a refresh that
never commits leaves nothing behind (§5.1): its version row, segments and
id are rolled back with it, whether the backend is terminated (its
transaction aborts), killed (the postmaster then resets every backend and
shared memory, the shared cache's registry and segments with it, §5.6) or
the server stops at once (the next start recovers from the WAL). No
cleanup job is needed, and none exists.

Each scenario interrupts a refresh of a graph big enough to stop at a
chosen point (`POINTS`) in one way (`KINDS`), recovers, and checks
(`Scenario.check`) that the catalog is exactly as before (versions,
segments, vectors and the current version), that the graph answers as
before, that the killed backend left no row in `warren.backends()` or
`warren.progress()` and no pin in the shared cache, that the shared cache
loads and shares the current version again, and that the server log holds
nothing but what the interruption explains. After each kind, a refresh
succeeds and builds the source."""

import os
import signal
import threading
import time
from dataclasses import dataclass

from . import server_log, shared_cache
from .session import BackendLost, Session

GRAPH = "big"
GATE = 7  # the advisory lock the edge query takes, shared: held exclusively, it stops a refresh reading edges
NODE_QUERY = "SELECT id FROM public.nodes"
EDGE_QUERY = f"SELECT src, dst FROM public.edges, (SELECT pg_advisory_xact_lock_shared({GATE})) AS gate"
WRITTEN = 1 << 20  # segment bytes a refresh has written when it is stopped writing them
RECOVERY_SECONDS = 120
JOIN_SECONDS = 60  # for a stopped refresh's worker to see its backend gone

# Where a refresh is stopped: waiting to read its edges (no version row yet);
# writing its segments (its version row and some segments written); and
# after it returned, before its transaction commits (everything written).
POINTS = ("reading edges", "writing segments", "before commit")
# How: pg_terminate_backend (SIGTERM, the transaction aborts); SIGKILL of the
# backend (the postmaster reinitialises); and an immediate shutdown and a
# restart (WAL recovery).
KINDS = ("terminate", "kill", "immediate")

# What each kind may leave in the server log (SQLSTATEs of ERROR, WARNING
# and FATAL lines, and other lines it explains): the terminated backend's
# FATAL (57P01); with a crash, the postmaster's report of the killed
# backend, the other backends it ends (57P02, or 57P01 at an immediate
# shutdown) and connections refused while it recovers (57P03).
LOG = {
    "terminate": ({"57P01"}, ()),
    "kill": ({"57P01", "57P02", "57P03"},
             (r"server process \(PID \d+\) was terminated by signal 9: Killed",
              r"terminating any other active server processes")),
    "immediate": ({"57P01", "57P02", "57P03"}, ()),
}

SNAPSHOT = {
    "versions": "SELECT version_id, seq, staged, node_count, id_space, edge_count, bytes FROM warren.versions "
                "ORDER BY version_id",
    "segments": "SELECT version_id, segment, count(*), sum(length(data)) FROM warren.segments "
                "GROUP BY version_id, segment ORDER BY version_id, segment",
    "vectors": "SELECT v.vector_id, v.version_id, v.name, count(c.chunk_no), sum(length(c.data)) "
               "FROM warren.vectors v LEFT JOIN warren.vector_chunks c USING (vector_id) "
               "GROUP BY v.vector_id ORDER BY v.vector_id",
    "current": f"SELECT current_version FROM warren.graphs WHERE name = '{GRAPH}'",
    "answers": f"SELECT (SELECT count(*) FROM warren.reachable_set('{GRAPH}', ARRAY[1::bigint], 3)), "
               f"warren.degree('{GRAPH}', 1::bigint, direction := 'both'), "
               f"warren.vector_reduce('{GRAPH}', 'ones', 'sum', "
               f"(SELECT version_id FROM warren.vectors WHERE name = 'ones'))",
}


def setup_sql(nodes, edges):
    """The source tables (a ring with a chord per edge row), the graph and a
    persisted vector; its first version is built by `refresh` after."""
    return (f"CREATE TABLE public.nodes AS SELECT generate_series(1, {nodes})::bigint AS id;\n"
            f"CREATE TABLE public.edges AS SELECT i % {nodes} + 1 AS src, (i * 7919) % {nodes} + 1 AS dst "
            f"FROM generate_series(1::bigint, {edges}) i;\n"
            f"SELECT warren.create_graph('{GRAPH}', $q${NODE_QUERY}$q$, $q${EDGE_QUERY}$q$);\n")


def differences(before, after):
    """What differs between two `SNAPSHOT`s, by part."""
    return [f"{part} changed: {before[part]} → {after[part]}" for part in SNAPSHOT if before[part] != after[part]]


def log_problems(text, kind):
    allowed, expected = LOG[kind]
    return [f"server log: {line}" for line in server_log.problems(text, allowed, expected)]


@dataclass
class Interrupted:
    """What an interruption saw: the backend it stopped, the phase
    `warren.progress()` showed for it then (where another backend's progress
    shows) and the version it was building, if known."""
    pid: int
    phase: str | None
    version: str | None


class Scenario:
    """One refresh of `cluster`'s database `database`, interrupted at `point` by `kind`."""

    def __init__(self, cluster, database, point, kind):
        self.cluster, self.database, self.point, self.kind = cluster, database, point, kind
        self.interrupted = None

    def session(self):
        return Session(self.cluster, self.database)

    def snapshot(self, s):
        return {part: s.run(sql).rows for part, sql in SNAPSHOT.items()}

    def run(self):
        """Interrupts a refresh, recovers, and returns what is wrong."""
        log_from = self.cluster.log.stat().st_size
        with self.session() as s:
            before = self.snapshot(s)
        interrupted = self.interrupted = self.interrupt()
        self.recover()
        with self.session() as s:
            failures = differences(before, self.snapshot(s))
            failures += self.leftovers(s, interrupted)
            failures += shared_cache_works(self, s)
        failures += self.point_reached(interrupted)
        with open(self.cluster.log) as log:
            log.seek(log_from)
            failures += log_problems(log.read(), self.kind)
        return failures

    def interrupt(self):
        """Stops a refresh at `point` by `kind`; what it saw then. Its
        sessions are closed and its worker ended however it ends: the
        monitor otherwise kept holding the gate, and the next scenario's
        refresh waited for it."""
        sessions, workers = [], []
        try:
            interrupted = self._interrupt(sessions, workers)
        except BaseException:
            end(sessions, workers)
            raise
        if not end(sessions, workers):
            raise RuntimeError(f"the refresh stopped at {self.point} did not end within {JOIN_SECONDS} s")
        return interrupted

    def _interrupt(self, sessions, workers):
        monitor = self.session()
        sessions.append(monitor)
        refresher = self.session()
        sessions.append(refresher)
        # The refresher reads the current version first, so that it maps it
        # (shared) or holds it (private) when it is stopped.
        refresher.run(f"SELECT warren.degree('{GRAPH}', 1::bigint)").value()
        outcome = {}

        def refresh(sql):
            try:
                outcome["refresh"] = refresher.run(sql)
            except BackendLost as lost:
                outcome["lost"] = lost

        if self.point == "reading edges":
            monitor.run(f"SELECT pg_advisory_lock({GATE})").value()
            workers.append(refreshing(refresh))
            until(monitor, f"SELECT count(*) = 0 FROM pg_stat_activity WHERE pid = {refresher.pid} "
                           "AND wait_event_type = 'Lock'")
        elif self.point == "writing segments":
            base = int(monitor.run("SELECT pg_total_relation_size('warren.segments')").value())
            workers.append(worker := refreshing(refresh))
            until(monitor, f"SELECT pg_total_relation_size('warren.segments') <= {base + WRITTEN}",
                  give_up=lambda: not worker.is_alive())
        else:
            refresher.run("BEGIN")
            refresher.run(f"SELECT warren.refresh('{GRAPH}')").value()
        seen = monitor.run(f"SELECT phase, version FROM warren.progress() WHERE pid = {refresher.pid}").rows
        phase, version = (field or None for field in seen[0]) if seen else (None, None)
        interrupted = Interrupted(refresher.pid, phase, version)
        self.stop(monitor, refresher.pid)
        for worker in workers:
            worker.join(JOIN_SECONDS)
        if "refresh" in outcome and outcome["refresh"].ok:
            raise RuntimeError(f"the refresh finished before it was stopped at {self.point}")
        return interrupted

    def stop(self, monitor, pid):
        if self.kind == "terminate":
            monitor.run(f"SELECT pg_terminate_backend({pid}, 10000)").value()
        elif self.kind == "kill":
            os.kill(pid, signal.SIGKILL)
        else:
            self.cluster.stop_immediately()

    def recover(self):
        if not self.cluster.running():
            self.cluster.start()
        deadline = time.monotonic() + RECOVERY_SECONDS
        while True:
            try:
                with self.session() as s:
                    if s.run("SELECT 1").ok:
                        return
            except (BackendLost, RuntimeError, ValueError):
                pass
            if time.monotonic() > deadline:
                raise RuntimeError(f"the server did not accept connections {RECOVERY_SECONDS} s after {self.kind}")
            time.sleep(0.2)

    def leftovers(self, s, interrupted):
        failures = []
        for view in ("warren.backends()", "warren.progress()"):
            rows = s.run(f"SELECT count(*) FROM {view} WHERE pid = {interrupted.pid}").value()
            if rows != "0":
                failures.append(f"{view} still lists the stopped backend {interrupted.pid}")
        pinned = s.run("SELECT count(*) FROM warren.shared_cache() WHERE pins <> 0").value()
        if pinned != "0":
            failures.append(f"{pinned} shared entries are still pinned")
        if interrupted.version is not None:
            left = s.run(f"SELECT count(*) FROM warren.versions WHERE version_id = {interrupted.version}").value()
            if left != "0":
                failures.append(f"the interrupted version {interrupted.version} is recorded")
        return failures

    def point_reached(self, interrupted):
        """Where another backend's progress shows: that the refresh was
        stopped in the phase `point` names."""
        phases = {"reading edges": {"reading edges"}, "writing segments": {"building", "writing"},
                  "before commit": {None}}[self.point]
        if interrupted.phase is None and self.point != "before commit":
            return []
        if interrupted.phase not in phases:
            return [f"stopped at {interrupted.phase!r}, not at {self.point}"]
        return []


def shared_cache_works(scenario, s):
    """One backend loads the current version and another reads it: from the
    shared cache, where the cluster shares versions."""
    current = s.run(SNAPSHOT["current"]).value()
    failures = []
    with scenario.session() as loader:
        loaded = loader.run(f"SELECT warren.prewarm('{GRAPH}')")
        if not loaded.ok:
            return [f"prewarm after recovery failed: {loaded.sqlstate} {loaded.message}"]
        with scenario.session() as reader:
            read = reader.run(SNAPSHOT["answers"])
            if not read.ok:
                failures.append(f"a read after recovery failed: {read.sqlstate} {read.message}")
            if s.run(shared_cache.SHARES).value() == "t":
                shared = s.run(f"SELECT count(*) FROM warren.shared_cache() WHERE kind = 'version' "
                               f"AND version = {current}").value()
                hits = s.run(f"SELECT shared_hits FROM warren.backends() WHERE pid = {reader.pid}").value()
                if shared != "1" or int(hits) < 1:
                    failures.append(f"after recovery the shared cache lists the current version {shared} times "
                                    f"and the reader's shared hits are {hits}")
    return failures


def refresh_builds_the_source(s):
    """A refresh succeeds, and its version holds the source's nodes and edges."""
    done = s.run(f"SELECT warren.refresh('{GRAPH}')")
    if not done.ok:
        return [f"a refresh after recovery failed: {done.sqlstate} {done.message}"]
    stats = s.run(f"SELECT node_count, edge_count FROM warren.versions WHERE version_id = {done.value()}").rows
    source = s.run("SELECT (SELECT count(*) FROM public.nodes), "
                   "(SELECT count(*) FROM (SELECT DISTINCT src, dst FROM public.edges) e)").rows
    if stats != source:
        return [f"the refresh after recovery built (nodes, edges) {stats}, the source has {source}"]
    return []


def end(sessions, workers):
    """Closes `sessions` (the monitor first: it releases the gate) and
    waits for `workers`; whether they all ended."""
    for session in sessions:
        session.close()
    for worker in workers:
        worker.join(JOIN_SECONDS)
    return not any(worker.is_alive() for worker in workers)


def refreshing(refresh):
    """A started daemon thread that runs `refresh` of the graph: one left
    behind never holds the run open."""
    worker = threading.Thread(target=refresh, args=(f"SELECT warren.refresh('{GRAPH}')",), daemon=True)
    worker.start()
    return worker


def until(s, sql, give_up=lambda: False, seconds=RECOVERY_SECONDS):
    """Polls `sql`, a boolean, while it is true; fails if it still is after
    `seconds`, or once `give_up()`."""
    deadline = time.monotonic() + seconds
    while s.run(sql).value() == "t":
        if give_up() or time.monotonic() > deadline:
            raise RuntimeError(f"gave up waiting while: {sql}")
        time.sleep(0.005)
