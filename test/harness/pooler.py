"""PgBouncer in front of a scratch cluster (DESIGN.md §13 M11, "a soak
behind a connection pooler"): one pooler per pool mode, `transaction` or
`session`, listening only on a Unix socket in its own directory beside
the cluster's, for one database and the users given (trust
authentication, as the scratch cluster's). Its server connections are
never retired for age or idleness, so a server backend serves many
clients for the whole soak unless a client breaks it (a disconnect in a
transaction closes that server connection). Session mode resets a server
connection with `DISCARD ALL` when its client leaves; transaction mode
does not reset between transactions.

A `harness.session.Session` connects to a pooler as to a cluster: it has
`client` and `connection`. `show` reads the admin console (`SHOW POOLS`,
`SHOW STATS`). As root, pgbouncer runs as the cluster's user."""

import shutil
import subprocess
import sys
import time
from pathlib import Path

from .cluster import PORT, SUPERUSER

MODES = ("transaction", "session")
PGBOUNCER = "/usr/sbin/pgbouncer"
LIFETIME_SECONDS = 365 * 24 * 3600
START_SECONDS = 10


def config(mode, directory, server_directory, server_port, database, port, pool_size, max_clients, admin):
    """pgbouncer.ini for `mode`, in `directory`, pooling `database` of the
    cluster on socket `server_directory`:`server_port`."""
    return (f"[databases]\n{database} = host={server_directory} port={server_port} dbname={database}\n"
            f"[pgbouncer]\nlisten_addr = \nlisten_port = {port}\nunix_socket_dir = {directory}\n"
            f"auth_type = trust\nauth_file = {directory}/users.txt\npool_mode = {mode}\n"
            f"default_pool_size = {pool_size}\nmin_pool_size = 0\nreserve_pool_size = 0\n"
            f"max_client_conn = {max_clients}\nserver_reset_query = DISCARD ALL\n"
            f"server_lifetime = {LIFETIME_SECONDS}\nserver_idle_timeout = 0\n"
            f"logfile = {directory}/pgbouncer.log\npidfile = {directory}/pgbouncer.pid\n"
            f"admin_users = {admin}\n")


def users_file(users):
    """The auth_file: every user, without a password (trust)."""
    return "".join(f'"{user}" ""\n' for user in users)


def parse_show(text):
    """The rows of an admin console `SHOW`, as psql prints them unaligned
    with tabs and a header, as dicts by column."""
    lines = text.splitlines()
    if not lines:
        return []
    header = lines[0].split("\t")
    return [dict(zip(header, line.split("\t"))) for line in lines[1:] if line]


class Pooler:
    def __init__(self, cluster, mode, database, users, pool_size, max_clients, port, pgbouncer=PGBOUNCER):
        self.cluster, self.mode, self.database = cluster, mode, database
        self.users = [SUPERUSER, *users]
        self.pool_size, self.max_clients, self.port = pool_size, max_clients, port
        self.pgbouncer = pgbouncer
        self.directory = Path(cluster.directory) / f"pooler-{mode}-{port}"

    def __enter__(self):
        self.directory.mkdir()
        (self.directory / "users.txt").write_text(users_file(self.users))
        (self.directory / "pgbouncer.ini").write_text(
            config(self.mode, self.directory, self.cluster.directory, PORT, self.database,
                   self.port, self.pool_size, self.max_clients, SUPERUSER))
        if self.cluster.runas:
            for path in (self.directory, *self.directory.iterdir()):
                shutil.chown(path, self.cluster.runas)
        done = subprocess.run(self._as_runas([self.pgbouncer, "-d", str(self.directory / "pgbouncer.ini")]),
                              capture_output=True, text=True)
        if done.returncode != 0:
            raise RuntimeError(f"pgbouncer failed: {done.stdout}{done.stderr}")
        deadline = time.monotonic() + START_SECONDS
        while not (self.directory / f".s.PGSQL.{self.port}").exists():
            if time.monotonic() > deadline:
                log = self.log.read_text()[-2000:] if self.log.exists() else ""
                self.__exit__()  # it daemonized: stop it before giving up on it
                raise RuntimeError(f"pgbouncer did not start: {log}")
            time.sleep(0.1)
        return self

    def __exit__(self, *_):
        """Stops pgbouncer and waits until it has: SIGTERM first, then, as
        from PgBouncer 1.23 SIGTERM waits for its clients to leave, SIGQUIT
        (an immediate shutdown)."""
        pid_file = self.directory / "pgbouncer.pid"
        for signal in ("TERM", "QUIT"):
            if not pid_file.exists():
                return
            subprocess.run(self._as_runas(["kill", f"-{signal}", pid_file.read_text().strip()]),
                           capture_output=True)
            deadline = time.monotonic() + START_SECONDS
            while pid_file.exists() and time.monotonic() < deadline:
                time.sleep(0.1)
        if pid_file.exists():
            print(f"warning: pgbouncer in {self.directory} did not stop", file=sys.stderr, flush=True)

    @property
    def log(self):
        return self.directory / "pgbouncer.log"

    def client(self, program):
        return self.cluster.client(program)

    def connection(self, name, user=SUPERUSER):
        """libpq client options for database `name` as `user`, through the pooler."""
        return ("-h", str(self.directory), "-p", str(self.port), "-U", user, "-d", name)

    def show(self, what):
        """The admin console's `SHOW <what>` (POOLS, STATS, …), as rows."""
        done = subprocess.run([self.client("psql"), "-X", "-A", "-F", "\t", "-P", "footer=off",
                               *self.connection("pgbouncer"), "-c", f"SHOW {what}"], capture_output=True, text=True)
        if done.returncode != 0:
            raise RuntimeError(f"SHOW {what}: {done.stderr}")
        return parse_show(done.stdout)

    def _as_runas(self, command):
        return ["sudo", "-u", self.cluster.runas, "--", *command] if self.cluster.runas else command
