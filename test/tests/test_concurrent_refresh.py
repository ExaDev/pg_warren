"""The concurrent refresh check's reads, their oracle answers, the failures
it allows and what it counts as proving too little."""

import threading
import time

from harness.concurrent_refresh import (MUST_SUCCEED, READS, Tally, allowed, expected, generation, joined,
                                        read_sql, repr_cost, summary, too_little)
from harness.generations import MARK, Source, oracle
from harness.session import Outcome

GRAPH = oracle(Source(frozenset({1, 2, 3}), {(1, 2): 2.0, (2, 3): 5.5, (1, 3): 9.0}), 4)


def test_every_read_also_reads_the_marker_of_the_same_version():
    for kind in READS:
        sql = read_sql(kind, 1, 3, 17)
        assert sql.startswith("SELECT (SELECT max(n) FROM warren.neighbours('g', 0::bigint, version := 17) n), (")
        assert sql.count("version := 17") == 2, kind


def test_a_read_of_the_current_version_names_no_version():
    assert read_sql("degree", 1, 1, "NULL").count("version := NULL") == 2


def test_expected_answers_are_printed_as_postgres_prints_them():
    assert expected("reachable_set", GRAPH, 1, 1) == "3"
    assert expected("bfs", GRAPH, 1, 1) == "3/1"
    assert expected("neighbours", GRAPH, 1, 1) == "2,3"
    assert expected("neighbours", GRAPH, 3, 1) == ""
    assert expected("dijkstra", GRAPH, 1, 3) == "7.5"
    assert expected("dijkstra", GRAPH, 3, 1) == "none"
    assert expected("walk", GRAPH, 1, 1) == "1"
    assert expected("degree", GRAPH, 2, 1) == "1"


def test_costs_print_without_a_point_when_whole():
    assert (repr_cost(7.0), repr_cost(7.25)) == ("7", "7.25")


def test_a_marker_names_its_generation():
    assert generation(str(MARK + 12)) == 12


def test_a_retired_version_is_allowed_to_reads_but_not_to_introspection():
    assert allowed("read", "42704", 'graph "g" has no version 12')
    assert not allowed("read", "42704", 'graph "h" does not exist')
    assert not allowed("introspect", "42704", 'graph "g" has no version 12')


def test_a_lock_timeout_is_allowed_only_to_the_refreshers():
    assert allowed("refresh", "55P03", "canceling statement due to lock timeout")
    assert not allowed("read", "55P03", "canceling statement due to lock timeout")


def test_a_tally_keeps_each_failure_that_is_not_allowed():
    tally = Tally()
    tally.count("read", Outcome([["1"]], "00000", ""))
    tally.count("read", Outcome([], "42704", 'graph "g" has no version 3'))
    tally.count("read", Outcome([], "XX000", "internal error"))
    assert tally.succeeded("read") == 1
    assert tally.unexpected == [("read", "XX000", "internal error")]
    assert "read: ok 1, 42704 1, XX000 1" in summary(tally)


def test_a_version_read_as_two_generations_is_a_mismatch():
    tally = Tally()
    tally.saw("5", 1)
    tally.saw(5, 1)
    assert tally.mismatches == []
    tally.saw(5, 2)
    assert tally.mismatches == ["version 5 read as generation 1 and 2"]


def test_a_run_without_every_operation_and_promotions_proves_too_little():
    tally = Tally()
    failures = too_little(tally, shares=True)
    assert len(failures) == len(MUST_SUCCEED) + 2
    for operation in MUST_SUCCEED:
        tally.count(operation, Outcome([], "00000", ""))
    for order in range(3):
        tally.promoted(order, order + 10)
    assert too_little(tally, shares=False) == []
    assert too_little(tally, shares=True) == ["the cluster shares versions, but warren.shared_cache() never listed one"]


def test_joining_reports_the_workers_that_do_not_stop_in_time():
    release = threading.Event()
    stuck = threading.Thread(target=release.wait, name="stuck", daemon=True)
    done = threading.Thread(target=lambda: None, name="done")
    for thread in (stuck, done):
        thread.start()
    started = time.monotonic()
    assert joined([stuck, done], 0.2) == ["stuck"]
    assert time.monotonic() - started < 5
    release.set()
    assert joined([stuck, done], 5) == []
