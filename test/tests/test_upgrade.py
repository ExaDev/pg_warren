"""Tests of the upgrade check's own checks (harness/upgrade.py)."""

from harness.upgrade import library_problem


class Answers:
    """A database whose queries all return `rows`."""

    def __init__(self, rows):
        self.rows = rows
        self.asked = []

    def query(self, sql):
        self.asked.append(sql)
        return self.rows


def test_the_library_loaded_must_be_the_version_expected():
    database = Answers([["0.1.0"]])
    assert library_problem(database, "0.1.0", "after installing release 0.1.0") is None
    assert database.asked == ["SELECT warren.version()"]
    problem = library_problem(Answers([["0.3.0"]]), "0.1.0", "after installing release 0.1.0")
    assert problem == "after installing release 0.1.0 the library loaded is 0.3.0, not 0.1.0"
