"""Shrinking: delta debugging over a failure's statements, and the psql
script a shrunk failure is printed as."""

from harness.sql_fuzz.actions import CONNECT
from harness.sql_fuzz.run import Executed
from harness.sql_fuzz.shrink import _growing, ddmin, fixture_script, script


def test_ddmin_finds_the_two_items_that_together_make_the_failure():
    tests = []

    def fails(items):
        tests.append(items)
        return 3 in items and 7 in items

    assert ddmin(list(range(10)), fails) == [3, 7]
    assert len(tests) < 40


def test_ddmin_empties_a_list_none_of_which_matters():
    assert ddmin([1, 2, 3], lambda items: True) == []


def test_ddmin_stops_when_told_to():
    assert ddmin([1, 2, 3], lambda items: True, more=lambda: False) == [1, 2, 3]


def test_the_actions_before_a_failure_are_tried_in_doubling_suffixes():
    assert list(_growing(5)) == [0, 1, 2, 4, 5]
    assert list(_growing(0)) == [0]


def test_a_script_reconnects_where_the_role_changes_or_a_session_was_opened():
    actions = [[Executed("fuzz_owner", CONNECT), Executed("fuzz_owner", "SET search_path = hostile")],
               [Executed("fuzz_owner", "SELECT 1"), Executed("postgres", "SELECT 2;")]]
    assert script(actions) == ("\\c - fuzz_owner\nSET search_path = hostile;\nSELECT 1;\n"
                               "\\c - postgres\nSELECT 2;")


def test_the_fixture_script_ends_as_the_superuser():
    text = fixture_script()
    assert text.startswith("\\c - postgres\n")
    assert "\\c - fuzz_owner\nSELECT warren.create_graph('g8'" in text
    assert text.endswith("\\c - postgres\n")
