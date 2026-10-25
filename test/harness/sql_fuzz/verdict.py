"""The oracle: what a statement's outcome may be (DESIGN.md §7, §11).

A failure must have a SQLSTATE of a class the surface documents for bad
input, missing objects, privileges, limits and state (`ALLOWED_CLASSES`),
or one of a few single states (`ALLOWED_STATES`): a statement timeout, a
caller's SQL that tried to write in the read-only transaction a function
runs it in, a trigger function called directly. Never an internal error
(XX000), corrupted data (XX001: only a corrupted segment explains it, and
the fuzzer never corrupts one), a raw integrity violation (23xxx: the
catalog's constraints are the extension's to keep), a system error (58),
an aborted transaction it did not cause (25P02), or `WF001`, which the
fixture's hostile operators raise when they run elevated.

A success is a breach when a role that is not a superuser reads a graph
it neither owns nor reads, does what only the owner may to a graph it
does not own (`contract` into an existing graph included), or has the
function run SQL that reads `public.secret`,
which only the superuser may read; except a call Postgres never made (a
STRICT function given a NULL), `evict`, which anyone may call (it then
frees only the caller's own copies), and `report_progress` with a NULL
command, which only clears the caller's frame (DESIGN.md §5.7).

An allowed class still hides a fault of the extension's own SQL: a
privilege error on its schema, its catalog tables or sequences or a
function it calls, or a missing `warren.*` relation or function other
than the one the statement itself calls. Those are `internal` findings,
in a call's result or the server log (`log_faults`), where the
extension's documented warnings (`LOG_EXPECTED`) are expected."""

import re
from dataclasses import dataclass

from ..cluster import SUPERUSER
from .fixture import ESCALATED, SECRET

ALLOWED_CLASSES = frozenset({"22", "42", "53", "54", "55", "0A", "21", "2B", "3F", "40"})
ALLOWED_STATES = frozenset({"57014", "25006", "39P03"})
CATALOG_RELATIONS = r"(graphs|versions|segments|vectors|vector_chunks|vector_quotas)\w*"
INTERNAL = re.compile(r"permission denied for (schema warren\b|(table|sequence|view) " + CATALOG_RELATIONS
                      + r"|function \w+)|relation \"(warren\.\w+|" + CATALOG_RELATIONS + r')" does not exist')
MISSING_FUNCTION = re.compile(r"function warren\.(\w+)\(.*\) does not exist")
# What the server log may show besides errors of allowed SQLSTATEs: the
# warnings the extension documents (an iteration that did not converge,
# a matrix of several components), and a parallel worker that exits after
# the error it raised (the leader raises it again) or that its leader
# stops after an error of its own.
LOG_EXPECTED = (r"WARNING:  \w+ did not (converge|stabilise) in \d+ (iterations|steps)",
                r"WARNING:  the matrix has \d+ connected components",
                r'background worker "parallel worker" \(PID \d+\) exited with exit code 1',
                r'FATAL:  terminating background worker "parallel worker" due to administrator command')
OPEN = frozenset({"evict"})
OWNER_ONLY = frozenset({"refresh", "promote", "drop_version", "drop_graph", "compact", "grant", "revoke",
                        "set_owner", "set_vector_quota"})


@dataclass(frozen=True)
class Finding:
    kind: str  # sqlstate, escalation, access, crash, hang, leak
    detail: str


def allowed(sqlstate):
    return sqlstate in ALLOWED_STATES or (sqlstate[:2] in ALLOWED_CLASSES and sqlstate != ESCALATED)


def judge(role, call, outcome, world):
    """What is wrong with `outcome`, the result of a statement `role` ran
    (`call`, if it is a call, generated in `world`); None if nothing."""
    if not outcome.ok:
        if outcome.sqlstate == ESCALATED:
            return Finding("escalation", outcome.message)
        if not allowed(outcome.sqlstate):
            return Finding("sqlstate", f"{outcome.sqlstate}: {outcome.message}")
        if _internal(outcome.message, call):
            return Finding("internal", f"{outcome.sqlstate}: {outcome.message}")
        return None
    if call is None or role == SUPERUSER or call.skipped() or call.function.name in OPEN or _clears(call):
        return None
    return _breach(role, call, world)


def _internal(message, call):
    """Whether `message` is about the extension's own objects rather than
    the caller's or the statement's (`call`'s) own function."""
    if INTERNAL.search(message):
        return True
    missing = MISSING_FUNCTION.search(message)
    return missing is not None and (call is None or not _calls(call.expression(), missing[1]))


def _calls(sql, name):
    return f"warren.{name}(" in sql or f'warren."{name}"(' in sql


def log_faults(text):
    """The lines of server log `text` that show an internal fault an
    allowed SQLSTATE hides (the log does not say which call it was, so a
    missing function is left to the call's own result)."""
    return [line for line in text.splitlines() if INTERNAL.search(line)]


def _clears(call):
    return call.function.name == "report_progress" and call.positional[0].startswith("NULL")


def _breach(role, call, world):
    if any(SECRET in sql for sql in call.queries):
        return Finding("access", f"{role} ran SQL that reads {SECRET}")
    for name in call.graphs:
        graph = world.graph(name)
        if graph is None:
            continue
        if not graph.may_read(role):
            return Finding("access", f"{role}, neither owner nor reader, called {call.function.name} on {name}")
        if call.function.name in OWNER_ONLY and role != graph.owner:
            return Finding("access", f"{role}, not the owner, called {call.function.name} on {name}")
    out = call.argument("out_graph") if call.function.name == "contract" else None
    into = world.graph(out[1:-1]) if out and out.startswith("'") else None
    if into is not None and role != into.owner:
        return Finding("access", f"{role}, not the owner, contracted into {into.name}")
    return None


def changes(role, before, after):
    """What `role`'s action, between worlds `before` and `after`, changed
    that it may not: a graph it does not own dropped, given away, granted
    or revoked, or its versions or current version changed (whatever the
    function: `reassign_owned`, `drop_owned` and `drop_orphans` included);
    or a persisted vector of another role dropped or replaced on a graph it
    does not own (DESIGN.md §7: a vector's creator or a member of the
    owner role). No fixture role is a member of another, so owning is
    being the owner."""
    if role == SUPERUSER:
        return []
    found = []
    for graph in before.graphs:
        if graph.owner == role:
            continue
        now = after.graph(graph.name)
        shape = (graph.owner, graph.readers, graph.current, _versions(before, graph.name))
        if now is None or (now.owner, now.readers, now.current, _versions(after, graph.name)) != shape:
            what = "dropped" if now is None else "changed"
            found.append(Finding("access", f"{role} {what} graph {graph.name}, owned by {graph.owner}"))
    owners = {graph.name: graph.owner for graph in before.graphs}
    kept = set(after.vectors)
    for vector in before.vectors:
        graph, name, _, creator, *_ = vector
        if creator != role and owners.get(graph) != role and vector not in kept:
            found.append(Finding("access", f"{role} dropped or replaced vector {name} of {graph}, "
                                           f"created by {creator}"))
    return found


def _versions(world, name):
    return sorted((version, staged) for graph, version, staged in world.versions if graph == name)


def signature(function, finding):
    """What makes two findings the same bug: the function, the kind and the
    message's first line, its numbers left out."""
    first = finding.detail.splitlines()[0] if finding.detail else ""
    return function, finding.kind, re.sub(r"0x[0-9a-f]+|\d+", "N", first)
