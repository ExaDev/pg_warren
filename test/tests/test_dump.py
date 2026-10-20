"""The dump/restore check's commands and comparison."""

from pathlib import Path

from harness.dump import STATE, differences, restore_command, unexpected_warnings


class FakeCluster:
    def client(self, program):
        return f"/pg/bin/{program}"

    def connection(self, name, user="postgres"):
        return ("-d", name, "-U", user)


def test_a_custom_dump_is_restored_by_pg_restore_one_item_at_a_time():
    command = restore_command(FakeCluster(), "db", Path("d.custom"), custom=True)
    assert command[0] == "/pg/bin/pg_restore"
    assert "--exit-on-error" in command and "--single-transaction" not in command


def test_a_plain_dump_is_restored_by_psql_stopping_at_the_first_error():
    command = restore_command(FakeCluster(), "db", Path("d.sql"), custom=False, user="installer")
    assert command[0] == "/pg/bin/psql" and "ON_ERROR_STOP=1" in command
    assert ("-U", "installer") == tuple(command[command.index("-U"):command.index("-U") + 2])
    assert "-1" not in command and "--single-transaction" not in command


def test_equal_states_have_no_differences():
    state = {kind: [["1", "a"]] for kind in STATE}
    assert differences(state, state, "restored") == ""


def test_a_changed_row_is_reported_with_its_kind():
    before = {kind: [["1"]] for kind in STATE}
    after = dict(before, vectors=[["2"]])
    diff = differences(before, after, "restored")
    assert "restored: vectors" in diff and "-1" in diff and "+2" in diff


SELF_REFERENCE = """pg_dump: warning: there are circular foreign-key constraints on this table:
pg_dump: detail: versions
pg_dump: hint: You might not be able to restore the dump without using --disable-triggers or temporarily dropping the constraints.
pg_dump: hint: Consider using a full dump instead of a --data-only dump to avoid this problem.
"""


def test_the_versions_self_reference_warning_is_expected():
    assert unexpected_warnings(SELF_REFERENCE) == []


def test_a_cycle_between_tables_is_an_unexpected_warning():
    cycle = """pg_dump: warning: there are circular foreign-key constraints among these tables:
pg_dump: detail: graphs
pg_dump: detail: versions
"""
    found = unexpected_warnings(SELF_REFERENCE + cycle)
    assert found == ["warning: there are circular foreign-key constraints among these tables: graphs, versions"]
