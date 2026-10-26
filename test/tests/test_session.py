"""A psql session's statement outcomes: rows, or the SQLSTATE and message."""

import subprocess
import time

import pytest

from harness.session import END, MESSAGE, SESSION_SETTINGS, Outcome, Session, outcome, script


def test_the_script_runs_the_statement_and_then_echoes_both_markers():
    assert script("SELECT 1;  ") == (f"SELECT 1;\n\\echo {MESSAGE} :LAST_ERROR_MESSAGE\n"
                                     f"\\echo {END} :SQLSTATE\n")


def test_a_statement_a_meta_command_sends_takes_no_semicolon():
    assert script("SELECT $1::int \\bind 5 \\g").startswith("SELECT $1::int \\bind 5 \\g\n\\echo")


def test_a_success_has_its_rows_split_on_tabs_and_no_message():
    done = outcome(["1\ta", "2\tb", f"{MESSAGE} an earlier statement's error", f"{END} 00000"])
    assert done == Outcome([["1", "a"], ["2", "b"]], "00000", "")
    assert done.ok


def test_a_failure_keeps_its_sqlstate_and_its_message_across_lines():
    done = outcome([f"{MESSAGE} graph \"g\" has no version 7", "second line", f"{END} 42704"])
    assert (done.rows, done.sqlstate, done.message) == ([], "42704", 'graph "g" has no version 7\nsecond line')
    assert not done.ok


def test_value_is_the_only_rows_first_column():
    assert Outcome([["5", "x"]], "00000", "").value() == "5"


def test_value_of_a_failure_names_it():
    with pytest.raises(RuntimeError, match="42704: gone"):
        Outcome([], "42704", "gone").value()


def test_value_needs_exactly_one_row():
    with pytest.raises(RuntimeError, match="one row"):
        Outcome([["1"], ["2"]], "00000", "").value()


def test_every_session_bounds_its_statements_and_notices_a_lost_client():
    assert "statement_timeout = '120s'" in SESSION_SETTINGS
    assert "client_connection_check_interval = '1s'" in SESSION_SETTINGS


def test_closing_a_session_whose_psql_does_not_end_kills_it():
    session = Session.__new__(Session)
    session.process = subprocess.Popen(["sleep", "60"], stdin=subprocess.PIPE, text=True)
    started = time.monotonic()
    session.close(seconds=0.2)
    assert session.process.returncode is not None
    assert time.monotonic() - started < 10
