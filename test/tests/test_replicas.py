"""The replica check's coverage of the SQL surface, its comparisons, its
refusals and what it allows in the server logs."""

import re

import pytest

from harness import replicas
from harness.extension import REPOSITORY
from harness.replicas import (NOT_CALLED, READS, REFUSAL, SERVER_STATE, WRITES, Retirement, compare_reads,
                              log_problems, read_failures, refusals, server_state, wait_for_replay)
from harness.session import Outcome

PREFIX = "2026-10-25 10:00:00.001 UTC [7]"
LABELS = REPOSITORY / "crates" / "pg_warren" / "src" / "parallel_labels.rs"
NAMES = {"compacted": "4", "current": "5", "kg": "6"}


class FakeSession:
    """Answers each statement with the outcome `answer(sql)` gives."""

    def __init__(self, answer):
        self.answer, self.ran = answer, []

    def run(self, sql):
        self.ran.append(sql)
        return self.answer(sql)


def ok(rows=()):
    return Outcome([list(row) for row in rows], "00000", "")


def failed(sqlstate, message):
    return Outcome([], sqlstate, message)


def every_function():
    """Every `warren.*` function and its parallel label, as
    `parallel_labels`'s test declares them."""
    return dict(re.findall(r'\("(\w+)", "([urs])"\)', LABELS.read_text()))


def called(statements):
    return {name for sql in statements for name in re.findall(r"warren\.(\w+)\(", sql)}


def test_every_function_is_read_refused_or_known_not_to_run():
    functions = every_function()
    assert len(functions) > 80
    reads = called([sql for statements in READS.values() for sql in statements] + SERVER_STATE)
    writes = called(WRITES.values())
    missing = set(functions) - reads - writes - set(NOT_CALLED)
    assert not missing, f"neither read nor written on the standby: {sorted(missing)}"


def test_every_function_that_cannot_write_is_read_on_the_standby():
    """`PARALLEL SAFE` and `RESTRICTED` functions write nothing (§9.3), so
    none may be only among the refused writes."""
    reads = called([sql for statements in READS.values() for sql in statements] + SERVER_STATE)
    readable = {name for name, label in every_function().items() if label != "u"}
    assert readable <= reads, sorted(readable - reads)


def test_every_unsafe_function_is_refused_unless_it_persists_nothing():
    unsafe = {name for name, label in every_function().items() if label == "u"}
    assert unsafe - called(WRITES.values()) - set(NOT_CALLED) <= {"gather", "eigen"}, \
        "only gather (no vector) and eigen (stored or not) also run as reads"


def test_the_statements_name_only_known_versions():
    for statements in [*READS.values(), list(WRITES.values())]:
        for sql in statements:
            sql.format(**NAMES)


def test_equal_reads_have_no_differences_and_a_difference_names_the_read():
    expected = {read: [[["1"]] for _ in statements] for read, statements in READS.items()}
    assert compare_reads(expected, FakeSession(lambda sql: ok([["1"]])), NAMES) == []
    differs = compare_reads(expected, FakeSession(lambda sql: ok([["2"]]) if "dijkstra" in sql else ok([["1"]])),
                            NAMES)
    assert len(differs) == 1 and differs[0].startswith("dijkstra: ")


def test_a_read_that_fails_on_the_standby_differs():
    expected = {read: [[["1"]] for _ in statements] for read, statements in READS.items()}
    found = compare_reads(expected, FakeSession(lambda sql: failed("XX000", "cannot assign TransactionIds "
                                                                             "during recovery")), NAMES)
    assert len(found) == len(READS)
    assert "XX000" in found[0]


def test_reads_must_succeed_on_the_primary_but_the_strangers():
    expected = {read: [[["1"]]] for read in READS}
    expected["as a stranger"] = [[], ("42501", "must be a member"), []]
    assert read_failures(expected) == []
    expected["bfs"] = [("42704", "no such graph")]
    assert read_failures(expected) == ["bfs on the primary: [('42704', 'no such graph')]"]
    expected["bfs"], expected["as a stranger"] = [[["1"]]], [[], [["3"]], []]
    assert read_failures(expected) == ["as a stranger on the primary: [[], [['3']], []]"]


def test_a_write_must_be_refused_with_the_read_only_sqlstate_and_message():
    assert refusals(FakeSession(lambda sql: failed(*REFUSAL)), NAMES) == []
    hint = failed(REFUSAL[0], REFUSAL[1] + "\nHINT:  This server is a standby.")
    assert refusals(FakeSession(lambda sql: hint), NAMES) == []
    internal = refusals(FakeSession(lambda sql: failed("XX000", "cannot assign TransactionIds during recovery")),
                        NAMES)
    assert len(internal) == len(WRITES) and internal[0].startswith("create_graph: XX000")
    written = refusals(FakeSession(lambda sql: ok([["7"]])), NAMES)
    assert len(written) == len(WRITES)


def test_the_servers_own_state_must_be_readable():
    assert server_state(FakeSession(lambda sql: ok([["t"]]))) == []
    assert len(server_state(FakeSession(lambda sql: failed("55000", "no table")))) == len(SERVER_STATE)


def test_a_retired_version_is_read_consistently_or_the_read_cancelled():
    retirement = Retirement(None, None, "replicas", feedback=False)
    expected = [["50", "1275"]]
    assert retirement.consistent("a read", ok(expected), expected) == []
    assert retirement.consistent("a read", failed("40001", "canceling statement due to conflict with recovery"),
                                 expected) == []
    assert retirement.seen == ["a read: consistent", "a read: cancelled (40001)"]
    assert retirement.consistent("a read", ok([["49", "1225"]]), expected) != []
    assert retirement.consistent("a read", failed("XX000", "broken"), expected) != []


def test_waiting_for_replay_gives_up():
    behind = FakeSession(lambda sql: ok([["f"]]))
    with pytest.raises(RuntimeError, match="did not replay up to 0/3000000"):
        wait_for_replay(behind, "0/3000000", seconds=0)
    assert wait_for_replay(FakeSession(lambda sql: ok([["t"]])), "0/3000000") is None


def test_refusals_conflicts_and_a_promotions_walreceiver_are_allowed_in_the_logs():
    lines = [f"{PREFIX} 25006 ERROR:  cannot change pg_warren's catalog in a read-only transaction",
             f"{PREFIX} 40001 FATAL:  terminating connection due to conflict with recovery",
             f"{PREFIX} 40001 ERROR:  canceling statement due to conflict with recovery",
             f"{PREFIX} 57P01 FATAL:  terminating walreceiver process due to administrator command"]
    assert log_problems("\n".join(lines)) == []


def test_an_internal_error_or_a_crash_on_a_standby_is_a_problem():
    for line in (f"{PREFIX} XX000 ERROR:  cannot assign TransactionIds during recovery",
                 f"{PREFIX} 00000 LOG:  server process (PID 9) was terminated by signal 11: Segmentation fault",
                 f"{PREFIX} 01000 WARNING:  pagerank did not converge in 100 iterations"):
        assert log_problems(line) == [f"server log: {line}"]


def test_every_server_flushes_commits_and_cascades():
    assert "synchronous_commit = on" in replicas.SETTINGS
    assert "full_page_writes = on" in replicas.SETTINGS
