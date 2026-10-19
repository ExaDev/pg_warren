"""A SQL test file and its directives.

A test is a `.sql` file run by psql in a fresh database with `pg_warren` and
the `assert` schema (`test/lib/assert.sql`) installed. It passes when psql
exits 0; with `ON_ERROR_STOP` any error, including a failed assertion, fails
it. Directives in the leading comment block prepare the database first:

    -- fixture: <bench/generate.py arguments, without --out>
    -- oracle: <computation> [--nodes-sql SQL] [--edges-sql SQL] [--directed] [...]

At most one fixture per test. Each oracle line writes a NetworkX reference
table (see `harness.oracle`)."""

import shlex
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class OracleRequest:
    computation: str
    arguments: tuple[str, ...]


@dataclass(frozen=True)
class Directives:
    fixture: tuple[str, ...] | None
    oracles: tuple[OracleRequest, ...]

    @classmethod
    def parse(cls, text):
        fixture, oracles = None, []
        for line in text.splitlines():
            if not line.startswith("--"):
                break
            key, _, value = line[2:].strip().partition(":")
            if key == "fixture":
                if fixture is not None:
                    raise ValueError("a test takes at most one fixture")
                fixture = tuple(shlex.split(value))
            elif key == "oracle":
                computation, *arguments = shlex.split(value)
                oracles.append(OracleRequest(computation, tuple(arguments)))
        return cls(fixture, tuple(oracles))


@dataclass(frozen=True)
class SqlTest:
    path: Path
    directives: Directives

    @classmethod
    def load(cls, path):
        path = Path(path).resolve()
        return cls(path, Directives.parse(path.read_text()))

    @property
    def name(self):
        return self.path.stem
