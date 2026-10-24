"""Tests of release/prepare.py against a clone of this repository put back
to 0.5.0 in development: CHANGELOG's 0.5.0 section undated and README's
install at v0.4.0, as before release 0.5.0's two commits. Preparing 0.5.0
must reproduce them: the commit to tag, 5f4176f's tree, and the freeze
commit, 8d779aa's changes but README's Status (prose a person writes). The
schema drift check, which needs cargo-pgrx, is a stand-in that passes and
on `--update` writes the install script 8d779aa committed for 0.6.0."""

import os
import subprocess
import sys
from pathlib import Path

import pytest

REPOSITORY = Path(__file__).resolve().parents[2]
PREPARE = REPOSITORY / "release" / "prepare.py"
TAGGED_0_5_0 = "5f4176f4e4c8980acf751a342d8622fdb93838c0"
FROZE_0_5_0 = "8d779aa74d223e3f0036082c42bb8bff3007af8a"

DRIFT = """#!/usr/bin/env bash
set -euo pipefail
[[ -z "${FAKE_DRIFT:-}" ]] || { echo "schema drift" >&2; exit 1; }
if [[ "${1:-}" == "--update" ]]; then
    [[ -z "${FAKE_UPDATE_FAILS:-}" ]] || { echo "cargo pgrx schema failed" >&2; exit 1; }
    version="$(sed -n 's/^version = "\\(.*\\)"$/\\1/p' Cargo.toml | head -n 1)"
    cp "$FAKE_GENERATED" "sql/pg_warren--$version.sql"
fi
"""


def git(repo, *args):
    return subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True, text=True).stdout


def show(repo, revision, path):
    return subprocess.run(["git", "show", f"{revision}:{path}"], cwd=repo, check=True,
                          capture_output=True).stdout


@pytest.fixture(scope="module")
def base(tmp_path_factory):
    """A clone at 0.5.0 in development, with the stand-in drift check."""
    root = tmp_path_factory.mktemp("release")
    clone = root / "base"
    subprocess.run(["git", "clone", "-q", "--no-checkout", "--shared", str(REPOSITORY), str(clone)], check=True)
    git(clone, "checkout", "-q", "-B", "release-0.5.0", TAGGED_0_5_0)
    git(clone, "config", "user.name", "Release Test")
    git(clone, "config", "user.email", "release@example.invalid")
    changelog = clone / "CHANGELOG.md"
    changelog.write_text(changelog.read_text().replace(
        "## 0.5.0 — the shared cache (2026-10-23)", "## 0.5.0 — in development", 1))
    readme = clone / "README.md"
    readme.write_text(readme.read_text().replace("git checkout v0.5.0", "git checkout v0.4.0", 1))
    git(clone, "commit", "-q", "-am", "fixture: 0.5.0 in development")
    drift = root / "schema-drift.sh"
    drift.write_text(DRIFT)
    drift.chmod(0o755)
    generated = root / "generated.sql"
    generated.write_bytes(show(REPOSITORY, FROZE_0_5_0, "sql/pg_warren--0.6.0.sql"))
    return clone, drift, generated


@pytest.fixture
def repo(base, tmp_path, monkeypatch):
    clone, drift, generated = base
    copy = tmp_path / "repo"
    subprocess.run(["git", "clone", "-q", "--shared", "--branch", "release-0.5.0", str(clone), str(copy)], check=True)
    git(copy, "config", "user.name", "Release Test")
    git(copy, "config", "user.email", "release@example.invalid")
    monkeypatch.setenv("FAKE_GENERATED", str(generated))
    monkeypatch.setenv("FAKE_DRIFT_SCRIPT", str(drift))
    return copy


def prepare(repo, *args):
    return subprocess.run([sys.executable, str(PREPARE), *args, "--schema-drift", os.environ["FAKE_DRIFT_SCRIPT"]],
                          cwd=repo, capture_output=True, text=True)


def prepare_0_5_0(repo, *extra):
    return prepare(repo, "0.5.0", "0.6.0", "--date", "2026-10-23", "--title", "the shared cache", *extra)


def changed(repo, start, end):
    return set(git(repo, "diff", "--name-only", start, end).split())


def subject(repo, revision="HEAD"):
    return git(repo, "log", "-1", "--format=%s", revision).strip()


def test_preparing_0_5_0_reproduces_its_release_commits(repo):
    start = git(repo, "rev-parse", "HEAD").strip()
    done = prepare_0_5_0(repo)
    assert done.returncode == 0, done.stderr
    tagged = git(repo, "rev-parse", "HEAD~1").strip()
    assert git(repo, "rev-parse", "HEAD~2").strip() == start
    assert subject(repo, "HEAD~1") == "release: 0.5.0"
    assert subject(repo) == "release: freeze 0.5.0, develop 0.6.0"
    # The commit to tag is 5f4176f's tree.
    assert git(repo, "diff", "--stat", TAGGED_0_5_0, tagged) == ""
    # The freeze commit is 8d779aa's change, but README's Status.
    assert changed(repo, tagged, "HEAD") == changed(repo, TAGGED_0_5_0, FROZE_0_5_0) - {"README.md"}
    for path in changed(repo, tagged, "HEAD"):
        expected = show(repo, FROZE_0_5_0, path)
        if path == "sql/released.commits":
            expected = expected.replace(TAGGED_0_5_0.encode(), tagged.encode())
        assert show(repo, "HEAD", path) == expected, path
    assert git(repo, "status", "--porcelain") == ""


def test_preparing_again_changes_nothing(repo):
    assert prepare_0_5_0(repo).returncode == 0
    head = git(repo, "rev-parse", "HEAD")
    again = prepare_0_5_0(repo)
    assert again.returncode == 0, again.stderr
    assert "already prepared" in again.stdout
    assert git(repo, "rev-parse", "HEAD") == head


def test_a_failed_freeze_leaves_the_commit_to_tag_and_resumes(repo, monkeypatch):
    monkeypatch.setenv("FAKE_UPDATE_FAILS", "1")
    failed = prepare_0_5_0(repo)
    assert failed.returncode != 0
    assert "cargo pgrx schema failed" in failed.stderr
    assert subject(repo) == "release: 0.5.0"
    assert git(repo, "status", "--porcelain") == ""
    monkeypatch.delenv("FAKE_UPDATE_FAILS")
    resumed = prepare_0_5_0(repo)
    assert resumed.returncode == 0, resumed.stderr
    assert subject(repo, "HEAD~1") == "release: 0.5.0"
    assert subject(repo, "HEAD~2") == "fixture: 0.5.0 in development"
    assert show(repo, "HEAD", "sql/released.sha256") == show(repo, FROZE_0_5_0, "sql/released.sha256")


def test_the_next_version_is_released_from_the_section_it_opened(repo):
    assert prepare_0_5_0(repo).returncode == 0
    nxt = prepare(repo, "0.6.0", "0.7.0", "--date", "2026-10-24")
    assert nxt.returncode == 0, nxt.stderr
    changelog = show(repo, "HEAD", "CHANGELOG.md").decode()
    assert changelog.startswith("# Changelog\n\n## 0.7.0 — in development\n\n## 0.6.0 (2026-10-24)\n\n"
                                "## 0.5.0 — the shared cache (2026-10-23)\n")
    assert "git checkout v0.6.0   # the latest release" in show(repo, "HEAD", "README.md").decode()
    header = show(repo, "HEAD", "sql/pg_warren--0.6.0--0.7.0.sql").decode()
    assert header.startswith("-- pg_warren 0.6.0 -> 0.7.0 (DESIGN.md §13;")
    assert header.endswith("""\\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.7.0'" to load this file. \\quit\n""")
    assert show(repo, "HEAD", "sql/released.commits").decode().endswith(
        f"{git(repo, 'rev-parse', 'HEAD~1').strip()}  0.6.0\n")


def refused(repo, result, message):
    head = git(repo, "rev-parse", "HEAD")
    assert result.returncode != 0
    assert message in result.stderr
    assert git(repo, "rev-parse", "HEAD") == head


def test_a_dirty_tree_is_refused(repo):
    (repo / "stray.txt").write_text("not committed\n")
    refused(repo, prepare_0_5_0(repo), "the working tree is not clean")


def test_a_released_version_is_refused(repo):
    refused(repo, prepare(repo, "0.4.0", "0.5.0"), "0.4.0 is already released")


def test_a_tagged_version_is_refused(repo):
    git(repo, "tag", "v0.5.0")
    refused(repo, prepare_0_5_0(repo), "tag v0.5.0 already exists")


def test_a_version_not_in_development_is_refused(repo):
    refused(repo, prepare(repo, "0.7.0", "0.8.0"), "the version in development is 0.5.0, not 0.7.0")


def test_a_next_version_not_after_the_release_is_refused(repo):
    refused(repo, prepare(repo, "0.5.0", "0.5.0"), "the next version, 0.5.0, must come after 0.5.0")


def test_a_malformed_version_is_refused(repo):
    refused(repo, prepare(repo, "0.5", "0.6.0"), "0.5 is not a version X.Y.Z")


def test_schema_drift_is_refused_before_anything_is_committed(repo, monkeypatch):
    monkeypatch.setenv("FAKE_DRIFT", "1")
    refused(repo, prepare_0_5_0(repo), "schema drift")
    assert git(repo, "status", "--porcelain") == ""
