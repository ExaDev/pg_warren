"""Shrinking: delta debugging over a failure's statements, and the psql
script a shrunk failure is printed as."""

from harness.sql_fuzz.actions import CONNECT
from harness.sql_fuzz.run import Executed, Run, replay
from harness.sql_fuzz.verdict import Finding
from harness.sql_fuzz.shrink import Shrinker, _growing, ddmin, fixture_script, script


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


def test_a_run_shrinks_at_most_its_cap_of_failures():
    shrinker = Shrinker(None, "fuzz_template", 60, cap=2)
    assert [shrinker.take() for _ in range(4)] == [True, True, False, False]


def test_a_replay_stops_at_the_deadline():
    class Untouched:
        failures = {}

        def __getattr__(self, name):
            raise AssertionError(f"replayed past the deadline: {name}")

    actions = [[Executed("fuzz_owner", "SELECT 1")]]
    assert replay(Untouched(), actions, more=lambda: False) is None


def test_a_finding_made_after_its_action_ended_ends_its_transcript_with_that_action():
    run = Run(None, "fuzz_1", quiet=True)
    drop = Executed("fuzz_reader", "SELECT warren.drop_owned('fuzz_reader')")
    run.action = [drop]
    run.end_action()
    run._found(Finding("access", "fuzz_reader changed graph g8, owned by fuzz_owner"), drop)
    (failure,) = run.failures.values()
    assert failure.transcript == [[drop]]
