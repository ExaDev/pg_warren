"""What the major upgrade check allows in the server logs."""

from harness.major_upgrade import log_problems

PREFIX = "2026-10-29 09:18:45.670 UTC [7]"


def test_the_strangers_refusal_and_connections_ended_at_shutdown_are_expected():
    lines = [f"{PREFIX} 42501 ERROR:  permission denied for graph social",
             f"{PREFIX} 57P01 FATAL:  terminating connection due to administrator command"]
    assert log_problems("\n".join(lines)) == []


def test_the_launcher_exiting_as_the_old_cluster_stops_is_expected():
    """Through PG 16, a clean shutdown logs the logical replication
    launcher's exit with code 1; pg_upgrade stops the old cluster."""
    line = (f'{PREFIX} 00000 LOG:  background worker "logical replication launcher" (PID 13685) '
            "exited with exit code 1")
    assert log_problems(line) == []


def test_an_internal_error_or_a_crashed_backend_is_a_problem():
    for line in (f"{PREFIX} XX000 ERROR:  segment checksum mismatch",
                 f"{PREFIX} 00000 LOG:  client backend (PID 9) was terminated by signal 11: Segmentation fault"):
        assert log_problems(line) == [f"server log: {line}"]
