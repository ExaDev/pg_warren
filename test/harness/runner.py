"""Running SQL tests: each in a fresh database with pg_warren, the assert
schema, its fixture (generated once per run by bench/generate.py) and its
oracle tables."""

import hashlib
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path

from . import oracle
from .extension import REPOSITORY

ASSERT_LIBRARY = REPOSITORY / "test" / "lib" / "assert.sql"
GENERATE = REPOSITORY / "bench" / "generate.py"


@dataclass(frozen=True)
class Outcome:
    name: str
    passed: bool
    seconds: float
    output: str


class Fixtures:
    """Generated fixture directories, keyed by their generate.py arguments."""

    def __init__(self, directory):
        self.directory = Path(directory)

    def path(self, arguments):
        key = hashlib.sha256("\0".join(arguments).encode()).hexdigest()[:16]
        path = self.directory / f"{arguments[0]}-{key}"
        if not path.exists():
            subprocess.run([sys.executable, str(GENERATE), *arguments, "--out", str(path)], check=True)
        return path


def run(cluster, test, fixtures):
    started = time.monotonic()
    try:
        database = prepare(cluster, test, fixtures)
        done = database.run_file(test.path, cwd=test.path.parent)
        passed, output = done.returncode == 0, done.stdout
    except (RuntimeError, subprocess.CalledProcessError) as error:
        passed, output = False, f"setup failed: {error}"
    return Outcome(test.name, passed, time.monotonic() - started, output)


def prepare(cluster, test, fixtures):
    name = f"test_{test.name}"
    cluster.database().execute(f'DROP DATABASE IF EXISTS "{name}"')
    cluster.database().execute(f'CREATE DATABASE "{name}"')
    database = cluster.database(name)
    database.execute("CREATE EXTENSION pg_warren")
    database.run_file_checked(ASSERT_LIBRARY, cwd=ASSERT_LIBRARY.parent)
    if test.directives.fixture:
        path = fixtures.path(test.directives.fixture)
        database.run_file_checked(path / "load.sql", cwd=path)
    for request in test.directives.oracles:
        oracle.run(database, request.computation, list(request.arguments))
    return database
