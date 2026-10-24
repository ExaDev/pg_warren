"""Tests of release/publish.py against a small repository with one release,
1.0.0: the frozen install scripts are checked, and the release's tag is made
on its recorded commit once, so that a re-run succeeds and a tag on another
commit fails."""

import hashlib
import subprocess
import sys
from pathlib import Path

import pytest

PUBLISH = Path(__file__).resolve().parents[1] / "publish.py"


def git(repo, *args):
    return subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True, text=True).stdout.strip()


def write(repo, path, text):
    (repo / path).parent.mkdir(parents=True, exist_ok=True)
    (repo / path).write_text(text)


@pytest.fixture
def repo(tmp_path):
    """Release 1.0.0 tagged-to-be at `repo.released`, frozen on top of it."""
    git(tmp_path, "init", "-q", "-b", "main")
    git(tmp_path, "config", "user.name", "Release Test")
    git(tmp_path, "config", "user.email", "release@example.invalid")
    write(tmp_path, "sql/pg_warren--1.0.0.sql", "CREATE SCHEMA warren;\n")
    git(tmp_path, "add", "--all")
    git(tmp_path, "commit", "-q", "-m", "release: 1.0.0")
    released = git(tmp_path, "rev-parse", "HEAD")
    digest = hashlib.sha256(b"CREATE SCHEMA warren;\n").hexdigest()
    write(tmp_path, "sql/released.sha256", f"{digest}  pg_warren--1.0.0.sql\n")
    write(tmp_path, "sql/released.commits", f"{released}  1.0.0\n")
    git(tmp_path, "add", "--all")
    git(tmp_path, "commit", "-q", "-m", "release: freeze 1.0.0, develop 1.1.0")
    return tmp_path


def publish(repo, *args):
    return subprocess.run([sys.executable, str(PUBLISH), *args], cwd=repo, capture_output=True, text=True)


def released(repo):
    return git(repo, "rev-parse", "HEAD~1")


def test_the_latest_release_is_the_last_one_recorded(repo):
    with (repo / "sql" / "released.commits").open("a") as commits:
        commits.write(f"{git(repo, 'rev-parse', 'HEAD')}  1.1.0\n")
    assert publish(repo, "latest").stdout == "1.1.0\n"


def test_unchanged_frozen_scripts_verify(repo):
    done = publish(repo, "verify")
    assert done.returncode == 0, done.stderr


def test_a_changed_frozen_script_fails_verification(repo):
    write(repo, "sql/pg_warren--1.0.0.sql", "CREATE SCHEMA warren; -- changed\n")
    done = publish(repo, "verify")
    assert done.returncode == 1
    assert "sql/pg_warren--1.0.0.sql does not match sql/released.sha256" in done.stderr


def test_the_tag_is_annotated_on_the_recorded_commit(repo):
    done = publish(repo, "tag", "1.0.0")
    assert done.returncode == 0, done.stderr
    assert git(repo, "cat-file", "-t", "v1.0.0") == "tag"
    assert git(repo, "rev-parse", "v1.0.0^{commit}") == released(repo)
    assert f"tagged v1.0.0 at {released(repo)}" in done.stdout


def test_a_tag_already_on_the_recorded_commit_succeeds(repo):
    assert publish(repo, "tag", "1.0.0").returncode == 0
    tag = git(repo, "rev-parse", "v1.0.0")
    again = publish(repo, "tag", "1.0.0")
    assert again.returncode == 0, again.stderr
    assert f"v1.0.0 already names {released(repo)}" in again.stdout
    assert git(repo, "rev-parse", "v1.0.0") == tag


def test_a_tag_on_another_commit_fails(repo):
    git(repo, "tag", "-a", "-m", "elsewhere", "v1.0.0", "HEAD")
    done = publish(repo, "tag", "1.0.0")
    assert done.returncode == 1
    assert f"v1.0.0 names {git(repo, 'rev-parse', 'HEAD')}, not 1.0.0's recorded commit {released(repo)}" \
        in done.stderr


def test_an_unrecorded_version_is_not_tagged(repo):
    done = publish(repo, "tag", "1.1.0")
    assert done.returncode == 1
    assert "1.1.0 has no commit in sql/released.commits" in done.stderr
    assert git(repo, "tag", "--list") == ""


def test_a_recorded_commit_not_merged_is_not_tagged(repo):
    # A release PR squash-merged: the recorded commit is not in main's history.
    recorded = released(repo)
    git(repo, "checkout", "-q", "--orphan", "squashed")
    git(repo, "commit", "-q", "-m", "squashed release")
    done = publish(repo, "tag", "1.0.0")
    assert done.returncode == 1
    assert f"1.0.0's recorded commit {recorded} is not in HEAD's history" in done.stderr


def test_a_recorded_commit_without_the_released_script_is_not_tagged(repo):
    write(repo, "sql/released.commits", f"{git(repo, 'rev-parse', 'HEAD')}  1.0.0\n")
    write(repo, "sql/pg_warren--1.0.0.sql", "CREATE SCHEMA warren; -- changed\n")
    git(repo, "commit", "-q", "-am", "a later commit with another 1.0.0 script")
    write(repo, "sql/released.commits", f"{git(repo, 'rev-parse', 'HEAD')}  1.0.0\n")
    write(repo, "sql/pg_warren--1.0.0.sql", "CREATE SCHEMA warren;\n")
    git(repo, "commit", "-q", "-am", "recorded")
    done = publish(repo, "tag", "1.0.0")
    assert done.returncode == 1
    assert "does not hold the released sql/pg_warren--1.0.0.sql" in done.stderr


def test_a_version_recorded_twice_fails_verification(repo):
    with (repo / "sql" / "released.commits").open("a") as commits:
        commits.write(f"{git(repo, 'rev-parse', 'HEAD')}  1.0.0\n")
    done = publish(repo, "verify")
    assert done.returncode == 1
    assert "1.0.0 is recorded more than once in sql/released.commits" in done.stderr


def test_a_commit_recorded_for_two_versions_fails_verification(repo):
    with (repo / "sql" / "released.commits").open("a") as commits:
        commits.write(f"{released(repo)}  1.1.0\n")
    done = publish(repo, "verify")
    assert done.returncode == 1
    assert f"commit {released(repo)} is recorded for more than one release in sql/released.commits" in done.stderr


def test_the_recorded_commit_is_printed(repo):
    done = publish(repo, "commit", "1.0.0")
    assert done.returncode == 0, done.stderr
    assert done.stdout == f"{released(repo)}\n"
