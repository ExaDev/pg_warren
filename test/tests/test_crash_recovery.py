"""The crash recovery check's scenarios, its comparison and what each way
of stopping a refresh may leave in the server log."""

import threading

import pytest

from harness import crash_recovery
from harness.crash_recovery import (EDGE_QUERY, GATE, KINDS, POINTS, SNAPSHOT, Interrupted, Scenario, differences,
                                    log_problems, refresh_builds_the_source, setup_sql)
from harness.session import BackendLost, Outcome

PREFIX = "2026-10-25 10:00:00.001 UTC [7]"


def test_every_point_is_tried_in_every_way():
    assert POINTS == ("reading edges", "writing segments", "before commit")
    assert KINDS == ("terminate", "kill", "immediate")


def test_the_edge_query_waits_on_the_gate_shared():
    assert f"pg_advisory_xact_lock_shared({GATE})" in EDGE_QUERY
    assert "CREATE TABLE public.edges" in setup_sql(10, 100) and "generate_series(1::bigint, 100)" in setup_sql(10, 100)


def test_equal_snapshots_have_no_differences_and_others_name_the_part():
    before = {part: [["1"]] for part in SNAPSHOT}
    assert differences(before, before) == []
    after = dict(before, segments=[["1"], ["2"]])
    assert differences(before, after) == ["segments changed: [['1']] → [['1'], ['2']]"]


def test_a_terminated_backends_fatal_is_expected_but_no_crash():
    fatal = f"{PREFIX} 57P01 FATAL:  terminating connection due to administrator command"
    crash = f"{PREFIX} 00000 LOG:  server process (PID 9) was terminated by signal 9: Killed"
    assert log_problems(fatal, "terminate") == []
    assert log_problems(crash, "terminate") == [f"server log: {crash}"]
    assert log_problems(crash, "kill") == []
    named = f"{PREFIX} 00000 LOG:  client backend (PID 9) was terminated by signal 9: Killed"
    assert log_problems(named, "kill") == [], "as PG 17 and later name it"


def test_a_crash_explains_the_backends_it_ends_and_the_connections_it_refuses():
    lines = [f"{PREFIX} 57P02 WARNING:  terminating connection because of crash of another server process",
             f"{PREFIX} 57P03 FATAL:  the database system is in recovery mode",
             f"{PREFIX} 00000 LOG:  terminating any other active server processes"]
    assert log_problems("\n".join(lines), "kill") == []
    assert log_problems(lines[0], "terminate") == [f"server log: {lines[0]}"]


def test_no_way_of_stopping_explains_an_internal_error_or_a_panic():
    for line in (f"{PREFIX} XX000 ERROR:  pg_warren's shared cache: broken", f"{PREFIX} XX000 PANIC:  oops"):
        for kind in KINDS:
            assert log_problems(line, kind) == [f"server log: {line}"]


def scenario(point):
    return Scenario(cluster=None, database="crash", point=point, kind="terminate")


def test_a_refresh_stopped_writing_was_building_or_writing():
    assert scenario("writing segments").point_reached(Interrupted(1, "building", "4")) == []
    assert scenario("writing segments").point_reached(Interrupted(1, "writing", "4")) == []
    assert scenario("writing segments").point_reached(Interrupted(1, "reading edges", None)) == [
        "stopped at 'reading edges', not at writing segments"]


def test_a_refresh_stopped_before_commit_shows_no_progress():
    assert scenario("before commit").point_reached(Interrupted(1, None, None)) == []
    assert scenario("before commit").point_reached(Interrupted(1, "writing", "4")) == [
        "stopped at 'writing', not at before commit"]


def test_where_another_backends_progress_does_not_show_the_point_is_not_checked():
    assert scenario("reading edges").point_reached(Interrupted(1, None, None)) == []


class StuckSession:
    """A session whose refresh blocks until the session is closed, as one
    waiting on the gate does, and whose other statements succeed."""

    def __init__(self, sessions):
        self.pid, self.closed = 1000 + len(sessions), threading.Event()
        sessions.append(self)

    def run(self, sql):
        if "warren.refresh" in sql:
            self.closed.wait()
            raise BackendLost("closed")
        return Outcome([["1"]], "00000", "")

    def close(self):
        self.closed.set()


def test_an_interruption_that_fails_closes_its_sessions_and_ends_its_worker(monkeypatch):
    sessions = []
    run = scenario("reading edges")
    monkeypatch.setattr(run, "session", lambda: StuckSession(sessions))

    def never(*_args, **_kwargs):
        raise RuntimeError("gave up waiting")
    monkeypatch.setattr(crash_recovery, "until", never)
    before = threading.active_count()
    with pytest.raises(RuntimeError, match="gave up waiting"):
        run.interrupt()
    assert [session.closed.is_set() for session in sessions] == [True, True]
    assert threading.active_count() == before


class SourceSession:
    """A session after recovery whose refresh builds version 5 with the
    source's 200000 nodes and edges, and that records its statements.
    Counting the source's distinct edges fails as `distinct` says."""

    def __init__(self, distinct=Outcome([["200000", "200000"]], "00000", "")):
        self.statements, self.distinct = [], distinct

    def run(self, sql):
        self.statements.append(sql)
        if "warren.refresh" in sql:
            return Outcome([["5"]], "00000", "")
        if "FROM warren.versions" in sql:
            return Outcome([["200000", "200000"]], "00000", "")
        if "DISTINCT" in sql:
            return self.distinct
        return Outcome([], "00000", "")


def test_a_refresh_that_builds_the_source_is_no_failure():
    assert refresh_builds_the_source(SourceSession()) == []


def test_the_source_is_counted_by_sorting_not_hashing():
    """A parallel HashAggregate of the 2M unanalysed edge rows sometimes
    spills into ~10^5 batches and runs for minutes (seen on PG 16 and 18,
    with or without pg_warren): the count is made with hash aggregation off."""
    s = SourceSession()
    refresh_builds_the_source(s)
    counted = next(i for i, sql in enumerate(s.statements) if "DISTINCT" in sql)
    assert "SET enable_hashagg = off" in s.statements[:counted]
    assert s.statements[counted + 1:] == ["RESET enable_hashagg"]


def test_a_source_count_that_fails_says_how():
    timeout = Outcome([], "57014", "canceling statement due to statement timeout")
    assert refresh_builds_the_source(SourceSession(timeout)) == [
        "counting the source after recovery failed: 57014 canceling statement due to statement timeout"]
