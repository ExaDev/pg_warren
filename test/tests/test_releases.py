"""Tests of how the release builds find each release's commit
(harness/releases.py): from sql/released.commits, cross-checked against the
released install script's sha256 and, if present, the release's tag."""

import subprocess

import pytest

from harness.extension import REPOSITORY
from harness.releases import ReleaseError, release_commit

V010 = "9cdf581358a7d0d6bcd8b8d4695a75c4e106558d"
V020 = "b711d515aeb3d201c11bf867c88825bc39e97d2d"


def commits(tmp_path, *lines):
    path = tmp_path / "released.commits"
    path.write_text("".join(f"{line}\n" for line in lines))
    return path


def tag(name):
    found = subprocess.run(["git", "rev-parse", "--verify", "--quiet", f"{name}^{{commit}}"],
                           cwd=REPOSITORY, capture_output=True, text=True)
    return found.stdout.strip() if found.returncode == 0 else None


def test_the_recorded_commits_resolve():
    assert release_commit("0.1.0") == V010
    assert release_commit("0.2.0") == V020


def test_a_release_without_a_recorded_commit_is_refused(tmp_path):
    with pytest.raises(ReleaseError, match=r"0\.2\.0 has no commit in .*released\.commits"):
        release_commit("0.2.0", commits(tmp_path, f"{V010}  0.1.0"))


def test_a_recorded_commit_missing_from_the_clone_is_refused(tmp_path):
    missing = "0" * 40
    with pytest.raises(ReleaseError, match=r"not in this clone.*fetch-depth: 0"):
        release_commit("0.1.0", commits(tmp_path, f"{missing}  0.1.0"))


def test_a_commit_whose_install_script_is_not_the_released_one_is_refused(tmp_path):
    # 0.1.0's commit has no 0.2.0 install script at all.
    with pytest.raises(ReleaseError, match=r"pg_warren--0\.2\.0\.sql .*does not match"):
        release_commit("0.2.0", commits(tmp_path, f"{V010}  0.2.0"))


def test_a_tag_that_names_another_commit_is_refused(tmp_path):
    if tag("v0.1.0") is None:
        pytest.skip("tag v0.1.0 not fetched")
    # 0.2.0's commit still has the frozen 0.1.0 script, so only the tag differs.
    with pytest.raises(ReleaseError, match=r"tag v0\.1\.0 names .*, not"):
        release_commit("0.1.0", commits(tmp_path, f"{V020}  0.1.0"))
