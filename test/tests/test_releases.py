"""Tests of the release builds' tag resolution (harness/releases.py)."""

import subprocess

import pytest

from harness.extension import REPOSITORY
from harness.releases import MissingTag, release_commit


def test_a_release_without_its_tag_is_refused_with_how_to_get_it():
    with pytest.raises(MissingTag, match=r"no git tag v99\.0\.0 .*git fetch --tags"):
        release_commit("99.0.0")


def test_a_release_resolves_to_the_commit_its_tag_names():
    tags = subprocess.run(["git", "tag", "--list", "v*"], cwd=REPOSITORY, capture_output=True,
                          text=True, check=True).stdout.split()
    if not tags:
        pytest.skip("no release tags fetched")
    tag = tags[0]
    commit = subprocess.run(["git", "rev-list", "-n", "1", tag], cwd=REPOSITORY, capture_output=True,
                            text=True, check=True).stdout.strip()
    assert release_commit(tag[1:]) == commit
