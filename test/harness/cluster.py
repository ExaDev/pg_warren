"""A scratch Postgres cluster in a temporary directory, run with the binaries
of a given `pg_config`. It listens only on a Unix socket in its own
directory, so it never collides with another cluster's port.

Given a running `primary`, the cluster is instead its physical streaming
hot standby: a base backup of it (`pg_basebackup -R`, which writes
`standby.signal` and the `primary_conninfo` that streams from the
primary's socket), with this cluster's own socket and `settings`. A
standby's `primary` may itself be a standby (cascading replication).

initdb and postgres refuse to run as root; as root the cluster runs as
`runas` (default `postgres`) through sudo, as in CONTRIBUTING.md. `parent`
puts the cluster directory somewhere other than the system temporary
directory, and `settings` adds postgresql.conf lines."""

import os
import pwd
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from .database import Database

PORT = "5432"
SUPERUSER = "postgres"


class Cluster:
    def __init__(self, pg_config, runas="postgres", keep=False, parent=None, settings=(), primary=None):
        self.bindir = Path(_pg_config(pg_config, "--bindir"))
        self.runas = runas if os.geteuid() == 0 else None
        self.keep = keep
        self.parent = parent
        self.settings = tuple(settings)
        self.primary = primary
        self.directory = None

    def __enter__(self):
        self.directory = Path(tempfile.mkdtemp(prefix="warren-cluster-", dir=self.parent))
        if self.runas:
            user = pwd.getpwnam(self.runas)
            os.chown(self.directory, user.pw_uid, user.pw_gid)
        data = self.directory / "data"
        self._create(data)
        with open(data / "postgresql.conf", "a") as conf:
            conf.write(f"listen_addresses = ''\nunix_socket_directories = '{self.directory}'\nport = {PORT}\n"
                       "fsync = off\nsynchronous_commit = off\nfull_page_writes = off\n")
            conf.writelines(f"{line}\n" for line in self.settings)
        self.start()
        return self

    def _create(self, data):
        """Makes the data directory `data`: a new cluster, or a base backup of `primary`."""
        if self.primary is None:
            self._run("initdb", "-D", data, "-U", SUPERUSER, "-A", "trust", "-E", "UTF8", "--locale=C", "--no-sync")
        else:
            self._run("pg_basebackup", "-D", data, "-R", "-X", "stream", "-c", "fast", "--no-sync",
                      "-h", self.primary.directory, "-p", PORT, "-U", SUPERUSER)

    @property
    def log(self):
        """The server log."""
        return self.directory / "server.log"

    def start(self):
        self._run("pg_ctl", "-D", self.directory / "data", "-l", self.log, "-w", "start")

    def restart(self):
        """Restarts the server, for a setting that needs it or a clean shared memory."""
        self._run("pg_ctl", "-D", self.directory / "data", "-l", self.log, "-m", "fast", "-w", "restart")

    def promote(self):
        """Ends a standby's recovery (`pg_ctl promote`) and waits until it accepts writes."""
        self._run("pg_ctl", "-D", self.directory / "data", "-w", "promote")

    def running(self):
        """Whether the server is running (`pg_ctl status`)."""
        command = [str(self.bindir / "pg_ctl"), "-D", str(self.directory / "data"), "status"]
        if self.runas:
            command = ["sudo", "-u", self.runas, "--", *command]
        return subprocess.run(command, cwd=self.directory, capture_output=True).returncode == 0

    def stop_immediately(self):
        """Stops the server as a crash would (`pg_ctl stop -m immediate`): the
        next `start` recovers from the WAL."""
        self._run("pg_ctl", "-D", self.directory / "data", "-m", "immediate", "-w", "stop")

    def __exit__(self, kind, _error, _traceback):
        """Stops the server if it runs, and removes the cluster directory
        (unless kept) whatever happens. A failed stop is raised only if
        nothing else ended the run, so it never hides that error."""
        try:
            if self.running():
                self._run("pg_ctl", "-D", self.directory / "data", "-m", "fast", "-w", "stop")
        except RuntimeError as stop:
            if kind is None:
                raise
            print(f"while handling the run's error, {stop}", file=sys.stderr)
        finally:
            if self.keep:
                print(f"kept cluster directory {self.directory}")
            else:
                shutil.rmtree(self.directory, ignore_errors=True)

    def database(self, name="postgres", user=SUPERUSER):
        return Database(str(self.bindir / "psql"), self.connection(name, user))

    def connection(self, name, user=SUPERUSER):
        """libpq client options (psql, pg_dump, pg_restore) for database `name` as `user`."""
        return ("-h", str(self.directory), "-p", PORT, "-U", user, "-d", name)

    def client(self, program):
        """The path of client program `program` (pg_dump, pg_restore) of this cluster's Postgres."""
        return str(self.bindir / program)

    def dump_schema(self, name):
        """The plain schema-only dump of database `name`, by the superuser."""
        done = subprocess.run([self.client("pg_dump"), *self.connection(name), "--schema-only"],
                              capture_output=True, text=True)
        if done.returncode != 0:
            raise RuntimeError(f"pg_dump failed: {done.stdout}{done.stderr}")
        return done.stdout

    def _run(self, program, *arguments):
        command = [str(self.bindir / program), *map(str, arguments)]
        if self.runas:
            command = ["sudo", "-u", self.runas, "--", *command]
        done = subprocess.run(command, cwd=self.directory, capture_output=True, text=True)
        if done.returncode != 0:
            raise RuntimeError(f"{program} failed: {done.stdout}{done.stderr}")


def _pg_config(pg_config, option):
    return subprocess.run([pg_config, option], check=True, capture_output=True, text=True).stdout.strip()
