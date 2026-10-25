"""Many backends refreshing and querying one graph at once (DESIGN.md §13,
M11 "Robustness"; §5.4, §5.6, §7).

For a bounded time, *readers* traverse and compute on the graph's current
version or on a version they name, *refreshers* edit its source tables
(one generation at a time, `harness.generations`) and refresh, promote,
stage, drop and compact versions, and a *churner* writes and drops
persisted vectors. Every read also reads the generation marker of the
version it read (in the same statement, so of the same version) and is
checked against the oracle of that generation: a read never sees a
half-built version. Every failure must have a SQLSTATE `ALLOWED` for its
operation. Afterwards (`settled`): no backend is left in the activity
table, no shared version is pinned or mapped, the graph's current version
is the one its last committed promotion made, no staged version, vector
or partial segment is left, every remaining version matches its
generation, retention keeps `retain_versions` once a refresh runs, and
the shared cache, evicted, holds no more than at the start."""

import math
import random
import re
import threading
import time
from collections import Counter
from dataclasses import dataclass, field

from .generations import EDGE_QUERY, MARK, NODE_QUERY, Generations, edit_sql, oracle, setup_sql
from .session import Session

GRAPH = "g"
PROMOTIONS = "public.promotions"
RETAIN = 2
LOCK_TIMEOUT = "200ms"
STAGED_DROP_ATTEMPTS = 50

# The failures each operation may have, as SQLSTATE and message, and why
# (DESIGN.md §5.2, §6.2, §7): a version named by id was retired or dropped
# after its id was read; a compaction moved a vector to the version it made
# while the vector's writer still used it (so the writer finds it gone, or
# finds a vector of its name it did not write); the refresher with a
# lock_timeout gave up waiting for the graph's (or the state table's) row;
# a staged version's id space was replaced by a compaction before it could
# be promoted.
RETIRED = ("42704", r'^graph "g" has no version \d+$|^version \d+ no longer exists: vector "\w+" was not written')
MOVED = ("42704", r'^graph "g" has no vector "\w+" at version \d+$')
WRITTEN_MEANWHILE = ("40001", r'^vector "\w+" at version \d+ was written by a concurrent transaction; retry$')
LOCK_TIMED_OUT = ("55P03", r"^canceling statement due to lock timeout$")
SUPERSEDED = ("55000", r'^version \d+ of graph "g" is in an id space that compacted version \d+ replaced$')
ALLOWED = {
    "read": [RETIRED],
    "compute": [RETIRED, MOVED, WRITTEN_MEANWHILE],
    "introspect": [],
    "edit": [LOCK_TIMED_OUT],
    "refresh": [LOCK_TIMED_OUT],
    "edit+refresh": [LOCK_TIMED_OUT],
    "rollback": [LOCK_TIMED_OUT],
    "stage": [LOCK_TIMED_OUT],
    "promote": [LOCK_TIMED_OUT, SUPERSEDED],
    "drop_version": [LOCK_TIMED_OUT],
    "compact": [LOCK_TIMED_OUT, WRITTEN_MEANWHILE],
    "churn": [RETIRED, MOVED, WRITTEN_MEANWHILE],
    "cleanup": [],
}


def allowed(operation, sqlstate, message):
    """Whether `operation` may fail with `sqlstate` and `message` (`ALLOWED`)."""
    return any(state == sqlstate and re.search(pattern, message) for state, pattern in ALLOWED[operation])


def marker_sql(version):
    return f"(SELECT max(n) FROM warren.neighbours('{GRAPH}', 0::bigint, version := {version}) n)"


def read_sql(kind, node, other, version):
    """One statement that reads the generation marker and `kind`'s answer,
    as text, from one version: `version` (an id, or NULL for the current
    one)."""
    at = f"version := {version}"
    answer = {
        "reachable_set": f"SELECT count(*) FROM warren.reachable_set('{GRAPH}', ARRAY[{node}::bigint], 3, {at})",
        "bfs": f"SELECT count(*) || '/' || max(depth) FROM warren.bfs('{GRAPH}', ARRAY[{node}::bigint], 2, {at})",
        "neighbours": (f"SELECT coalesce(string_agg(n::text, ',' ORDER BY n), '') "
                       f"FROM warren.neighbours('{GRAPH}', {node}::bigint, {at}) n"),
        "dijkstra": (f"SELECT coalesce(min(cost)::text, 'none') "
                     f"FROM warren.dijkstra('{GRAPH}', {node}::bigint, {other}::bigint, {at})"),
        "walk": (f"SELECT count FROM warren.walk('{GRAPH}', ARRAY[{node}::bigint], "
                 f"ARRAY[warren.s(NULL, 'out', NULL::bigint[], 2, 2)], mode := 'count', {at})"),
        "degree": f"SELECT warren.degree('{GRAPH}', {node}::bigint, {at})::text",
    }[kind]
    return f"SELECT {marker_sql(version)}, ({answer})"


def expected(kind, graph, node, other):
    """`kind`'s answer, as `read_sql` returns it, on the oracle's `graph`."""
    if kind == "reachable_set":
        return str(len(graph.reachable(node, 3)))
    if kind == "bfs":
        depths = graph.depths(node, 2)
        return f"{len(depths)}/{max(depths.values())}"
    if kind == "neighbours":
        return ",".join(map(str, graph.out(node)))
    if kind == "dijkstra":
        cost = graph.cost(node, other)
        return "none" if cost is None else repr_cost(cost)
    if kind == "walk":
        return str(len(graph.ends(node, 2)))
    if kind == "degree":
        return str(len(graph.out(node)))
    raise KeyError(kind)


def repr_cost(cost):
    """A float8 as Postgres prints it (shortest round trip; integers without a point)."""
    return str(int(cost)) if cost == int(cost) else repr(cost)


READS = ("reachable_set", "bfs", "neighbours", "dijkstra", "walk", "degree")


def generation(marker):
    """The generation a version's marker names."""
    return int(marker) - MARK


@dataclass
class Tally:
    """What a run did and saw, shared by its threads."""
    outcomes: Counter = field(default_factory=Counter)
    unexpected: list = field(default_factory=list)  # (operation, SQLSTATE, message) not allowed
    mismatches: list = field(default_factory=list)
    generations: dict = field(default_factory=dict)  # version id → generation, as read
    promotions: dict = field(default_factory=dict)  # commit order (PROMOTIONS) → the version made current
    shared_versions: int = 0  # the most versions warren.shared_cache() listed at once
    lock: threading.Lock = field(default_factory=threading.Lock)

    def count(self, operation, done):
        """Counts `operation`'s outcome `done`; a failure not `allowed` is kept."""
        with self.lock:
            self.outcomes[(operation, done.sqlstate)] += 1
            if not done.ok and not allowed(operation, done.sqlstate, done.message):
                self.unexpected.append((operation, done.sqlstate, done.message))

    def mismatch(self, text):
        with self.lock:
            self.mismatches.append(text)

    def saw(self, version, gen):
        """Records that `version` holds generation `gen`; a second, different one is a mismatch."""
        with self.lock:
            known = self.generations.setdefault(int(version), gen)
        if known != gen:
            self.mismatch(f"version {version} read as generation {known} and {gen}")

    def promoted(self, order, version):
        with self.lock:
            self.promotions[int(order)] = int(version)

    def succeeded(self, operation):
        return self.outcomes[(operation, "00000")]


class Run:
    """One run on database `database` of `cluster`, whose graph has `nodes` nodes."""

    def __init__(self, cluster, database, nodes, seed=1):
        self.cluster, self.database = cluster, database
        self.generations = Generations(nodes)
        self.nodes = nodes
        self.seed = seed
        self.tally = Tally()
        self.stop = threading.Event()

    def session(self, *settings):
        return Session(self.cluster, self.database, settings)

    def prepare(self):
        """Creates the source tables at generation 0, the graph and its first version."""
        with self.session() as s:
            for statement in (setup_sql(self.generations.source(0)), f"CREATE SEQUENCE {PROMOTIONS}",
                              f"SELECT warren.create_graph('{GRAPH}', $q${NODE_QUERY}$q$, $q${EDGE_QUERY}$q$, "
                              f"weighted := true, retain_versions := {RETAIN})",
                              f"SELECT warren.refresh('{GRAPH}')"):
                done = s.run(statement)
                if not done.ok:
                    raise RuntimeError(f"{statement[:80]}: {done.message}")

    def go(self, seconds, readers, refreshers):
        """Runs the workers for `seconds`, then drops, through the API, any vector they left (a compaction may have moved one)."""
        threads = ([self._thread(self._read, i) for i in range(readers)]
                   + [self._thread(self._refresh, i) for i in range(refreshers)]
                   + [self._thread(self._churn, 0)])
        for thread in threads:
            thread.start()
        self.stop.wait(seconds)
        self.stop.set()
        for thread in threads:
            thread.join()
        with self.session() as s:
            for version, name in s.run("SELECT version_id, name FROM warren.vectors").rows:
                self._record("cleanup", s.run(f"SELECT warren.drop_vector('{GRAPH}', '{name}', {version})"))

    def _thread(self, work, number):
        def body():
            rng = random.Random(f"{self.seed}-{work.__name__}-{number}")
            try:
                settings = (f"lock_timeout = '{LOCK_TIMEOUT}'",) if work == self._refresh and number == 0 else ()
                with self.session(*settings) as s:
                    while not self.stop.is_set():
                        work(s, rng, number)
            except Exception as error:  # noqa: BLE001: any failure of a worker fails the run, and says why
                self.tally.mismatch(f"{work.__name__} {number} stopped: {error}")
                self.stop.set()
        return threading.Thread(target=body, name=f"{work.__name__}-{number}")

    def _record(self, operation, done):
        self.tally.count(operation, done)
        return done.ok

    def _current(self, s):
        return s.run(f"SELECT warren.info('{GRAPH}') ->> 'current_version'").value()

    # Readers.

    def _read(self, s, rng, number):
        choice = rng.random()
        if choice < 0.7:
            self._traverse(s, rng)
        elif choice < 0.9:
            self._compute(s, rng, number)
        else:
            self._introspect(s)

    def _traverse(self, s, rng):
        kind = rng.choice(READS)
        node, other = rng.randint(1, self.nodes // 2), rng.randint(1, self.nodes // 2)
        version = "NULL" if rng.random() < 0.5 else self._current(s)
        done = s.run(read_sql(kind, node, other, version))
        if not self._record("read", done):
            return
        marker, answer = done.rows[0]
        gen = generation(marker)
        if version != "NULL":
            self.tally.saw(version, gen)
        want = expected(kind, oracle(self.generations.source(gen), gen), node, other)
        if answer != want:
            self.tally.mismatch(f"{kind}({node}, {other}) at version {version}, generation {gen}: "
                                f"{answer}, expected {want}")

    def _drop_everywhere(self, s, operation, name):
        """Drops vector `name` at every version: a compaction may have moved it
        from the version it was written to (DESIGN.md §5.2)."""
        self._record(operation, s.run(f"SELECT count(warren.drop_vector('{GRAPH}', name, version_id, if_exists := true)) "
                                      f"FROM warren.vectors WHERE name = '{name}'"))

    def _compute(self, s, rng, number):
        """spmv (row sums of the weights) or pagerank, persisted on a
        version named by id, read back and dropped."""
        version = self._current(s)
        name = f"c{number}"
        self._drop_everywhere(s, "compute", name)
        if rng.random() < 0.5:
            steps = [f"SELECT warren.vector_const('{GRAPH}', 'one{number}', 1, persist := false, version := {version})",
                     f"SELECT warren.spmv('{GRAPH}', 'one{number}', '{name}', weighted := true, version := {version})"]
            check = self._weight_sum
        else:
            steps = [f"SELECT warren.pagerank('{GRAPH}', '{name}', max_iter := 20, version := {version})"]
            check = self._probabilities
        for step in steps:
            if not self._record("compute", s.run(step)):
                return
        done = s.run(f"SELECT {marker_sql(version)}, warren.vector_reduce('{GRAPH}', '{name}', 'sum', {version})")
        if self._record("compute", done):
            marker, total = done.rows[0]
            gen = generation(marker)
            self.tally.saw(version, gen)
            check(version, gen, float(total))
        self._record("compute", s.run(f"SELECT warren.drop_vector('{GRAPH}', '{name}', {version})"))

    def _weight_sum(self, version, gen, total):
        want = oracle(self.generations.source(gen), gen).weight_sum
        if not math.isclose(total, want, rel_tol=1e-9):
            self.tally.mismatch(f"spmv's row sums at version {version} (generation {gen}) total {total}, "
                                f"expected {want}")

    def _probabilities(self, version, gen, total):
        if not math.isclose(total, 1.0, rel_tol=1e-6):
            self.tally.mismatch(f"pagerank at version {version} (generation {gen}) sums to {total}")

    def _introspect(self, s):
        done = s.run("SELECT (SELECT count(*) FROM warren.cache()), "
                     "(SELECT count(*) FILTER (WHERE pins < 0 OR mappers < 0 OR bytes < 0) FROM warren.shared_cache()), "
                     "(SELECT count(*) FILTER (WHERE kind = 'version') FROM warren.shared_cache()), "
                     "(SELECT count(*) FROM warren.backends()), (SELECT count(*) FROM warren.progress())")
        if not self._record("introspect", done):
            return
        if done.rows[0][1] != "0":
            self.tally.mismatch(f"shared_cache() has a negative count: {done.rows}")
        with self.tally.lock:
            self.tally.shared_versions = max(self.tally.shared_versions, int(done.rows[0][2]))

    # Refreshers.

    def _refresh(self, s, rng, _number):
        operation = rng.choices(["edit", "refresh", "edit+refresh", "rollback", "stage", "compact"],
                                weights=[30, 25, 20, 10, 12, 3])[0]
        {"edit": self._edit, "refresh": self._plain_refresh, "edit+refresh": self._edit_and_refresh,
         "rollback": self._rolled_back, "stage": self._stage, "compact": self._compact}[operation](s, rng)

    def _apply_edit(self, s, operation):
        """In the open transaction, moves the source tables on one generation; that generation, or None."""
        done = s.run("SELECT gen FROM public.state FOR UPDATE")
        if not done.ok:
            self._record(operation, done)
            s.run("ROLLBACK")
            return None
        gen = int(done.value()) + 1
        done = s.run(edit_sql(self.generations.edit(gen), gen))
        if not done.ok:
            raise RuntimeError(f"edit to generation {gen}: {done.message}")
        return gen

    def _commit(self, s, operation):
        done = s.run("COMMIT")
        self._record(operation, done)
        return done.ok

    def _edit(self, s, _rng):
        s.run("BEGIN")
        if self._apply_edit(s, "edit") is not None:
            self._commit(s, "edit")

    def _promoting(self, s, operation, call, version=None, gen=None):
        """In the open transaction, runs `call`, which makes `version` (or the
        version it returns) current (of generation `gen`, if known), and commits, recording the version
        and its place in the order promotions commit: `call` holds the
        graph's row until the commit, so the order it then takes from
        `PROMOTIONS` is the commit order."""
        done = s.run(call)
        if not done.ok:
            self._record(operation, done)
            s.run("ROLLBACK")
            return
        version, order = version or done.value(), s.run(f"SELECT nextval('{PROMOTIONS}')").value()
        if self._commit(s, operation):
            self.tally.promoted(order, version)
            if gen is not None:
                self.tally.saw(version, gen)

    def _plain_refresh(self, s, _rng):
        s.run("BEGIN")
        self._promoting(s, "refresh", f"SELECT warren.refresh('{GRAPH}')")

    def _edit_and_refresh(self, s, _rng):
        s.run("BEGIN")
        gen = self._apply_edit(s, "edit+refresh")
        if gen is not None:
            self._promoting(s, "edit+refresh", f"SELECT warren.refresh('{GRAPH}')", gen=gen)

    def _rolled_back(self, s, rng):
        """An edit and a refresh, rolled back: nothing of either is left."""
        s.run("BEGIN")
        if self._apply_edit(s, "rollback") is None:
            return
        done = s.run(f"SELECT warren.refresh('{GRAPH}', promote := {rng.random() < 0.5})")
        s.run("ROLLBACK")
        if not self._record("rollback", done):
            return
        version = done.value()
        left = s.run(f"SELECT count(*) FROM warren.versions WHERE version_id = {version}").value()
        if left != "0":
            self.tally.mismatch(f"version {version}, rolled back, is still recorded")

    def _stage(self, s, rng):
        """A staged refresh, then its promotion or its drop."""
        done = s.run(f"SELECT warren.refresh('{GRAPH}', promote := false)")
        if not self._record("stage", done):
            return
        version = done.value()
        if rng.random() < 0.5:
            s.run("BEGIN")
            self._promoting(s, "promote", f"SELECT warren.promote('{GRAPH}', {version})", version)
            if s.run(f"SELECT staged FROM warren.versions WHERE version_id = {version}").rows != [["t"]]:
                return
        for _ in range(STAGED_DROP_ATTEMPTS):  # a lock timeout is retried: the staged version must go
            done = s.run(f"SELECT warren.drop_version('{GRAPH}', {version})")
            if self._record("drop_version", done) or done.sqlstate != LOCK_TIMED_OUT[0]:
                return

    def _compact(self, s, _rng):
        s.run("BEGIN")
        self._promoting(s, "compact", f"SELECT warren.compact('{GRAPH}')")

    # The vector churner.

    def _churn(self, s, rng, _number):
        """Writes a persisted vector on the current version, checks it, and drops it."""
        version = self._current(s)
        value = rng.randint(1, 9)
        self._drop_everywhere(s, "churn", "churn")
        if not self._record("churn", s.run(f"SELECT warren.vector_const('{GRAPH}', 'churn', {value}, "
                                           f"version := {version})")):
            return
        done = s.run(f"SELECT {marker_sql(version)}, warren.vector_reduce('{GRAPH}', 'churn', 'sum', {version})")
        if self._record("churn", done):
            gen = generation(done.rows[0][0])
            self.tally.saw(version, gen)
            want = value * oracle(self.generations.source(gen), gen).node_count
            if float(done.rows[0][1]) != want:
                self.tally.mismatch(f"vector_const {value} at version {version} sums to {done.rows[0][1]}, "
                                    f"expected {want}")
        time.sleep(rng.random() * 0.05)
        self._record("churn", s.run(f"SELECT warren.drop_vector('{GRAPH}', 'churn', {version})"))


UNNAMED_SHARED_BYTES = ("SELECT coalesce(sum(CASE WHEN kind = 'cluster' THEN bytes ELSE -bytes END), 0)::bigint "
                        "FROM warren.shared_cache()")
SETTLE_SECONDS = 10


def until(s, sql, seconds=SETTLE_SECONDS):
    """Polls `sql`, a count, until it is 0 or `seconds` pass; its last value."""
    deadline = time.monotonic() + seconds
    while (value := s.run(sql).value()) != "0" and time.monotonic() < deadline:
        time.sleep(0.1)
    return value


def settled(run, unnamed_before):
    """What is wrong once every worker has disconnected (see the module's
    doc comment); `unnamed_before` is `UNNAMED_SHARED_BYTES` at the start."""
    tally, failures = run.tally, []
    with run.session() as s:
        if until(s, "SELECT count(*) FROM warren.backends() WHERE pid <> pg_backend_pid()") != "0":
            failures.append(f"backends left in warren.backends(): {s.run('SELECT * FROM warren.backends()').rows}")
        if until(s, "SELECT count(*) FROM warren.shared_cache() WHERE pins <> 0 OR mappers <> 0") != "0":
            failures.append(f"shared entries still pinned or mapped: {s.run('SELECT * FROM warren.shared_cache()').rows}")
        own = s.run("SELECT private_bytes + transient_bytes FROM warren.backends() WHERE pid = pg_backend_pid()")
        if own.rows != [["0"]]:
            failures.append(f"a fresh backend's private memory is {own.rows}, not 0")
        failures += _current_is_the_last_promotion(s, tally)
        failures += _no_leftovers(s)
        failures += _versions_match_their_generations(s, run)
        refreshed = s.run(f"SELECT warren.refresh('{GRAPH}')")
        if not refreshed.ok:
            failures.append(f"a refresh after the run failed: {refreshed.sqlstate} {refreshed.message}")
        promoted = s.run("SELECT count(*) FROM warren.versions WHERE NOT staged").value()
        if int(promoted) > RETAIN:
            failures.append(f"{promoted} promoted versions are kept after a refresh, beyond retain_versions {RETAIN}")
        s.run(f"SELECT warren.evict('{GRAPH}')").value()
        left = s.run("SELECT count(*) FROM warren.shared_cache() WHERE kind IN ('version', 'vector')").value()
        unnamed = s.run(UNNAMED_SHARED_BYTES).value()
        if left != "0" or unnamed != unnamed_before:
            failures.append(f"after evict, the shared cache lists {left} entries and {unnamed} unnamed bytes "
                            f"(at the start, {unnamed_before}): {s.run('SELECT * FROM warren.shared_cache()').rows}")
    return failures


def _current_is_the_last_promotion(s, tally):
    current = s.run(f"SELECT current_version FROM warren.graphs WHERE name = '{GRAPH}'").value()
    last = tally.promotions[max(tally.promotions)] if tally.promotions else None
    if last != int(current):
        return [f"current version {current} is not {last}, which the last committed promotion made current"]
    return []


def _no_leftovers(s):
    failures = []
    staged = s.run("SELECT version_id FROM warren.versions WHERE staged").rows
    if staged:
        failures.append(f"staged versions left behind: {staged}")
    vectors = s.run("SELECT version_id, name FROM warren.vectors").rows
    if vectors:
        failures.append(f"vectors left behind: {vectors}")
    partial = s.run("SELECT version_id FROM warren.versions v WHERE bytes <> (SELECT coalesce(sum(length(data)), 0) "
                    "FROM warren.segments s WHERE s.version_id = v.version_id)").rows
    if partial:
        failures.append(f"versions whose segments do not add up to their bytes: {partial}")
    return failures


def _versions_match_their_generations(s, run):
    failures = []
    for version, nodes, edges, dropped in s.run("SELECT version_id, node_count, edge_count, dropped_edges "
                                                "FROM warren.versions ORDER BY version_id").rows:
        gen = generation(s.run(f"SELECT {marker_sql(version)}").value())
        known = run.tally.generations.get(int(version), gen)
        graph = oracle(run.generations.source(gen), gen)
        stats = (int(nodes), int(edges), int(dropped))
        want = (graph.node_count, graph.edge_count, graph.dropped_edges)
        answer = s.run(read_sql("reachable_set", 1, 1, version)).rows[0][1]
        if known != gen or stats != want or answer != expected("reachable_set", graph, 1, 1):
            failures.append(f"version {version}: generation {gen} (read before as {known}), (nodes, edges, dropped) "
                            f"{stats}, expected {want}; reachable from 1: {answer}")
    return failures


MUST_SUCCEED = ("read", "compute", "introspect", "edit", "refresh", "edit+refresh", "stage", "churn")
MIN_PROMOTIONS = 3


def too_little(tally, shares):
    """Why the run proves too little: an operation that never succeeded, too
    few promotions, or, where the cluster `shares` versions (`shared_cache.SHARES`), no
    shared version seen."""
    failures = [f"no {operation} succeeded" for operation in MUST_SUCCEED if not tally.succeeded(operation)]
    if len(tally.promotions) < MIN_PROMOTIONS:
        failures.append(f"only {len(tally.promotions)} promotions committed; at least {MIN_PROMOTIONS} are needed")
    if shares and not tally.shared_versions:
        failures.append("the cluster shares versions, but warren.shared_cache() never listed one")
    return failures


def summary(tally):
    """One line per operation: its successes and each SQLSTATE it failed with."""
    operations = sorted({operation for operation, _ in tally.outcomes})
    lines = []
    for operation in operations:
        states = sorted((state, n) for (op, state), n in tally.outcomes.items() if op == operation)
        lines.append(f"  {operation}: " + ", ".join(f"{'ok' if s == '00000' else s} {n}" for s, n in states))
    lines.append(f"  {len(tally.promotions)} promotions committed, {len(tally.generations)} versions read, "
                 f"at most {tally.shared_versions} shared at once")
    return "\n".join(lines)
