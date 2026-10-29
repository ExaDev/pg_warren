"""Crash recovery during `refresh`, compute and loads (DESIGN.md §13, M11
"Robustness" and 0.9.0 "Lifecycle"; §5.1, §5.6, §6.9, §7).

A version is built inside its refresh's transaction, so a refresh that
never commits leaves nothing behind (§5.1): its version row, segments and
id are rolled back with it, whether the backend is terminated (its
transaction aborts), killed (the postmaster then resets every backend and
shared memory, the shared cache's registry and segments with it, §5.6) or
the server stops at once (the next start recovers from the WAL). No
cleanup job is needed, and none exists.

A compute call persists its vector in its own transaction too, so one
that is stopped leaves no vector; a load into the shared cache that is
stopped gives up its claim as its transaction aborts, and the next
reader (one already waiting on it, or later) loads the version itself.

Each scenario interrupts a call on a graph big enough to stop at a
chosen point (`POINTS`: a refresh, `pagerank` iterating, a read loading
the current version) in one way (`KINDS`), recovers, and checks
(`Scenario.check`) that the catalog is exactly as before (versions,
segments, vectors and the current version), that the graph answers as
before, that the killed backend left no row in `warren.backends()` or
`warren.progress()` and no pin in the shared cache, that the shared cache
loads and shares the current version again, and that the server log holds
nothing but what the interruption explains. A cancelled call fails with
57014 and its backend goes on: it reads, and lists no progress. A reader
that waited while a load was terminated or cancelled reads the version.
After each kind, a refresh succeeds and builds the source."""

import os
import signal
import threading
import time
from dataclasses import dataclass

from . import server_log, shared_cache
from .session import BackendLost, Outcome, Session

GRAPH = "big"
GATE = 7  # the advisory lock the edge query takes, shared: held exclusively, it stops a refresh reading edges
NODE_QUERY = "SELECT id FROM public.nodes"
EDGE_QUERY = f"SELECT src, dst FROM public.edges, (SELECT pg_advisory_xact_lock_shared({GATE})) AS gate"
SOURCE = ("SELECT (SELECT count(*) FROM public.nodes), "
          "(SELECT count(*) FROM (SELECT DISTINCT src, dst FROM public.edges) e)")
REFRESH = f"SELECT warren.refresh('{GRAPH}')"
# A compute that persists its vector and runs until it is stopped (about
# 35 ms an iteration on a debug build).
COMPUTE = f"SELECT warren.pagerank('{GRAPH}', 'rank', max_iter := 1000000, tol := 0)"
# A read that loads the current version on a miss (about 75 ms on a debug build).
LOAD = f"SELECT count(*) FROM warren.reachable_set('{GRAPH}', ARRAY[1::bigint], 3)"
LOAD_ATTEMPTS = 50  # loads tried until one is seen loading
WRITTEN = 1 << 20  # segment bytes a refresh has written when it is stopped writing them
RECOVERY_SECONDS = 120
JOIN_SECONDS = 60  # for a stopped refresh's worker to see its backend gone

# Where a call is stopped: a refresh waiting to read its edges (no version
# row yet), writing its segments (its version row and some segments
# written), and after it returned, before its transaction commits
# (everything written); `pagerank` iterating; and a read loading the
# current version (into the shared cache, where the cluster shares).
POINTS = ("reading edges", "writing segments", "before commit", "computing", "loading")
PHASES = {"reading edges": {"reading edges"}, "writing segments": {"building", "writing"},
          "before commit": {None}, "computing": {"iterating"}, "loading": {"loading"}}
# How: pg_terminate_backend (SIGTERM, the transaction aborts);
# pg_cancel_backend (SIGINT, the statement fails and the backend goes on);
# SIGKILL of the backend (the postmaster reinitialises); and an immediate
# shutdown and a restart (WAL recovery).
KINDS = ("terminate", "cancel", "kill", "immediate")

# What each kind may leave in the server log (SQLSTATEs of ERROR, WARNING
# and FATAL lines, and other lines it explains): the terminated backend's
# FATAL (57P01); the cancelled statement's ERROR (57014); with a crash,
# the postmaster's report of the killed backend, the other backends it
# ends (57P02, or 57P01 at an immediate shutdown) and connections refused
# while it recovers (57P03).
LOG = {
    "terminate": ({"57P01"}, ()),
    "cancel": ({"57014"}, ()),
    "kill": ({"57P01", "57P02", "57P03"},
             # "server process" through PG 16, the backend's type from PG 17.
             (r"(server process|client backend) \(PID \d+\) was terminated by signal 9: Killed",
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


def statement(point):
    """The call stopped at `point`."""
    return {"computing": COMPUTE, "loading": LOAD}.get(point, REFRESH)


def applies(point, kind):
    """Whether `kind` can stop the call at `point`: a cancel interrupts a
    running statement, and an idle transaction (before commit) ignores it."""
    return not (kind == "cancel" and point == "before commit")


def cancel_failures(outcome, read, progress):
    """What is wrong after a cancel: the call's `outcome` must be 57014, a
    `read` after it in the same session must succeed, and
    `warren.progress()` must list `progress` (a count) rows of it."""
    failures = []
    if outcome.sqlstate != "57014":
        failures.append(f"the cancelled call failed with {outcome.sqlstate} {outcome.message or outcome.rows}, "
                        "not 57014")
    if not read.ok:
        failures.append(f"a read in the cancelled call's session failed: {read.sqlstate} {read.message}")
    if progress != "0":
        failures.append("warren.progress() still lists the cancelled call")
    return failures


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
        self.found = []  # what the interruption itself found wrong

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
        failures += self.found + self.point_reached(interrupted)
        with open(self.cluster.log) as log:
            log.seek(log_from)
            failures += log_problems(log.read(), self.kind)
        return failures

    def interrupt(self):
        """Stops the call at `point` by `kind`; what it saw then. Its
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
            raise RuntimeError(f"the call stopped at {self.point} did not end within {JOIN_SECONDS} s")
        return interrupted

    def _interrupt(self, sessions, workers):
        monitor = self.session()
        sessions.append(monitor)
        caller = self.session()
        sessions.append(caller)
        # Other than a load, the call reads the current version first, so
        # that it maps it (shared) or holds it (private) when it is stopped.
        if self.point != "loading":
            caller.run(f"SELECT warren.degree('{GRAPH}', 1::bigint)").value()
        outcome = {}

        def call(sql):
            try:
                outcome["done"] = caller.run(sql)
            except BackendLost as lost:
                outcome["lost"] = lost

        waiter = None
        if self.point == "reading edges":
            monitor.run(f"SELECT pg_advisory_lock({GATE})").value()
            workers.append(calling(call, REFRESH))
            until(monitor, f"SELECT count(*) = 0 FROM pg_stat_activity WHERE pid = {caller.pid} "
                           "AND wait_event_type = 'Lock'")
        elif self.point == "writing segments":
            base = int(monitor.run("SELECT pg_total_relation_size('warren.segments')").value())
            workers.append(worker := calling(call, REFRESH))
            until(monitor, f"SELECT pg_total_relation_size('warren.segments') <= {base + WRITTEN}",
                  give_up=lambda: not worker.is_alive())
        elif self.point == "before commit":
            caller.run("BEGIN")
            caller.run(REFRESH).value()
        elif self.point == "computing":
            workers.append(worker := calling(call, COMPUTE))
            wait_for_phase(monitor, caller.pid, "iterating", worker)
        else:
            waiter = self.load(monitor, caller, call, sessions, workers)
        seen = monitor.run(f"SELECT phase, version FROM warren.progress() WHERE pid = {caller.pid}").rows
        phase, version = (field or None for field in seen[0]) if seen else (None, None)
        interrupted = Interrupted(caller.pid, phase, version)
        self.stop(monitor, caller.pid)
        for worker in workers:
            worker.join(JOIN_SECONDS)
        if "done" in outcome and outcome["done"].ok:
            raise RuntimeError(f"the call finished before it was stopped at {self.point}")
        self.found += self.after_stop(monitor, caller, outcome, waiter)
        return interrupted

    def after_stop(self, monitor, caller, outcome, waiter):
        """What is wrong once the call is stopped: after a cancel, in the
        caller's session (`cancel_failures`); and what the `waiter`, if
        any, read."""
        found = []
        if self.kind == "cancel":
            stopped = outcome.get("done") or Outcome([], "lost", str(outcome.get("lost")))
            found += cancel_failures(stopped, caller.run(LOAD), monitor.run(
                f"SELECT count(*) FROM warren.progress() WHERE pid = {caller.pid}").value())
        if waiter is not None:
            found += waiter.failures()
        return found

    def load(self, monitor, loader, call, sessions, workers):
        """Starts `loader` loading the current version; once it is seen
        loading, for a terminate or a cancel, a `Waiter` reading it too.
        The expected answer is read in a session of its own, closed before
        the load: a backend that maps a version keeps it from being
        evicted, so the next read would find it rather than load it."""
        with self.session() as reader:
            expected = reader.run(LOAD).rows
        waiting = None
        if self.kind in ("terminate", "cancel"):
            sessions.append(waiting := self.session())
        workers.append(loading(monitor, loader, call))
        return None if waiting is None else Waiter(waiting, expected)

    def stop(self, monitor, pid):
        if self.kind == "terminate":
            monitor.run(f"SELECT pg_terminate_backend({pid}, 10000)").value()
        elif self.kind == "cancel":
            monitor.run(f"SELECT pg_cancel_backend({pid})").value()
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
        if interrupted.version is not None and statement(self.point) == REFRESH:
            left = s.run(f"SELECT count(*) FROM warren.versions WHERE version_id = {interrupted.version}").value()
            if left != "0":
                failures.append(f"the interrupted version {interrupted.version} is recorded")
        return failures

    def point_reached(self, interrupted):
        """Where another backend's progress shows: that the call was
        stopped in the phase `point` names."""
        if interrupted.phase is None and self.point != "before commit":
            return []
        if interrupted.phase not in PHASES[self.point]:
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
    source = count_source(s)
    if not source.ok:
        return [f"counting the source after recovery failed: {source.sqlstate} {source.message}"]
    if stats != source.rows:
        return [f"the refresh after recovery built (nodes, edges) {stats}, the source has {source.rows}"]
    return []


def count_source(s):
    """The source's nodes and distinct edges, counted by sorting: a
    parallel HashAggregate of the unanalysed edge rows now and then spills
    into some 10^5 batches and runs for minutes, past the session's
    statement_timeout (Postgres 16 and 18 alike, pg_warren loaded or not)."""
    sorting = s.run("SET enable_hashagg = off")
    if not sorting.ok:
        return sorting
    counted = s.run(SOURCE)
    s.run("RESET enable_hashagg")
    return counted


def end(sessions, workers):
    """Closes `sessions` (the monitor first: it releases the gate) and
    waits for `workers`; whether they all ended."""
    for session in sessions:
        session.close()
    for worker in workers:
        worker.join(JOIN_SECONDS)
    return not any(worker.is_alive() for worker in workers)


def calling(call, sql):
    """A started daemon thread that runs `call(sql)`: one left behind never
    holds the run open."""
    worker = threading.Thread(target=call, args=(sql,), daemon=True)
    worker.start()
    return worker


def visible(monitor, pid):
    """Whether session `monitor` sees backend `pid`'s progress: the
    activity table exists (PG 17 and later, or pg_warren preloaded)."""
    return monitor.run(f"SELECT count(*) FROM warren.backends() WHERE pid = {pid}").rows == [["1"]]


def wait_for_phase(monitor, pid, phase, worker):
    """Waits until backend `pid`, running `worker`'s call, is in `phase`;
    where its progress does not show, until its statement runs."""
    if visible(monitor, pid):
        until(monitor, f"SELECT count(*) = 0 FROM warren.progress() WHERE pid = {pid} AND phase = '{phase}'",
              give_up=lambda: not worker.is_alive())
    else:
        until(monitor, f"SELECT count(*) = 0 FROM pg_stat_activity WHERE pid = {pid} AND state = 'active'",
              give_up=lambda: not worker.is_alive())


def loading(monitor, loader, call, attempts=LOAD_ATTEMPTS):
    """A started thread in which session `loader` loads the current
    version, once `monitor` has seen it loading: each attempt evicts the
    version (the superuser owns the graph, so from the shared cache too)
    and reads it, until one is seen before it finishes."""
    for _ in range(attempts):
        loader.run(f"SELECT warren.evict('{GRAPH}')").value()
        worker = calling(call, LOAD)
        try:
            wait_for_phase(monitor, loader.pid, "loading", worker)
            return worker
        except RuntimeError:
            worker.join(JOIN_SECONDS)
            if worker.is_alive():
                raise
    raise RuntimeError(f"no read was seen loading in {attempts} attempts")


class Waiter:
    """Session `s` reading the current version while another backend loads
    it, on a daemon thread: once that load is stopped, it must read it
    (waiting for the load, then loading it itself) as `expected`."""

    def __init__(self, s, expected):
        self.expected, self.result = expected, None
        self.thread = threading.Thread(target=self._run, args=(s,), daemon=True)
        self.thread.start()

    def _run(self, s):
        try:
            self.result = s.run(LOAD)
        except BackendLost as lost:
            self.result = Outcome([], "lost", str(lost))

    def failures(self):
        self.thread.join(JOIN_SECONDS)
        if self.thread.is_alive():
            return [f"a reader waiting on the stopped load did not finish in {JOIN_SECONDS} s"]
        if not self.result.ok or self.result.rows != self.expected:
            return [f"a reader waiting on the stopped load read {self.result.sqlstate} "
                    f"{self.result.message or self.result.rows}, not {self.expected}"]
        return []


def until(s, sql, give_up=lambda: False, seconds=RECOVERY_SECONDS):
    """Polls `sql`, a boolean, while it is true; fails if it still is after
    `seconds`, or once `give_up()`."""
    deadline = time.monotonic() + seconds
    while s.run(sql).value() == "t":
        if give_up() or time.monotonic() > deadline:
            raise RuntimeError(f"gave up waiting while: {sql}")
        time.sleep(0.005)
