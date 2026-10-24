"""Tests of release/homebrew.py, which renders the tap's formula
(packaging/homebrew/pg_warren.rb.in) for a release: its source tarball's URL
and sha256, and the cargo-pgrx the source pins."""

import shutil
import subprocess

import pytest

from homebrew import HomebrewError, render

SHA = "a" * 64


def test_the_formula_names_the_releases_source_tarball_and_its_sha256():
    formula = render("0.7.0", SHA, "0.19.3")
    assert 'url "https://github.com/ExaDev/pg_warren/releases/download/v0.7.0/pg_warren-0.7.0.tar.gz"' in formula
    assert f'sha256 "{SHA}"' in formula


def test_the_formula_builds_with_the_cargo_pgrx_the_source_pins():
    assert '"cargo-pgrx", "--version", "0.19.3", "--locked"' in render("0.7.0", SHA, "0.19.3")


def test_every_placeholder_is_filled():
    assert "@" not in render("0.7.0", SHA, "0.19.3").replace("postgresql@", "")


def test_the_formula_builds_for_homebrews_postgresql_and_says_to_preload():
    formula = render("0.7.0", SHA, "0.19.3")
    assert 'depends_on "postgresql@17" => [:build, :test]' in formula
    assert 'depends_on "postgresql@18" => [:build, :test]' in formula
    assert "shared_preload_libraries" in formula and "CREATE EXTENSION pg_warren" in formula


@pytest.mark.parametrize("version, sha, pgrx", [
    ("0.7", SHA, "0.19.3"),
    ("0.7.0", "abc", "0.19.3"),
    ("0.7.0", SHA, "0.19"),
    ('0.7.0"; system "x', SHA, "0.19.3"),
])
def test_malformed_values_are_refused(version, sha, pgrx):
    with pytest.raises(HomebrewError):
        render(version, sha, pgrx)


@pytest.mark.skipif(shutil.which("ruby") is None, reason="needs ruby")
def test_the_rendered_formula_is_ruby(tmp_path):
    path = tmp_path / "pg_warren.rb"
    path.write_text(render("0.7.0", SHA, "0.19.3"))
    done = subprocess.run(["ruby", "-c", str(path)], capture_output=True, text=True)
    assert done.returncode == 0, done.stderr
