"""One backend, driven through a psql coprocess, so that a sequence of
statements runs in one session (its transactions, settings and cache) and
each statement's outcome, rows or SQLSTATE, is returned rather than
raised. The concurrent refresh and crash recovery checks run many at once,
one per thread.

After each statement psql echoes the error message of the last failed one
and the statement's SQLSTATE (`00000` when it succeeded), between markers
that `outcome` parses."""

import subprocess
from dataclasses import dataclass

MESSAGE = "@@warren-message@@"
END = "@@warren-end@@"
SEPARATOR = "\t"
SUCCESS = "00000"
# Every session's settings: notices off, a statement that hangs (a lock
# never released, a server that never answers) fails rather than holding
# the run forever, and a backend whose psql was killed (`Session.close`)
# stops its statement within a second rather than at its end.
SESSION_SETTINGS = ("client_min_messages = warning", "statement_timeout = '120s'",
                    "client_connection_check_interval = '1s'")
CLOSE_SECONDS = 10


class BackendLost(RuntimeError):
    """psql ended: its server connection was lost (a terminated or crashed
    backend, or a server restart)."""


@dataclass(frozen=True)
class Outcome:
    rows: list
    sqlstate: str
    message: str

    @property
    def ok(self):
        return self.sqlstate == SUCCESS

    def value(self):
        """The first column of the only row, or a `RuntimeError` naming the failure."""
        if not self.ok:
            raise RuntimeError(f"{self.sqlstate}: {self.message}")
        if len(self.rows) != 1:
            raise RuntimeError(f"expected one row, got {self.rows!r}")
        return self.rows[0][0]


def script(sql):
    """The text that runs `sql` and then prints its outcome's markers."""
    return f"{sql.rstrip().rstrip(';')};\n\\echo {MESSAGE} :LAST_ERROR_MESSAGE\n\\echo {END} :SQLSTATE\n"


def outcome(lines):
    """The outcome of one statement from psql's output `lines` (without
    their newlines), up to and including the end marker: tab-separated rows
    up to the message marker; the message, which may span lines, after it;
    and the SQLSTATE on the end marker's line. The message is that of the
    session's last error, so it is kept only when the statement failed."""
    message_at = next(i for i, line in enumerate(lines) if line.startswith(MESSAGE))
    rows = [line.split(SEPARATOR) for line in lines[:message_at]]
    message = "\n".join([lines[message_at][len(MESSAGE) + 1:], *lines[message_at + 1:-1]])
    sqlstate = lines[-1][len(END) + 1:].strip()
    return Outcome(rows, sqlstate, "" if sqlstate == SUCCESS else message)


class Session:
    def __init__(self, cluster, database, settings=()):
        command = [cluster.client("psql"), "-X", "-q", "-A", "-t", "-F", SEPARATOR, "-v", "ON_ERROR_STOP=0",
                   *cluster.connection(database)]
        # Notices go to stderr, which nothing reads: discarded, so it never fills.
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL, text=True, bufsize=1)
        for setting in (*SESSION_SETTINGS, *settings):
            self._set(setting)
        self.pid = int(self.run("SELECT pg_backend_pid()").value())

    def _set(self, setting):
        done = self.run(f"SET {setting}")
        if not done.ok:
            raise RuntimeError(f"SET {setting}: {done.message}")

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def close(self, seconds=CLOSE_SECONDS):
        """Ends psql: at once if it is idle; if a statement is still
        running after `seconds`, by killing it (its backend then stops the
        statement, `client_connection_check_interval`)."""
        try:
            self.process.stdin.close()
        except BrokenPipeError:
            pass
        try:
            self.process.wait(timeout=seconds)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()

    def run(self, sql):
        """Runs `sql`; its outcome. Raises `BackendLost` if psql ends first."""
        try:
            self.process.stdin.write(script(sql))
            self.process.stdin.flush()
        except BrokenPipeError as lost:
            raise BackendLost(f"psql ended before: {sql[:200]}") from lost
        lines = []
        for line in self.process.stdout:
            lines.append(line.rstrip("\n"))
            if line.startswith(END):
                return outcome(lines)
        raise BackendLost(f"psql ended during: {sql[:200]}")
