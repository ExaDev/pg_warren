"""Compute threads across the cluster (DESIGN.md §14 item 52): many
backends computing at once, measured from outside. Each backend's compute
threads are its threads but the main one, whose CPU time /proc reports:
the process's (which keeps that of threads that have exited, as pools are
rebuilt) less the main thread's. Over a window in which every session
computes, the compute threads of all of them together may use at most
`warren.cluster_threads_limit` cores, and without the cap they use more.
Also: a backend waiting for threads shows the wait event, a cancel and
`statement_timeout` end its wait, a backend terminated mid-kernel gives its
threads back, and the per-call limits hold. `warren.backends()` shows the
threads each backend holds and the cluster's count (DESIGN.md §5.7), which
the cap bounds."""

import os
import threading
import time
from dataclasses import dataclass

from .session import BackendLost, Session

DATABASE = "threads"
GRAPH = "g"
NODES = 20000
# A call measured: a few seconds of compute on the debug build, held as
# one grant from its first kernel to its last.
CALL = f"SELECT warren.betweenness_centrality('{GRAPH}', 'bc', k := 48, persist := false)"
# A call long enough to be waited for, cancelled or terminated: every source.
LONG_CALL = f"SELECT warren.betweenness_centrality('{GRAPH}', 'bc', persist := false)"
# The wait event of a backend waiting for threads: named on PG 17 and 18.
WAIT_EVENTS = ("WarrenComputeThreads", "Extension")
# A compute thread's name, as /proc keeps it (15 bytes).
THREAD_PREFIX = "warren-compute-"[:15]
# The cores the capped window may use past the cap: rayon's idle threads
# spin a moment before they sleep, and /proc counts in clock ticks.
SLACK = 0.25
NOT_ENFORCED = "warren.cluster_threads_limit is not enforced in this backend"

SETUP = f"""
CREATE TABLE nodes AS SELECT generate_series(1, {NODES})::bigint AS id;
SELECT warren.create_graph('{GRAPH}', 'SELECT id FROM nodes',
    'SELECT id, id % {NODES} + 1 FROM nodes UNION ALL SELECT id, (id * 7919) % {NODES} + 1 FROM nodes');
SELECT warren.refresh('{GRAPH}');
"""


def stat_ticks(text):
    """utime + stime, in clock ticks, from a /proc stat line (the command
    name, in parentheses, may hold spaces and parentheses)."""
    fields = text[text.rindex(")") + 2:].split()
    # Fields 14 and 15 of the line; the state, field 3, is fields[0].
    return int(fields[11]) + int(fields[12])


def compute_ticks(pid, proc="/proc"):
    """The CPU ticks of backend `pid`'s threads but its main one, exited
    threads included; 0 once the backend has gone."""
    try:
        with open(f"{proc}/{pid}/stat") as whole, open(f"{proc}/{pid}/task/{pid}/stat") as main:
            return stat_ticks(whole.read()) - stat_ticks(main.read())
    except (FileNotFoundError, ProcessLookupError):
        return 0


def compute_threads(pid, proc="/proc"):
    """The compute threads backend `pid` has now (its pool)."""
    count = 0
    try:
        tasks = os.listdir(f"{proc}/{pid}/task")
    except FileNotFoundError:
        return 0
    for task in tasks:
        try:
            with open(f"{proc}/{pid}/task/{task}/comm") as comm:
                count += comm.read().strip().startswith(THREAD_PREFIX)
        except FileNotFoundError:
            pass
    return count


@dataclass(frozen=True)
class Window:
    """What a measured window saw."""
    seconds: float
    cpu_seconds: float
    calls: int
    failures: tuple
    waits_seen: int
    # The most compute threads `warren.backends()` showed held: in the
    # cluster row, and in one of the sessions' rows.
    most_held: int = 0
    most_held_by_one: int = 0

    @property
    def cores(self):
        return self.cpu_seconds / self.seconds if self.seconds > 0 else 0.0


def within_cap(window, cap, slack=SLACK):
    """Whether the window's compute threads used at most `cap` cores (and
    `slack` more)."""
    return window.cores <= cap * (1 + slack)


def measure(cluster, sessions, threads, seconds, clock_ticks=None):
    """Runs `sessions` backends calling `CALL` over and over with
    `warren.threads = threads`, and measures, once each has finished a
    first call, `seconds` of their compute threads' CPU time, how often
    one was seen waiting for threads, and the most threads
    `warren.backends()` showed held."""
    ticks_per_second = clock_ticks or os.sysconf("SC_CLK_TCK")
    opened = [Session(cluster, DATABASE, settings=(f"warren.threads = {threads}",)) for _ in range(sessions)]
    warmed = threading.Barrier(sessions + 1)
    stop = threading.Event()
    calls, failures = [0] * sessions, []

    def work(index, session):
        first = session.run(CALL)
        if not first.ok:
            failures.append(f"{first.sqlstate}: {first.message}")
        warmed.wait()
        while not stop.is_set():
            done = session.run(CALL)
            calls[index] += 1
            if not done.ok:
                failures.append(f"{done.sqlstate}: {done.message}")

    workers = [threading.Thread(target=work, args=(i, s)) for i, s in enumerate(opened)]
    try:
        for worker in workers:
            worker.start()
        warmed.wait()
        pids = [session.pid for session in opened]
        waits, most_held, most_held_by_one = 0, 0, 0
        with Session(cluster, DATABASE) as watcher:
            before, started = sum(compute_ticks(pid) for pid in pids), time.monotonic()
            while time.monotonic() - started < seconds:
                waits += int(watcher.run(waiting_count(pids)).value())
                held, by_one = held_threads(watcher, pids)
                most_held, most_held_by_one = max(most_held, held), max(most_held_by_one, by_one)
                time.sleep(0.05)
            after, ended = sum(compute_ticks(pid) for pid in pids), time.monotonic()
        stop.set()
        for worker in workers:
            worker.join()
    finally:
        stop.set()
        for session in opened:
            session.close()
    return Window(ended - started, (after - before) / ticks_per_second, sum(calls), tuple(failures), waits,
                  most_held, most_held_by_one)


def held_threads(session, pids):
    """The compute threads `warren.backends()` shows held now, in its
    `cluster` row, and the most in one row of `pids`."""
    done = session.run(
        "SELECT coalesce(max(compute_threads) FILTER (WHERE kind = 'cluster'), 0), "
        f"coalesce(max(compute_threads) FILTER (WHERE pid IN ({', '.join(map(str, pids))})), 0) "
        "FROM warren.backends()")
    check(done, "warren.backends()")
    return int(done.rows[0][0]), int(done.rows[0][1])


def held_problems(window, cap):
    """What is wrong with the threads `warren.backends()` showed held over
    a capped window: never more than the cap, in the cluster or in one
    backend, and some."""
    problems = []
    if window.most_held > cap:
        problems.append(f"warren.backends() showed {window.most_held} threads held in the cluster, over the cap")
    if window.most_held_by_one > cap:
        problems.append(f"warren.backends() showed a backend holding {window.most_held_by_one} threads, "
                        "over the cap")
    if window.most_held == 0:
        problems.append("warren.backends() never showed a thread held")
    return problems


def waiting_count(pids):
    """SQL counting the backends of `pids` waiting for compute threads."""
    events = ", ".join(f"'{event}'" for event in WAIT_EVENTS)
    return (f"SELECT count(*) FROM pg_stat_activity WHERE pid IN ({', '.join(map(str, pids))}) "
            f"AND wait_event_type = 'Extension' AND wait_event IN ({events})")


def set_limit(cluster, name, value):
    """Sets server setting `name` to `value` (`ALTER SYSTEM`, reload), and
    returns once a new session sees it."""
    with Session(cluster, DATABASE) as session:
        # Unless preloaded, the library defines its settings once loaded.
        for sql in ("LOAD 'pg_warren'", f"ALTER SYSTEM SET {name} = '{value}'", "SELECT pg_reload_conf()"):
            check(session.run(sql), sql)
    database = cluster.database(DATABASE)
    for _ in range(200):
        if database.query(f"SELECT current_setting('{name}', true)")[0][0] == value:
            return
        time.sleep(0.05)
    raise RuntimeError(f"{name} did not become {value}")


def check(done, sql):
    """Raises unless `done`, the outcome of `sql`, succeeded."""
    if not done.ok:
        raise RuntimeError(f"{sql}: {done.sqlstate}: {done.message}")


def wait_for(predicate, seconds=30, interval=0.05):
    """Polls `predicate` until it holds; whether it did within `seconds`."""
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return False


class Background:
    """A statement running in its own session on a thread."""

    def __init__(self, cluster, sql, settings=()):
        self.session = Session(cluster, DATABASE, settings=settings)
        self.pid = self.session.pid
        self.outcome = None
        self.thread = threading.Thread(target=self._run, args=(sql,))
        self.thread.start()

    def _run(self, sql):
        try:
            self.outcome = self.session.run(sql)
        except BackendLost:
            self.outcome = None

    def finish(self, seconds=60):
        """The statement's outcome, once it ends (None if it does not
        within `seconds`)."""
        self.thread.join(seconds)
        return self.outcome

    def close(self):
        self.session.close()


def waiting(cluster, pid):
    """Whether backend `pid` is waiting for compute threads."""
    return int(cluster.database(DATABASE).query(waiting_count([pid]))[0][0]) == 1


def running(cluster, pid):
    """Whether backend `pid` runs a statement and waits for nothing."""
    rows = cluster.database(DATABASE).query(
        f"SELECT state, coalesce(wait_event, '') FROM pg_stat_activity WHERE pid = {pid}")
    return bool(rows) and rows[0][0] == "active" and rows[0][1] not in WAIT_EVENTS


def cancels_and_exits(cluster):
    """With a cap of one thread, held by a long call: a second call waits,
    showing the wait event; a cancel and `statement_timeout` end the wait
    with 57014; the holder, terminated mid-kernel, gives its thread back,
    so a call then runs. The failures, if any."""
    failures = []
    holder = Background(cluster, LONG_CALL, settings=("warren.threads = 1",))
    try:
        if not wait_for(lambda: running(cluster, holder.pid)):
            return ["the long call never started"]
        time.sleep(0.5)
        with Session(cluster, DATABASE) as watcher:
            held = held_threads(watcher, [holder.pid])
        if held != (1, 1):
            failures.append(f"the holder of the cap's thread: warren.backends() showed {held} held "
                            "(cluster, holder), not (1, 1)")
        waiter = Background(cluster, CALL)
        try:
            if not wait_for(lambda: waiting(cluster, waiter.pid)):
                failures.append("a call under a full cap was never seen waiting")
            cluster.database(DATABASE).query(f"SELECT pg_cancel_backend({waiter.pid})")
            done = waiter.finish(10)
            if done is None or done.sqlstate != "57014":
                failures.append(f"a cancelled wait ended with {done and done.sqlstate}, not 57014")
        finally:
            waiter.close()
        timed = Background(cluster, CALL, settings=("statement_timeout = '500ms'",))
        try:
            done = timed.finish(10)
            if done is None or done.sqlstate != "57014":
                failures.append(f"statement_timeout ended a wait with {done and done.sqlstate}, not 57014")
        finally:
            timed.close()
        cluster.database(DATABASE).query(f"SELECT pg_terminate_backend({holder.pid}, 10000)")
        holder.finish(10)
    finally:
        holder.close()
    if not wait_for(lambda: held_now(cluster) == 0, seconds=10):
        failures.append(f"after the holder was terminated, warren.backends() showed {held_now(cluster)} "
                        "threads held, not 0")
    after = Background(cluster, CALL, settings=("statement_timeout = '60s'", "warren.threads = 1"))
    try:
        done = after.finish(90)
        if done is None or not done.ok:
            failures.append("after the holder was terminated, a call could not get a thread: "
                            f"{done and done.sqlstate} {done and done.message}")
    finally:
        after.close()
    return failures


def held_now(cluster):
    """The compute threads the `cluster` row of `warren.backends()` shows
    held now."""
    return int(cluster.database(DATABASE).query(
        "SELECT compute_threads FROM warren.backends() WHERE kind = 'cluster'")[0][0])


def per_call_limits(cluster):
    """`warren.threads_limit` bounds a call's pool, and
    `warren.compute_memory_limit` its memory, whatever the session's own
    settings. The failures, if any."""
    failures = []
    set_limit(cluster, "warren.threads_limit", "1")
    with Session(cluster, DATABASE, settings=("warren.threads = 4",)) as session:
        session.run(CALL).value()
        threads = compute_threads(session.pid)
        if threads != 1:
            failures.append(f"warren.threads_limit = 1, but the pool has {threads} threads")
    set_limit(cluster, "warren.threads_limit", "-1")
    set_limit(cluster, "warren.compute_memory_limit", "64kB")
    with Session(cluster, DATABASE, settings=("warren.max_compute_memory = '8GB'",)) as session:
        done = session.run(f"SELECT warren.vector_const('{GRAPH}', 'x', 1)")
        if done.sqlstate != "53400":
            failures.append(f"warren.compute_memory_limit = 64kB, but a call ended with {done.sqlstate}")
    set_limit(cluster, "warren.compute_memory_limit", "-1")
    return failures
