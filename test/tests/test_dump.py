"""The dump/restore check's commands and comparison."""

from pathlib import Path

from harness.dump import (GRAPHS, MANAGED_ADMIN, MANAGED_ROLES, SEED, STATE, TRIGGER, differences, missing_roles,
                          restore_command, restore_errors, unexpected_warnings)


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


def test_a_plain_dump_may_be_restored_past_its_errors():
    command = restore_command(FakeCluster(), "db", Path("d.sql"), custom=False, stop_on_error=False)
    assert command[0] == "/pg/bin/psql" and "ON_ERROR_STOP=1" not in command


def test_only_the_event_trigger_may_fail_a_non_superusers_restore():
    stderr = """psql:/tmp/t.sql:40: ERROR:  permission denied to create event trigger "warren_drop_owned"
HINT:  Must be superuser to create an event trigger.
psql:/tmp/t.sql:41: ERROR:  event trigger "warren_drop_owned" does not exist
psql:/tmp/t.sql:50: ERROR:  relation "public.nodes" does not exist
"""
    assert restore_errors(stderr) == (
        ['permission denied to create event trigger "warren_drop_owned"',
         'event trigger "warren_drop_owned" does not exist'],
        ['relation "public.nodes" does not exist'],
    )


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


def test_restore_warnings_are_unexpected():
    stderr = """pg_restore: warning: errors ignored on restore: 1
psql:/tmp/d.sql:12: WARNING:  no privileges were granted for "graphs"
"""
    assert unexpected_warnings(stderr) == [
        "warning: errors ignored on restore: 1",
        'warning: no privileges were granted for "graphs"',
    ]


def test_the_managed_administrator_is_no_superuser_and_holds_the_predefined_roles_as_rds_superuser_does():
    statements = " ".join(MANAGED_ROLES.split())
    assert "SUPERUSER" not in statements.replace("NOSUPERUSER", "")
    assert f"CREATE ROLE {MANAGED_ADMIN} LOGIN NOSUPERUSER CREATEROLE CREATEDB" in statements
    assert "GRANT pg_read_all_data, pg_write_all_data TO warren_rds_superuser WITH ADMIN OPTION" in statements
    assert f"GRANT warren_rds_superuser TO {MANAGED_ADMIN}" in statements


def test_only_the_superusers_seed_creates_the_event_trigger():
    assert TRIGGER not in GRAPHS and SEED == GRAPHS + TRIGGER


def test_roles_missing_on_the_target_are_named_once_each():
    stderr = """pg_restore: error: could not execute query: ERROR:  role "dump_reader" does not exist
CONTEXT:  COPY graphs, line 1, column readers: "{dump_reader}"
psql:/tmp/d.sql:9: ERROR:  role "dump_reader" does not exist
psql:/tmp/d.sql:12: ERROR:  role "gone" does not exist
psql:/tmp/d.sql:13: ERROR:  relation "public.nodes" does not exist
"""
    assert missing_roles(stderr) == ["dump_reader", "gone"]
