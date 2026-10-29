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
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from .database import Database

PORT = "5432"
SUPERUSER = "postgres"


class Cluster:
    def __init__(self, pg_config, runas="postgres", keep=False, parent=None, settings=(), primary=None,
                 upgrade=None, backup=None):
        self.pg_config = pg_config
        self.bindir = Path(_pg_config(pg_config, "--bindir"))
        self.runas = runas if os.geteuid() == 0 else None
        self.keep = keep
        self.parent = parent
        self.settings = tuple(settings)
        self.primary = primary
        self.upgrade = upgrade
        self.backup = backup
        self.directory = None

    def __enter__(self):
        self.directory = Path(tempfile.mkdtemp(prefix="warren-cluster-", dir=self.parent))
        try:
            self._make()
        except BaseException:
            # `with` calls __exit__ only once __enter__ has returned.
            self.__exit__(*sys.exc_info())
            raise
        return self

    def _make(self):
        """Makes the cluster in `directory`, configures and starts it."""
        if self.runas:
            user = pwd.getpwnam(self.runas)
            os.chown(self.directory, user.pw_uid, user.pw_gid)
        data = self.directory / "data"
        self._create(data)
        self.configure(["listen_addresses = ''", f"unix_socket_directories = '{self.directory}'", f"port = {PORT}",
                        "fsync = off", "synchronous_commit = off", "full_page_writes = off", *self.settings])
        if self.upgrade is not None:
            self._upgrade()
        self.start()

    def _create(self, data):
        """Makes the data directory `data`: a new cluster, a base backup of
        `primary`, a new cluster for `upgrade`, or a copy of `backup`."""
        if self.backup is not None:
            self._restore(data)
        elif self.primary is None:
            self._run("initdb", "-D", data, "-U", SUPERUSER, "-A", "trust", "-E", "UTF8", "--locale=C", "--no-sync",
                      *self._checksums())
        else:
            self._run("pg_basebackup", "-D", data, "-R", "-X", "stream", "-c", "fast", "--no-sync",
                      "-h", self.primary.directory, "-p", PORT, "-U", SUPERUSER)

    def _checksums(self):
        """initdb's checksum option for a cluster that upgrades `upgrade`:
        the old cluster's (pg_upgrade refuses a difference; initdb enables
        them by default from PG 18)."""
        if self.upgrade is None:
            return ()
        if self.upgrade.checksums():
            return ("--data-checksums",)
        return ("--no-data-checksums",) if self.major() >= 18 else ()

    def _upgrade(self):
        """Stops `upgrade` and moves its databases into this cluster's
        (stopped) data directory with pg_upgrade, after its `--check`."""
        if self.upgrade.running():
            self.upgrade.stop()
        arguments = ("-b", self.upgrade.bindir, "-B", self.bindir, "-d", self.upgrade.directory / "data",
                     "-D", self.directory / "data", "-s", self.directory, "-U", SUPERUSER)
        self._run("pg_upgrade", "--check", *arguments)
        self._run("pg_upgrade", *arguments)

    def _restore(self, data):
        """Copies `backup` into `data`, owned by the cluster's user, to start
        in targeted recovery."""
        shutil.copytree(self.backup, data)
        (data / "recovery.signal").touch()
        data.chmod(0o700)
        if self.runas:
            user = pwd.getpwnam(self.runas)
            for path in (data, *data.rglob("*")):
                os.chown(path, user.pw_uid, user.pw_gid, follow_symlinks=False)

    def base_backup(self, destination):
        """Takes a base backup of the running cluster into `destination`
        (inside its directory), with the WAL that makes it consistent."""
        self._run("pg_basebackup", "-D", destination, "-X", "stream", "-c", "fast", "--no-sync",
                  "-h", self.directory, "-p", PORT, "-U", SUPERUSER)

    def checksums(self):
        """Whether the cluster has data checksums (pg_controldata)."""
        done = subprocess.run([str(self.bindir / "pg_controldata"), str(self.directory / "data")],
                              capture_output=True, text=True, check=True)
        return re.search(r"^Data page checksum version:\s+0$", done.stdout, re.MULTILINE) is None

    def major(self):
        """The Postgres major version of the cluster's binaries."""
        return major(self.pg_config)

    @property
    def log(self):
        """The server log."""
        return self.directory / "server.log"

    def start(self):
        self._run("pg_ctl", "-D", self.directory / "data", "-l", self.log, "-w", "start")

    def restart(self):
        """Restarts the server, for a setting that needs it or a clean shared memory."""
        self._run("pg_ctl", "-D", self.directory / "data", "-l", self.log, "-m", "fast", "-w", "restart")

    def configure(self, settings):
        """Appends `settings` to postgresql.conf, where they override the
        earlier lines; a restart (or reload) applies them."""
        with open(self.directory / "data" / "postgresql.conf", "a") as conf:
            conf.writelines(f"{line}\n" for line in settings)

    def stop(self):
        """Stops the server cleanly (`pg_ctl stop -m fast`)."""
        self._run("pg_ctl", "-D", self.directory / "data", "-m", "fast", "-w", "stop")

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
                self.stop()
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


def major(pg_config):
    """The Postgres major version of `pg_config`'s binaries."""
    return int(re.match(r"\d+", _pg_config(pg_config, "--version").split()[1]).group())


def _pg_config(pg_config, option):
    return subprocess.run([pg_config, option], check=True, capture_output=True, text=True).stdout.strip()
