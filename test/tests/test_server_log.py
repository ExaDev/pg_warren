"""Reading a scratch cluster's server log for trouble: errors with
SQLSTATEs nobody expected, crashes, assertion failures and panics."""

from harness.server_log import SETTINGS, problems

LOG = """\
2026-10-25 10:00:00.001 UTC [100] 00000 LOG:  database system is ready to accept connections
2026-10-25 10:00:01.002 UTC [101] 42704 ERROR:  graph "g" has no version 7
2026-10-25 10:00:01.002 UTC [101] 42704 STATEMENT:  SELECT warren.degree('g', 1::bigint, version := 7)
2026-10-25 10:00:02.003 UTC [102] XX000 ERROR:  internal error: index out of bounds
2026-10-25 10:00:02.004 UTC [102] XX000 CONTEXT:  SQL function
"""


def test_the_prefix_puts_each_lines_sqlstate_in_the_log():
    assert "log_line_prefix = '%m [%p] %e '" in SETTINGS


def test_an_error_whose_sqlstate_is_expected_is_no_problem():
    assert problems(LOG, allowed={"42704", "XX000"}) == []


def test_an_error_of_another_sqlstate_is_a_problem_but_not_its_context_lines():
    assert problems(LOG, allowed={"42704"}) == [
        "2026-10-25 10:00:02.003 UTC [102] XX000 ERROR:  internal error: index out of bounds"]


def test_warnings_and_fatals_are_checked_as_errors_are():
    log = ("2026-10-25 10:00:00.001 UTC [7] 01000 WARNING:  something leaked\n"
           "2026-10-25 10:00:00.001 UTC [7] 57P01 FATAL:  terminating connection due to administrator command\n")
    assert problems(log, allowed={"57P01"}) == [log.splitlines()[0]]


def test_a_panic_is_always_a_problem():
    line = "2026-10-25 10:00:00.001 UTC [7] XX000 PANIC:  could not write to file"
    assert problems(line, allowed={"XX000"}) == [line]


def test_crashes_assertions_and_rust_panics_are_problems():
    log = ("2026-10-25 10:00:00.001 UTC [1] 00000 LOG:  server process (PID 9) was terminated by signal 11: "
           "Segmentation fault\n"
           "TRAP: failed Assert(\"false\"), File: \"x.c\", Line: 1, PID: 9\n"
           "thread '<unnamed>' panicked at crates/warren-core/src/lib.rs:1:1:\n"
           "2026-10-25 10:00:00.001 UTC [1] 00000 LOG:  terminating any other active server processes\n"
           "2026-10-25 10:00:00.001 UTC [1] 00000 LOG:  checkpoint starting: time\n")
    assert problems(log) == log.splitlines()[:4]


def test_expected_lines_are_exempt():
    log = ("2026-10-25 10:00:00.001 UTC [1] 00000 LOG:  server process (PID 9) was terminated by signal 9: Killed\n"
           "2026-10-25 10:00:00.001 UTC [1] 00000 LOG:  terminating any other active server processes\n"
           "2026-10-25 10:00:00.001 UTC [1] 00000 LOG:  server process (PID 8) was terminated by signal 11\n")
    assert problems(log, expected=(r"terminated by signal 9: Killed", r"terminating any other")) == [
        log.splitlines()[2]]
