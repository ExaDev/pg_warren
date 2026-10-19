"""A database reached through psql: run SQL, scripts, COPY in and out.

Using the psql client (not a Python driver) keeps the harness dependency-free
and lets fixtures and tests use psql features (`\\copy`, `\\gset`, `\\ir`)."""

import csv
import io
import subprocess
from dataclasses import dataclass


@dataclass(frozen=True)
class Database:
    psql: str
    connection: tuple[str, ...] = ()  # psql connection options; empty means libpq's PG* environment

    def command(self, *arguments):
        return [self.psql, "-X", "-q", "-v", "ON_ERROR_STOP=1", *self.connection, *arguments]

    def execute(self, sql):
        self._check(subprocess.run(self.command("-c", sql), capture_output=True, text=True))

    def query(self, sql):
        """The rows of `sql` as lists of strings."""
        done = subprocess.run(self.command("-c", f"COPY ({sql}) TO STDOUT WITH (FORMAT csv)"),
                              capture_output=True, text=True)
        self._check(done)
        return list(csv.reader(io.StringIO(done.stdout)))

    def copy_in(self, table, rows):
        data = io.StringIO()
        csv.writer(data, lineterminator="\n").writerows(rows)
        self._check(subprocess.run(self.command("-c", f"\\copy {table} FROM pstdin WITH (FORMAT csv)"),
                                   input=data.getvalue(), capture_output=True, text=True))

    def run_file(self, path, cwd):
        """Runs psql script `path` with working directory `cwd`; returns the
        completed process, stderr merged into stdout, without checking it."""
        return subprocess.run(self.command("-f", str(path)), cwd=cwd, stdout=subprocess.PIPE,
                              stderr=subprocess.STDOUT, text=True)

    def run_file_checked(self, path, cwd):
        self._check(self.run_file(path, cwd))

    @staticmethod
    def _check(done):
        if done.returncode != 0:
            raise RuntimeError(f"psql failed ({done.returncode}): {done.stdout}{done.stderr or ''}")
