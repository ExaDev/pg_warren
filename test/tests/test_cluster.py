"""A scratch cluster's teardown: it stops the server only if it runs, and
removes the cluster directory whatever happens, without hiding the error
that ended the run."""

import pytest

from harness.cluster import Cluster, major


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
    scratch.upgrade, scratch.backup, scratch.runas, scratch.linked = None, None, None, False
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
    scratch.upgrade = scratch.backup = None
    scratch.running = lambda: False

    def run(program, *arguments):
        raise RuntimeError(f"{program} failed: could not initiate base backup")
    scratch._run = run
    with pytest.raises(RuntimeError, match="initdb failed"):
        scratch.__enter__()
    assert list(tmp_path.iterdir()) == []


def upgrading(tmp_path, old_checksums, new_major, transfer="copy", disabled=None):
    """A recording `Cluster` that upgrades `old`, a recording cluster whose
    data checksums are `old_checksums`, with binaries of major `new_major`,
    by `transfer`. Its pg_upgrade disables the old cluster (renames its
    `global/pg_control` to `pg_control.old`) when it links, unless
    `disabled` says otherwise."""
    old = recording(tmp_path / "old")
    old.bindir, old.checksums, old.stopped, old.linked = tmp_path / "old-bin", lambda: old_checksums, False, False
    old.running = lambda: not old.stopped

    def stop():
        old.stopped = True
    old.stop = stop
    new = recording(tmp_path / "new")
    new.bindir, new.upgrade, new.major, new.transfer = tmp_path / "new-bin", old, lambda: new_major, transfer
    control = old.directory / "data" / "global"
    control.mkdir(parents=True, exist_ok=True)

    def run(program, *arguments):
        new.ran.append((program, *map(str, arguments)))
        if "--check" not in arguments and (transfer == "link" if disabled is None else disabled):
            (control / "pg_control.old").touch()
    new._run = run
    return old, new


def test_an_upgraded_clusters_initdb_keeps_the_old_clusters_checksums(tmp_path):
    for old_checksums, new_major, flags in ((False, 17, []), (True, 17, ["--data-checksums"]),
                                            (False, 18, ["--no-data-checksums"]),
                                            (True, 18, ["--data-checksums"])):
        _old, new = upgrading(tmp_path, old_checksums, new_major)
        new._create(tmp_path / "new" / "data")
        [initdb] = new.ran
        assert initdb[0] == "initdb"
        assert [f for f in initdb if "checksums" in f] == flags, (old_checksums, new_major)


def test_an_upgrade_stops_the_old_cluster_checks_and_then_runs_pg_upgrade(tmp_path):
    old, new = upgrading(tmp_path, False, 18)
    new._upgrade()
    assert old.stopped
    assert [command[0] for command in new.ran] == ["pg_upgrade", "pg_upgrade"]
    check, upgrade = new.ran
    assert "--check" in check and "--check" not in upgrade
    for option, value in (("-b", old.bindir), ("-B", new.bindir), ("-d", old.directory / "data"),
                          ("-D", new.directory / "data"), ("-s", new.directory), ("-U", "postgres")):
        assert (option, str(value)) == upgrade[upgrade.index(option):upgrade.index(option) + 2]
    assert "--link" not in check + upgrade, "copy mode copies the files"


def test_a_link_upgrade_checks_and_runs_pg_upgrade_with_link(tmp_path):
    _old, new = upgrading(tmp_path, False, 18, transfer="link")
    new._upgrade()
    check, upgrade = new.ran
    assert "--check" in check and "--link" in check, "the check tests that the files can be linked"
    assert "--link" in upgrade and "--check" not in upgrade


def test_the_old_cluster_is_never_started_again_after_a_link_upgrade(tmp_path):
    """The new cluster shares the old one's files: starting the old one
    after the new one has started would corrupt both."""
    old, new = upgrading(tmp_path, False, 18, transfer="link")
    new._upgrade()
    assert [command[0] for command in old.ran] == [], "pg_upgrade alone touches the old cluster"
    for again in (old.start, old.restart):
        with pytest.raises(RuntimeError, match="linked"):
            again()
    assert old.ran == []


def test_a_link_upgrade_that_left_the_old_cluster_startable_did_not_link(tmp_path):
    """pg_upgrade --link renames the old cluster's pg_control once it
    links; without that, the files were not linked."""
    _old, new = upgrading(tmp_path, False, 18, transfer="link", disabled=False)
    with pytest.raises(RuntimeError, match="not linked"):
        new._upgrade()


def test_the_old_cluster_can_start_again_after_a_copy_upgrade(tmp_path):
    old, new = upgrading(tmp_path, False, 18)
    new._upgrade()
    old.start()
    assert [command[0] for command in old.ran] == ["pg_ctl"]


def test_a_base_backup_is_taken_from_the_clusters_socket_with_its_wal(tmp_path):
    scratch = recording(tmp_path)
    scratch.base_backup(tmp_path / "backup")
    [command] = scratch.ran
    assert command[0] == "pg_basebackup" and "-R" not in command
    assert ("-D", str(tmp_path / "backup")) == command[command.index("-D"):command.index("-D") + 2]
    assert ("-X", "stream") == command[command.index("-X"):command.index("-X") + 2]
    assert ("-h", str(tmp_path)) == command[command.index("-h"):command.index("-h") + 2]


def test_a_restored_cluster_is_a_copy_of_the_backup_that_recovers_from_the_archive(tmp_path):
    backup = tmp_path / "backup"
    backup.mkdir()
    (backup / "PG_VERSION").write_text("16\n")
    scratch = recording(tmp_path / "restored")
    scratch.backup = backup
    scratch._create(tmp_path / "restored" / "data")
    assert scratch.ran == [], "nothing is run: the backup is copied"
    assert (tmp_path / "restored" / "data" / "PG_VERSION").read_text() == "16\n"
    assert (tmp_path / "restored" / "data" / "recovery.signal").exists()
    assert (tmp_path / "restored" / "data").stat().st_mode & 0o777 == 0o700


def test_settings_configured_later_are_appended_and_so_win(tmp_path):
    scratch = recording(tmp_path)
    (tmp_path / "data").mkdir()
    (tmp_path / "data" / "postgresql.conf").write_text("shared_preload_libraries = 'pg_warren'\n")
    scratch.configure(["shared_preload_libraries = ''", "work_mem = '8MB'"])
    assert (tmp_path / "data" / "postgresql.conf").read_text().splitlines() == [
        "shared_preload_libraries = 'pg_warren'", "shared_preload_libraries = ''", "work_mem = '8MB'"]


def test_the_major_is_read_from_pg_configs_version(monkeypatch):
    for version, expected in (("PostgreSQL 16.10 (Ubuntu 16.10-1.pgdg24.04+1)", 16), ("PostgreSQL 18beta1", 18)):
        monkeypatch.setattr("harness.cluster._pg_config", lambda _pg_config, _option, v=version: v)
        assert major("pg_config") == expected
