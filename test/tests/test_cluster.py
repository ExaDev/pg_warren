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
