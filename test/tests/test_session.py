"""A psql session's statement outcomes: rows, or the SQLSTATE and message."""

import subprocess
import time

import pytest

from harness.session import END, MESSAGE, SESSION_SETTINGS, Outcome, Session, canceled_by_request, outcome, script


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


CANCELED = Outcome([], "57014", "canceling statement due to user request")


def test_only_a_requested_cancel_is_canceled_by_request():
    assert canceled_by_request(CANCELED)
    assert not canceled_by_request(Outcome([], "57014", "canceling statement due to statement timeout"))
    assert not canceled_by_request(Outcome([["1"]], "00000", ""))


def scripted_session(outcomes):
    """A session whose statements answer `outcomes` in turn, then succeed."""
    session = Session.__new__(Session)
    session.canceled_setup, session.ran = 0, []

    def run(sql):
        session.ran.append(sql)
        return outcomes.pop(0) if outcomes else Outcome([["4242"]], "00000", "")
    session.run = run
    return session


def test_a_setup_statement_stopped_by_a_cancel_meant_for_another_client_runs_again_and_is_counted():
    """Through a pooler, a cancel of another client's call can land on this
    client's setup: pg_cancel_backend cancels a backend, not a statement."""
    session = scripted_session([CANCELED, CANCELED])
    session._set("statement_timeout = '120s'")
    assert session.ran == ["SET statement_timeout = '120s'"] * 3
    assert session.canceled_setup == 2


def test_a_setup_statement_that_fails_otherwise_still_fails_the_session():
    session = scripted_session([Outcome([], "22023", "invalid value")])
    with pytest.raises(RuntimeError, match="invalid value"):
        session._set("statement_timeout = 'x'")
    assert session.canceled_setup == 0
