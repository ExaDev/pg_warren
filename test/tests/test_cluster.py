"""A scratch cluster's teardown: it stops the server only if it runs, and
removes the cluster directory whatever happens, without hiding the error
that ended the run."""

import pytest

from harness.cluster import Cluster


def cluster(tmp_path, running, stop_fails=False):
    """A `Cluster` in `tmp_path`, whose server runs or not, and whose stop fails or not."""
    scratch = Cluster.__new__(Cluster)
    scratch.directory, scratch.keep, scratch.stopped = tmp_path / "cluster", False, False
    scratch.directory.mkdir()
    scratch.running = lambda: running

    def run(program, *arguments):
        if stop_fails:
            raise RuntimeError(f"{program} failed")
        scratch.stopped = True
    scratch._run = run
    return scratch


def test_a_running_server_is_stopped_and_its_directory_removed(tmp_path):
    scratch = cluster(tmp_path, running=True)
    scratch.__exit__(None, None, None)
    assert scratch.stopped and not scratch.directory.exists()


def test_a_server_that_is_down_is_not_stopped_and_its_directory_is_still_removed(tmp_path):
    scratch = cluster(tmp_path, running=False, stop_fails=True)
    scratch.__exit__(None, None, None)
    assert not scratch.directory.exists()


def test_a_failed_stop_does_not_hide_the_error_that_ended_the_run(tmp_path):
    scratch = cluster(tmp_path, running=True, stop_fails=True)
    error = ValueError("the run's own error")
    assert not scratch.__exit__(ValueError, error, None), "the run's error propagates"
    assert not scratch.directory.exists()


def test_a_failed_stop_after_a_clean_run_is_raised(tmp_path):
    scratch = cluster(tmp_path, running=True, stop_fails=True)
    with pytest.raises(RuntimeError, match="pg_ctl failed"):
        scratch.__exit__(None, None, None)
    assert not scratch.directory.exists()


def recording(tmp_path, primary=None):
    """A `Cluster` in `tmp_path` that records the programs it runs instead of running them."""
    scratch = Cluster.__new__(Cluster)
    scratch.directory, scratch.primary, scratch.ran = tmp_path, primary, []
    scratch._run = lambda program, *arguments: scratch.ran.append((program, *map(str, arguments)))
    return scratch


def test_a_new_cluster_is_initialised(tmp_path):
    scratch = recording(tmp_path)
    scratch._create(tmp_path / "data")
    assert [command[0] for command in scratch.ran] == ["initdb"]


def test_a_standby_is_a_base_backup_of_its_primary_that_streams_from_it(tmp_path):
    primary = recording(tmp_path / "primary")
    standby = recording(tmp_path / "standby", primary=primary)
    standby._create(tmp_path / "standby" / "data")
    [command] = standby.ran
    assert command[0] == "pg_basebackup"
    assert ("-D", str(tmp_path / "standby" / "data")) == command[command.index("-D"):command.index("-D") + 2]
    assert "-R" in command, "writes standby.signal and primary_conninfo"
    assert ("-h", str(tmp_path / "primary")) == command[command.index("-h"):command.index("-h") + 2]


def test_promote_ends_a_standbys_recovery_and_waits_for_it(tmp_path):
    standby = recording(tmp_path, primary=recording(tmp_path))
    standby.promote()
    assert standby.ran == [("pg_ctl", "-D", str(tmp_path / "data"), "-w", "promote")]


def test_a_cluster_that_fails_to_start_is_removed_and_the_failure_raised(tmp_path):
    scratch = Cluster.__new__(Cluster)
    scratch.parent, scratch.runas, scratch.keep, scratch.primary = tmp_path, None, False, None
    scratch.running = lambda: False

    def run(program, *arguments):
        raise RuntimeError(f"{program} failed: could not initiate base backup")
    scratch._run = run
    with pytest.raises(RuntimeError, match="initdb failed"):
        scratch.__enter__()
    assert list(tmp_path.iterdir()) == []
