"""Tests of packaging/reproducibly.sh, which runs a command with the
RUSTFLAGS and CFLAGS of a reproducible build (CONTRIBUTING.md,
"Reproducible builds"): the source, its vendor/ and CARGO_HOME's crates.io
sources each mapped to a fixed path."""

import os
import subprocess

from packages import PACKAGING

SCRIPT = PACKAGING / "reproducibly.sh"


def environment(source, cargo_home, **extra):
    """RUSTFLAGS and CFLAGS as the command run by the script sees them."""
    env = {key: value for key, value in os.environ.items() if key not in ("RUSTFLAGS", "CFLAGS")}
    done = subprocess.run(["bash", str(SCRIPT), str(source), "python3", "-c",
                           "import os; print(os.environ['RUSTFLAGS']); print(os.environ['CFLAGS'])"],
                          capture_output=True, text=True, check=True,
                          env={**env, "CARGO_HOME": str(cargo_home), **extra})
    rustflags, cflags = done.stdout.splitlines()
    return rustflags.split(), cflags.split()


def test_the_source_its_vendor_directory_and_crates_io_s_sources_map_to_fixed_paths(tmp_path):
    source, home = tmp_path / "pg_warren-1.2.3", tmp_path / "cargo"
    source.mkdir()
    (home / "registry" / "src" / "index.crates.io-1949cf8c6b5b557f").mkdir(parents=True)
    rustflags, _ = environment(source, home)
    assert rustflags == [
        f"--remap-path-prefix={source}=/pg_warren",
        f"--remap-path-prefix={source}/vendor=/crates",
        f"--remap-path-prefix={home}/registry/src/index.crates.io-1949cf8c6b5b557f=/crates",
    ]


def test_c_sources_built_by_build_scripts_map_the_same_way(tmp_path):
    # pgrx-pg-sys compiles a C shim whose __FILE__ names its OUT_DIR.
    source, home = tmp_path / "pg_warren-1.2.3", tmp_path / "cargo"
    source.mkdir()
    (home / "registry" / "src" / "index.crates.io-1949cf8c6b5b557f").mkdir(parents=True)
    _, cflags = environment(source, home)
    assert cflags == [
        f"-ffile-prefix-map={source}=/pg_warren",
        f"-ffile-prefix-map={source}/vendor=/crates",
        f"-ffile-prefix-map={home}/registry/src/index.crates.io-1949cf8c6b5b557f=/crates",
    ]


def test_flags_already_set_are_kept_after_the_maps(tmp_path):
    rustflags, cflags = environment(tmp_path, tmp_path / "no-cargo-home", RUSTFLAGS="-D warnings", CFLAGS="-O2")
    assert rustflags[-2:] == ["-D", "warnings"] and cflags[-1] == "-O2"


def test_the_source_is_named_by_its_physical_path(tmp_path):
    source = tmp_path / "real"
    source.mkdir()
    (tmp_path / "link").symlink_to(source)
    rustflags, _ = environment(tmp_path / "link", tmp_path / "no-cargo-home")
    assert rustflags[0] == f"--remap-path-prefix={source}=/pg_warren"


def test_a_cargo_home_without_downloaded_crates_maps_only_the_source(tmp_path):
    rustflags, cflags = environment(tmp_path, tmp_path / "no-cargo-home")
    assert len(rustflags) == 2 and len(cflags) == 2


def test_the_command_s_exit_status_is_the_script_s(tmp_path):
    assert subprocess.run(["bash", str(SCRIPT), str(tmp_path), "false"]).returncode == 1


def test_a_missing_source_directory_or_command_is_refused(tmp_path):
    for args in ([str(tmp_path / "missing"), "true"], [str(tmp_path)]):
        done = subprocess.run(["bash", str(SCRIPT), *args], capture_output=True, text=True)
        assert done.returncode == 2 and "usage" in done.stderr
