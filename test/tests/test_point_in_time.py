"""The point-in-time recovery check's settings, its targets and what it
allows in the server logs."""

from pathlib import Path

from harness import point_in_time
from harness.point_in_time import CHANGES, LATER, holds, log_problems, recovery_settings
from harness.session import Outcome

PREFIX = "2026-10-29 10:00:00.001 UTC [7]"


def test_the_primary_archives_each_wal_file_once_beside_its_data_directory():
    settings = "\n".join(point_in_time.SETTINGS)
    assert "archive_mode = on" in settings
    assert "test ! -f ../archive/%f && cp %p ../archive/%f" in settings
    assert "full_page_writes = on" in settings and "synchronous_commit = on" in settings


def test_a_named_target_recovers_from_the_archive_to_it_and_promotes():
    settings = recovery_settings(Path("/tmp/primary/archive"), "built")
    assert "restore_command = 'cp /tmp/primary/archive/%f %p'" in settings
    assert "recovery_target_name = 'built'" in settings
    assert "recovery_target_action = 'promote'" in settings


def test_no_target_recovers_to_the_end_of_the_archive():
    settings = recovery_settings(Path("/tmp/primary/archive"), None)
    assert not [line for line in settings if line.startswith("recovery_target")]
    assert "restore_command = 'cp /tmp/primary/archive/%f %p'" in settings


def test_each_change_after_the_backup_is_a_target_and_the_later_one_is_not():
    assert [name for name, _ in CHANGES] == ["built", "staged"]
    assert all(statements for _, statements in CHANGES) and LATER


def test_a_wal_file_is_archived_once_the_archiver_has_reached_it():
    assert holds(Outcome([["t"]], "00000", ""))
    assert not holds(Outcome([["f"]], "00000", ""))
    assert not holds(Outcome([[""]], "00000", "")), "nothing archived yet"


def test_the_retired_versions_and_dropped_graphs_reads_name_are_expected_in_the_logs():
    lines = [f"{PREFIX} 42704 ERROR:  graph \"churn\" does not exist",
             f"{PREFIX} 42501 ERROR:  permission denied for graph social",
             f"{PREFIX} 57P01 FATAL:  terminating connection due to administrator command"]
    assert log_problems("\n".join(lines)) == []
    internal = f"{PREFIX} XX000 ERROR:  could not read segment"
    assert log_problems(internal) == [f"server log: {internal}"]
