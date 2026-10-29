"""Capturing a database's graphs and comparing two captures: the catalog
by table, the extension's version, the versions named and every read."""

from harness import graph_state, replicas
from harness.graph_state import State, capture, differences, writes_work
from harness.session import Outcome


def ok(rows=()):
    return Outcome([list(row) for row in rows], "00000", "")


class FakeSession:
    """Answers each statement with `answer(sql)`, and records them."""

    def __init__(self, answer):
        self.answer, self.ran = answer, []

    def run(self, sql):
        self.ran.append(sql)
        return self.answer(sql)


def catalog_answer(sql):
    if sql == graph_state.TABLES_SQL:
        return ok([["graphs"], ["segments"]])
    if sql.startswith("SELECT count(*), md5"):
        return ok([["2", "abc" if "segments" in sql else "def"]])
    if sql == replicas.VERSIONS_SQL:
        return ok([["4", "5", "6"]])
    if sql == graph_state.EXTENSION_SQL:
        return ok([["0.8.0"]])
    return ok([["1"]])


def state(**changed):
    parts = dict(catalog={"graphs": [["2", "def"]]}, extension="0.8.0", names={"current": "5"},
                 reads={"bfs": [[["1"]]]})
    parts.update(changed)
    return State(**parts)


def test_a_capture_digests_every_catalog_table_and_runs_every_read():
    s = FakeSession(catalog_answer)
    captured = capture(s)
    assert captured.catalog == {"graphs": [["2", "def"]], "segments": [["2", "abc"]]}
    assert captured.extension == "0.8.0"
    assert captured.names == {"compacted": "4", "current": "5", "kg": "6"}
    assert set(captured.reads) == set(replicas.READS)


def test_the_digest_reads_every_row_of_the_table_whatever_its_order():
    sql = graph_state.digest_sql("segments")
    assert "FROM warren.segments t" in sql and "ORDER BY md5(t::text)" in sql


def test_equal_captures_have_no_differences():
    assert differences(state(), state()) == []


def test_each_part_that_differs_is_named():
    assert differences(state(), state(catalog={"graphs": [["3", "x"]]})) == [
        "warren.graphs: [['2', 'def']] before, [['3', 'x']] after"]
    assert differences(state(), state(catalog={})) == ["warren.graphs: [['2', 'def']] before, None after"]
    assert differences(state(), state(extension="0.9.0")) == ["pg_warren 0.8.0 before, 0.9.0 after"]
    assert differences(state(), state(names={"current": "6"})) == [
        "the versions read are {'current': '5'} before, {'current': '6'} after"]
    assert differences(state(), state(reads={"bfs": [("XX000", "boom")]})) == [
        "bfs: [[['1']]] before, [('XX000', 'boom')] after"]


def test_writes_that_succeed_and_leave_what_they_should_are_no_failure():
    s = FakeSession(lambda sql: ok([["t", "0"]]) if sql == graph_state.WRITTEN else ok([["1"]]))
    assert writes_work(s) == []
    assert s.ran == [*graph_state.WRITES, graph_state.WRITTEN]


def test_the_first_write_that_fails_is_reported_and_the_rest_not_run():
    failing = graph_state.WRITES[1]
    s = FakeSession(lambda sql: Outcome([], "XX000", "boom") if sql == failing else ok())
    assert writes_work(s) == [f"{failing}: XX000 boom"]
    assert s.ran == list(graph_state.WRITES[:2])


def test_writes_that_leave_the_wrong_vectors_are_reported():
    s = FakeSession(lambda sql: ok([["f", "1"]]) if sql == graph_state.WRITTEN else ok())
    assert writes_work(s) == [f"after the writes, {graph_state.WRITTEN} gave 00000 [['f', '1']]"]
