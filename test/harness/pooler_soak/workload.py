"""The pooler soak's clients (DESIGN.md §13 M11): many client connections
through one pooler, each a psql session (harness.session) that runs a
few dozen transactions and disconnects, then connects again, so the
pool's few server backends serve many clients in turn.

A reader's transactions are a mix: traversals of both graphs (`traverse`),
a traversal and its `last_stats()` in one transaction (`stats`), compute
that persists nothing (`transient`: transient vectors written, read and
dropped in one transaction, on a version named by id; in session mode a
careless client sometimes leaves them for the pooler's `DISCARD ALL`),
a persisted vector written, read and dropped (`persist`), a prepared
statement (`prepared`: SQL `PREPARE` in session mode, the extended
protocol's unnamed statement in transaction mode, which works there),
introspection (`introspect`) and a call cut short by a `SET LOCAL
statement_timeout` (`timeout`). A writer edits the source and refreshes,
stages versions and promotes or drops them, refreshes the text graph and
now and then compacts (`write`), so versions are retired all the time.

Chaos (`Chaos`), once per period per pool: `pg_cancel_backend` of a
pooled backend running a pg_warren call, or a client that disconnects in
a transaction, idle or in a call (the pooler then closes that server
connection). Every failure must be `allowed` for its operation."""

import random
import re
import threading
import time
from collections import Counter
from dataclasses import dataclass, field

from ..concurrent_refresh import LOCK_TIMED_OUT, MOVED, RETIRED, SUPERSEDED, WRITTEN_MEANWHILE, joined
from ..session import BackendLost, Session
from .fixture import DATABASE, edit_sql

CANCELED = ("57014", r"^canceling statement due to (user request|statement timeout)$")
ALLOWED = {
    "traverse": [CANCELED],
    "stats": [CANCELED],
    "transient": [CANCELED, RETIRED, MOVED],
    "persist": [CANCELED, RETIRED, MOVED, WRITTEN_MEANWHILE],
    "prepared": [CANCELED],
    "introspect": [CANCELED],
    "timeout": [CANCELED],
    "write": [CANCELED, LOCK_TIMED_OUT, SUPERSEDED, WRITTEN_MEANWHILE],
}
READS = ("reachable_set", "bfs", "neighbours", "dijkstra", "walk", "degree")
JOIN_SECONDS = 180
CARELESS = 0.3
PERSISTED_NAMES_PER_CLIENT = 2  # p<n> and to<n>; a client reuses them
STAGED_DROP_ATTEMPTS = 10


def allowed(operation, sqlstate, message):
    """Whether `operation` may fail with `sqlstate` and `message`."""
    return any(state == sqlstate and re.search(pattern, message) for state, pattern in ALLOWED[operation])


def node_sql(graph, node):
    return f"'n{node}'::text" if graph == "t" else f"{node}::bigint"


def traversal_sql(graph, kind, node, other):
    """A traversal `kind` of `graph` from `node`, answering one row."""
    a, b = node_sql(graph, node), node_sql(graph, other)
    return {
        "reachable_set": f"SELECT count(*) FROM warren.reachable_set('{graph}', ARRAY[{a}], 2)",
        "bfs": f"SELECT count(*) FROM warren.bfs('{graph}', ARRAY[{a}], 2)",
        "neighbours": f"SELECT count(*) FROM warren.neighbours('{graph}', {a})",
        "dijkstra": f"SELECT count(*) FROM warren.dijkstra('{graph}', {a}, {b})",
        "walk": (f"SELECT count FROM warren.walk('{graph}', ARRAY[{a}], "
                 f"ARRAY[warren.s(NULL, 'out', NULL::{'text' if graph == 't' else 'bigint'}[], 1, 2)], "
                 "mode := 'count')"),
        "degree": f"SELECT warren.degree('{graph}', {a})",
    }[kind]


@dataclass
class Tally:
    """What every pool's clients did and saw, shared by their threads."""
    outcomes: Counter = field(default_factory=Counter)  # (mode, operation, SQLSTATE) → count
    unexpected: list = field(default_factory=list)  # (mode, operation, SQLSTATE, message)
    mismatches: list = field(default_factory=list)
    chaos: Counter = field(default_factory=Counter)  # (mode, event) → count
    lock: threading.Lock = field(default_factory=threading.Lock)

    def count(self, mode, operation, done):
        with self.lock:
            self.outcomes[(mode, operation, done.sqlstate)] += 1
            if not done.ok and not allowed(operation, done.sqlstate, done.message):
                self.unexpected.append((mode, operation, done.sqlstate, done.message))

    def mismatch(self, text):
        with self.lock:
            self.mismatches.append(text)

    def event(self, mode, event):
        with self.lock:
            self.chaos[(mode, event)] += 1


class Abandon:
    """A request, made by chaos, that one client of the pool disconnect in a transaction."""

    def __init__(self):
        self.pending, self.lock = 0, threading.Lock()

    def request(self):
        with self.lock:
            self.pending += 1

    def claim(self):
        with self.lock:
            if self.pending:
                self.pending -= 1
                return True
            return False


class Workload:
    """`clients` clients of pool `mode` through `pooler` as `role`, the first
    `writers` of them writers, on a source of `nodes` nodes."""

    def __init__(self, pooler, mode, role, clients, writers, nodes, seed, tally, stop, think_seconds,
                 write_seconds):
        self.pooler, self.mode, self.role = pooler, mode, role
        self.clients, self.writers, self.nodes, self.seed = clients, writers, nodes, seed
        self.tally, self.stop = tally, stop
        self.think_seconds, self.write_seconds = think_seconds, write_seconds
        self.abandon = Abandon()
        self.threads = []
        self.left = {number: set() for number in range(clients)}  # (name, version) a client's persist left

    def start(self):
        self.threads = [threading.Thread(target=self._client, args=(n,), name=f"{self.mode}-{n}", daemon=True)
                        for n in range(self.clients)]
        for thread in self.threads:
            thread.start()

    def join(self):
        for stuck in joined(self.threads, JOIN_SECONDS):
            self.tally.mismatch(f"client {stuck} did not stop within {JOIN_SECONDS} s of the end of the run")

    def _client(self, number):
        rng = random.Random(f"{self.seed}-{self.mode}-{number}")
        while not self.stop.is_set():
            try:
                self._connection(rng, number)
            except BackendLost as lost:
                self.tally.mismatch(f"{self.mode} client {number} lost its connection: {lost}")
            except Exception as error:  # noqa: BLE001: any failure of a client fails the run, and says why
                self.tally.mismatch(f"{self.mode} client {number} stopped: {error!r}")
                return

    def _connection(self, rng, number):
        """One client connection: a few dozen transactions, or fewer if chaos asks it to abandon one."""
        s = Session(self.pooler, DATABASE, user=self.role)
        prepared = False
        try:
            for _ in range(rng.randint(10, 60)):
                if self.stop.is_set():
                    break
                if self.abandon.claim():
                    self._abandon(s, rng, number)
                    return
                if number < self.writers:
                    self._write(s, rng)
                    self.stop.wait(self.write_seconds * rng.uniform(0.5, 1.5))
                    continue
                choice = rng.choices(["traverse", "stats", "transient", "persist", "prepared", "introspect",
                                      "timeout"], weights=[35, 10, 15, 10, 10, 12, 8])[0]
                if choice == "prepared":
                    prepared = self._prepared(s, rng, prepared)
                else:
                    getattr(self, f"_{choice}")(s, rng, number)
                self.stop.wait(rng.random() * self.think_seconds)
        finally:
            s.close()

    def _record(self, operation, done):
        self.tally.count(self.mode, operation, done)
        return done.ok

    def _transaction(self, s, operation, statements, end="COMMIT"):
        """Runs `statements` in one transaction, ended by `end`; their
        outcomes, or None if one failed (recorded; the transaction rolled
        back)."""
        s.run("BEGIN")
        outcomes = []
        for sql in statements:
            done = s.run(sql)
            if not done.ok:
                self._record(operation, done)
                s.run("ROLLBACK")
                return None
            outcomes.append(done)
        done = s.run(end)
        self._record(operation, done)
        return outcomes if done.ok else None

    def _current(self, s, operation, graph="g"):
        done = s.run(f"SELECT warren.info('{graph}') ->> 'current_version'")
        return done.value() if self._record(operation, done) else None

    # Readers.

    def _pick(self, rng):
        return rng.choice(("g", "t")), rng.choice(READS), rng.randint(1, self.nodes), rng.randint(1, self.nodes)

    def _traverse(self, s, rng, _number):
        self._record("traverse", s.run(traversal_sql(*self._pick(rng))))

    def _stats(self, s, rng, _number):
        """A traversal and its `last_stats()`, in one transaction: on one
        backend in either pool mode, so the stats are that call's."""
        graph, kind, node, other = self._pick(rng)
        kind = rng.choice(("reachable_set", "bfs", "dijkstra"))
        outcomes = self._transaction(s, "stats", [traversal_sql(graph, kind, node, other),
                                                  "SELECT warren.last_stats() ->> 'function'"])
        if outcomes and outcomes[1].value() != kind:
            self.tally.mismatch(f"{self.mode}: last_stats() after {kind} in its transaction names "
                                f"{outcomes[1].value()!r}")

    def _transient(self, s, rng, number):
        """Transient vectors on a version named by id, in one transaction;
        dropped, unless a careless client in session mode leaves them for
        the pooler's `DISCARD ALL`."""
        version = self._current(s, "transient")
        if version is None:
            return
        x, y = f"x{number}", f"y{number}"
        at = f"persist := false, version := {version}"
        statements = [f"SELECT warren.vector_const('g', '{x}', 1, {at})",
                      f"SELECT warren.spmv('g', '{x}', '{y}', weighted := true, {at})",
                      f"SELECT warren.vector_reduce('g', '{y}', 'sum', {version})"]
        if not (self.mode == "session" and rng.random() < CARELESS):
            statements += [f"SELECT warren.drop_vector('g', '{name}', {version})" for name in (x, y)]
        self._transaction(s, "transient", statements)

    def _persist(self, s, rng, number):
        """A persisted vector written, read and dropped, a statement each.
        One that a cancel or a lost connection kept from its drop is
        dropped by the client's next persist, as an application would
        clean up: a persisted vector keeps its version from retention
        (DESIGN.md §7), so one left for good would grow the catalog."""
        left = self.left[number]
        for name, version in sorted(left):
            if self._record("persist", s.run(self._drop(name, version))):
                left.discard((name, version))
        version = self._current(s, "persist")
        if version is None:
            return
        name = f"p{number}"
        left.add((name, version))
        call = rng.choice([f"SELECT warren.pagerank('g', '{name}', max_iter := 10, version := {version})",
                           f"SELECT warren.vector_degree('g', '{name}', version := {version})"])
        for sql in (call, f"SELECT warren.vector_reduce('g', '{name}', 'sum', {version})",
                    self._drop(name, version)):
            if not self._record("persist", s.run(sql)):
                return
        left.discard((name, version))

    @staticmethod
    def _drop(name, version):
        return f"SELECT warren.drop_vector('g', '{name}', {version}, if_exists := true)"

    def _prepared(self, s, rng, prepared):
        """A prepared statement: SQL `PREPARE` once per session in session
        mode; in transaction mode, the unnamed statement of the extended
        protocol, which pooling keeps. Whether the session has prepared."""
        node = rng.randint(1, self.nodes)
        if self.mode == "transaction":
            self._record("prepared", s.run(f"SELECT warren.degree('g', $1::bigint) \\bind {node} \\g"))
            return prepared
        if not prepared:
            prepared = self._record("prepared", s.run("PREPARE soak_degree(bigint) AS SELECT warren.degree('g', $1)"))
        if prepared:
            self._record("prepared", s.run(f"EXECUTE soak_degree({node})"))
        return prepared

    def _introspect(self, s, _rng, _number):
        done = s.run("SELECT (SELECT count(*) FROM warren.cache()), (SELECT count(*) FROM warren.backends()), "
                     "(SELECT count(*) FROM warren.progress()), "
                     "(SELECT count(*) FILTER (WHERE pins < 0 OR mappers < 0 OR bytes < 0) FROM warren.shared_cache())")
        if self._record("introspect", done) and done.rows[0][3] != "0":
            self.tally.mismatch(f"{self.mode}: shared_cache() has a negative count: {done.rows}")

    def _timeout(self, s, rng, number):
        """A long call cut short by a statement timeout (or finished in time), rolled back."""
        self._transaction(s, "timeout", [f"SET LOCAL statement_timeout = '{rng.randint(5, 60)}ms'",
                                         f"SELECT warren.pagerank('g', 'to{number}', max_iter := 300, tol := 0)"],
                          end="ROLLBACK")

    # Writers.

    def _write(self, s, rng):
        choice = rng.choices(["edit+refresh", "refresh t", "stage", "compact"], weights=[50, 15, 30, 5])[0]
        lock = "SET LOCAL lock_timeout = '2s'"
        if choice == "edit+refresh":
            self._transaction(s, "write", [lock, edit_sql(rng, self.nodes), "SELECT warren.refresh('g')"])
        elif choice == "refresh t":
            self._transaction(s, "write", [lock, "SELECT warren.refresh('t')"])
        elif choice == "compact":
            self._transaction(s, "write", [lock, "SELECT warren.compact('g')"])
        else:
            self._stage(s, rng, lock)

    def _stage(self, s, rng, lock):
        """A staged refresh, then its promotion or its drop."""
        outcomes = self._transaction(s, "write", [lock, "SELECT warren.refresh('g', promote := false)"])
        if outcomes is None:
            return
        version = outcomes[1].value()
        if rng.random() < 0.5:
            self._transaction(s, "write", [lock, f"SELECT warren.promote('g', {version})"])
        for _ in range(STAGED_DROP_ATTEMPTS):  # a cancel or a lock timeout is retried: the staged version must go
            current = self._current(s, "write")
            if current == version or (current is not None and self._transaction(
                    s, "write", [lock, f"SELECT warren.drop_version('g', {version})"]) is not None):
                return

    # Chaos.

    def _abandon(self, s, rng, number):
        """Disconnects in a transaction: idle after a transient write, or in
        a long call. The pooler closes the server connection."""
        s.run("BEGIN")
        s.run(f"SELECT warren.vector_const('g', 'a{number}', 1, persist := false)")
        if rng.random() < 0.5:
            self.tally.event(self.mode, "abandoned idle in a transaction")
        else:
            s.process.stdin.write("SELECT warren.pagerank('g', 'abandoned', max_iter := 1000, tol := 0);\n")
            s.process.stdin.flush()
            time.sleep(rng.uniform(0.02, 0.2))
            self.tally.event(self.mode, "abandoned in a call")
        s.process.kill()
        s.process.wait()


class Chaos:
    """Every `period` seconds or so, for each workload in turn: a cancel of
    one of its pool's backends running a pg_warren call, through the
    superuser's direct session `admin`, or a request that a client
    abandon its connection in a transaction."""

    def __init__(self, admin, workloads, period, seed, stop):
        self.admin, self.workloads, self.period, self.stop = admin, workloads, period, stop
        self.rng = random.Random(f"{seed}-chaos")
        self.thread = threading.Thread(target=self._run, name="chaos", daemon=True)

    def _run(self):
        turn = 0
        while not self.stop.wait(self.period * self.rng.uniform(0.5, 1.5)):
            workload = self.workloads[turn % len(self.workloads)]
            if (turn // len(self.workloads)) % 2 == 0:
                done = self.admin.run(
                    "SELECT count(*) FILTER (WHERE pg_cancel_backend(pid)) FROM (SELECT pid FROM pg_stat_activity "
                    f"WHERE usename = '{workload.role}' AND state = 'active' AND query LIKE '%warren.%' "
                    "ORDER BY random() LIMIT 1) a")
                workload.tally.event(workload.mode, "cancel sent" if done.ok and done.value() == "1"
                                     else "cancel found no call")
            else:
                workload.abandon.request()
            turn += 1

    def start(self):
        self.thread.start()

    def join(self):
        self.thread.join(JOIN_SECONDS)

