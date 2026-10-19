"""Installing pg_warren into the Postgres tree of a `pg_config`, under the
machine-wide lock that every `cargo pgrx test`/`install` on a shared machine
takes (CONTRIBUTING.md): installs overwrite the one shared `pg_warren.so`."""

import fcntl
import subprocess
from contextlib import contextmanager
from pathlib import Path

LOCK = Path("/tmp/pgrx-test.lock")
REPOSITORY = Path(__file__).resolve().parents[2]


@contextmanager
def shared_lock():
    """Holds `/tmp/pgrx-test.lock` (flock(2), as `flock(1)` takes it)."""
    try:
        handle = open(LOCK, "a")
    except PermissionError:
        handle = open(LOCK)
    with handle:
        fcntl.flock(handle, fcntl.LOCK_EX)
        yield


def install(pg_config, release=False):
    command = ["cargo", "pgrx", "install", "--pg-config", pg_config, *(["--release"] if release else [])]
    subprocess.run(command, cwd=REPOSITORY / "crates" / "pg_warren", check=True)
