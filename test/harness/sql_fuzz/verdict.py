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
does not own, or has the function run SQL that reads `public.secret`,
which only the superuser may read; except a call Postgres never made (a
STRICT function given a NULL), `evict`, which anyone may call (it then
frees only the caller's own copies), and `report_progress` with a NULL
command, which only clears the caller's frame (DESIGN.md §5.7)."""

import re
from dataclasses import dataclass

from ..cluster import SUPERUSER
from .fixture import ESCALATED, SECRET

ALLOWED_CLASSES = frozenset({"22", "42", "53", "54", "55", "0A", "21", "2B", "3F", "40"})
ALLOWED_STATES = frozenset({"57014", "25006", "39P03"})
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
        return None
    if call is None or role == SUPERUSER or call.skipped() or call.function.name in OPEN or _clears(call):
        return None
    return _breach(role, call, world)


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
    return None


def signature(function, finding):
    """What makes two findings the same bug: the function, the kind and the
    message's first line, its numbers left out."""
    first = finding.detail.splitlines()[0] if finding.detail else ""
    return function, finding.kind, re.sub(r"0x[0-9a-f]+|\d+", "N", first)
