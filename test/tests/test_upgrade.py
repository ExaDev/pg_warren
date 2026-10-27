"""Tests of the upgrade check's own checks (harness/upgrade.py)."""

from harness.upgrade import library_problem, old_format_problem, predates_sparse_offsets


class Answers:
    """A database whose queries all return `rows`."""

    def __init__(self, rows):
        self.rows = rows
        self.asked = []

    def query(self, sql):
        self.asked.append(sql)
        return self.rows

    def execute(self, sql):
        self.asked.append(sql)


def test_the_library_loaded_must_be_the_version_expected():
    database = Answers([["0.1.0"]])
    assert library_problem(database, "0.1.0", "after installing release 0.1.0") is None
    assert database.asked == ["SELECT warren.version()"]
    problem = library_problem(Answers([["0.3.0"]]), "0.1.0", "after installing release 0.1.0")
    assert problem == "after installing release 0.1.0 the library loaded is 0.3.0, not 0.1.0"


def test_only_releases_before_0_7_0_built_csrs_without_sparse_offsets():
    assert predates_sparse_offsets("0.1.0")
    assert predates_sparse_offsets("0.6.0")
    assert not predates_sparse_offsets("0.7.0")
    assert not predates_sparse_offsets("0.10.0")


def test_a_version_from_before_sparse_offsets_must_be_refused_as_one_to_refresh():
    statement, what = "warren.bfs('g', ARRAY[1::bigint], 3)", "reading the old version"
    assert old_format_problem(Answers([["55000:true"]]), statement, what, "0.6.0") is None
    assert old_format_problem(Answers([[""]]), statement, what, "0.6.0") == (
        "reading the old version gave [['']], not 55000 naming warren.refresh")


def test_a_version_from_a_release_with_sparse_offsets_must_still_be_read():
    statement, what = "warren.bfs('g', ARRAY[1::bigint], 3)", "reading the old version"
    assert old_format_problem(Answers([[""]]), statement, what, "0.7.0") is None
    assert old_format_problem(Answers([["55000:true"]]), statement, what, "0.7.0") == (
        "reading the old version gave [['55000:true']], not no error")
