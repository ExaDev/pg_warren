"""What a pool mode keeps of a client's session state (DESIGN.md §13 M11;
docs/limits.md, "Behind a connection pooler"), probed before the soak
through a pooler of two server connections:

- `setting`: a GUC set with `SET` (`warren.threads`);
- `transient vector`: a vector written with `persist := false`;
- `prepared statement`: SQL `PREPARE`;
- `last_stats`: the stats of the client's last traversal;
- `left by a client`: a transient vector and `last_stats` that a client
  leaves when it disconnects, read by the next client on its server
  backend (session mode resets it with `DISCARD ALL` first).

Client A makes the state; client B, in a transaction begun at once (so
on the server A just released, if A released it: the pool hands out the
last released first), reads it; then A reads it again. A probe says
whether A's state *carried* to its next transaction and whether it
*leaked* to another client; None where the probe could not tell (the
next client did not land on A's backend). Session pooling must carry
every state and leak none; what transaction pooling breaks is reported,
not failed: it is the pool mode's nature."""

from dataclasses import dataclass

from ..session import Session
from .fixture import DATABASE

PROBES = {
    "setting": ("SET warren.threads = 1", "SELECT current_setting('warren.threads')", "1"),
    "transient vector": ("SELECT warren.vector_const('g', 'probe', 1, persist := false)",
                         "SELECT warren.vector_reduce('g', 'probe', 'sum') > 0", "t"),
    "prepared statement": ("PREPARE probe AS SELECT warren.degree('g', 1::bigint) >= 0", "EXECUTE probe", "t"),
    "last_stats": ("SELECT count(*) FROM warren.bfs('g', ARRAY[1::bigint], 2)",
                   "SELECT warren.last_stats() ->> 'function'", "bfs"),
    "left by a client": None,
}
LEFT_STATE = ("SELECT warren.vector_const('g', 'probe_left', 1, persist := false)",
              "SELECT count(*) FROM warren.reachable_set('g', ARRAY[1::bigint], 2)")
LEFT_READS = {"transient vector": "SELECT warren.vector_reduce('g', 'probe_left', 'sum') > 0",
              "last_stats": "SELECT warren.last_stats() ->> 'function' = 'reachable_set'"}
POOL_SIZE = 2


@dataclass(frozen=True)
class Probe:
    mode: str
    name: str
    carried: bool | None
    leaked: bool | None
    detail: str


def failures(probes):
    """What session pooling lost or leaked; transaction pooling's probes are reported only."""
    found = []
    for probe in probes:
        if probe.mode != "session":
            continue
        if probe.carried is False:
            found.append(f"session pooling: {probe.name} was lost between a client's transactions ({probe.detail})")
        if probe.leaked:
            found.append(f"session pooling: {probe.name} leaked to another client ({probe.detail})")
    return found


def probe(pooler, mode, role):
    """Every probe of `PROBES` through `pooler` (pool mode `mode`) as `role`."""
    return [_left_by_a_client(pooler, mode, role) if steps is None else _carried(pooler, mode, role, name, *steps)
            for name, steps in PROBES.items()]


def _session(pooler, role):
    return Session(pooler, DATABASE, user=role)


def _pid(s):
    return s.run("SELECT pg_backend_pid()").value()


def _carried(pooler, mode, role, name, make, read, expect):
    with _session(pooler, role) as a, _session(pooler, role) as b:
        made = a.run(make)
        if not made.ok:
            raise RuntimeError(f"probe {name}: {make}: {made.sqlstate} {made.message}")
        b.run("BEGIN")
        b_pid, b_read = _pid(b), b.run(read)
        a_pid, a_read = _pid(a), a.run(read)
        b.run("ROLLBACK")
    carried = a_read.ok and a_read.rows == [[expect]]
    leaked = b_read.ok and b_read.rows == [[expect]]
    detail = (f"A read it on backend {a_pid}: {_said(a_read)}; B, in a transaction on backend {b_pid}: "
              f"{_said(b_read)}")
    return Probe(mode, name, carried, leaked, detail)


def _left_by_a_client(pooler, mode, role):
    with _session(pooler, role) as a:
        for sql in LEFT_STATE:
            made = a.run(sql)
            if not made.ok:
                raise RuntimeError(f"probe left by a client: {sql}: {made.sqlstate} {made.message}")
        a_pid = _pid(a)
    with _session(pooler, role) as c:
        c_pid = _pid(c)
        reads = {name: c.run(sql) for name, sql in LEFT_READS.items()}
    seen = sorted(name for name, done in reads.items() if done.ok and done.rows == [["t"]])
    detail = (f"A left them on backend {a_pid}; the next client, on backend {c_pid}, "
              f"found {', '.join(seen) or 'neither'}")
    return Probe(mode, "left by a client", None, bool(seen) if seen or c_pid == a_pid else None, detail)


def _said(done):
    return done.rows[0][0] if done.ok and done.rows else f"{done.sqlstate} {done.message}"
