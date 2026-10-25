"""Running actions against a database: one session per role, connected
when first used, each statement under a watchdog, each outcome judged
(`verdict`), and after each action the leak checks (nothing pinned in the
shared cache, no activity row of a backend that is gone). What ran is
recorded, statement by statement, so a failure can be replayed and shrunk
(`shrink`).

A crash (the backend, or the server, lost) or a hang (a statement still
running well past its timeout) is a finding; the run waits for the
server to come back and goes on with new sessions."""

import threading
import time
from collections import Counter
from dataclasses import dataclass

from ..cluster import SUPERUSER
from ..concurrent_refresh import until
from ..session import BackendLost, Session
from . import world as world_module
from .actions import CONNECT, STATEMENT_TIMEOUT
from .verdict import Finding, changes, judge, signature

# How long past its statement timeout a statement may run before the
# watchdog kills its psql (its backend then stops within a second).
WATCHDOG_SECONDS = 30
RECOVERY_SECONDS = 60
SESSION_SETTINGS = (f"statement_timeout = '{STATEMENT_TIMEOUT}'",)
LEAKS_SQL = ("SELECT (SELECT pg_catalog.count(*) FROM warren.shared_cache() WHERE pins <> 0), "
             "(SELECT coalesce(pg_catalog.string_agg(pid::text, ',' ORDER BY pid), '') FROM warren.backends())")


@dataclass(frozen=True)
class Executed:
    """One statement as it ran: `CONNECT` opens a new session for `role`;
    a call keeps its `Call`, form and limit, so it can be rebuilt without
    an argument when shrinking."""
    role: str
    sql: str
    call: object = None
    form: str | None = None
    limit: int = 1


@dataclass
class Failure:
    finding: Finding
    statement: Executed
    transcript: list  # the actions (lists of Executed) up to and including the failing statement
    occurrences: int = 1

    @property
    def signature(self):
        function = self.statement.call.function.name if self.statement.call else None
        return signature(function, self.finding)


class Lost(Exception):
    """A statement's backend was lost; `hung` if the watchdog killed it."""

    def __init__(self, hung):
        super().__init__("hung" if hung else "lost")
        self.hung = hung


class Run:
    """Statements run on database `database` of `cluster`, as roles; each
    new failure is printed unless `quiet`."""

    def __init__(self, cluster, database, quiet=False):
        self.cluster, self.database, self.quiet = cluster, database, quiet
        self.sessions = {}
        self.inspector = None
        self.world = world_module.World()
        self.transcript = []  # finished actions
        self.action = []  # the current action's statements so far
        self.failures = {}  # signature → Failure
        self.statements = 0
        self.outcomes = Counter()  # (function, SQLSTATE) of every call

    # Sessions.

    def close(self):
        for session in [*self.sessions.values(), self.inspector]:
            if session is not None:
                session.close()
        self.sessions, self.inspector = {}, None

    def _session(self, role):
        if role not in self.sessions:
            self.connect(role)
        return self.sessions[role]

    def connect(self, role, search_path=None):
        """A new session for `role` (closing its old one), recorded."""
        old = self.sessions.pop(role, None)
        if old is not None:
            old.close()
        self.sessions[role] = Session(self.cluster, self.database, SESSION_SETTINGS, user=role)
        self.action.append(Executed(role, CONNECT))
        if search_path is not None:
            self.statement(role, f"SET search_path = {search_path}")

    def _inspect(self, sql):
        if self.inspector is None:
            self.inspector = Session(self.cluster, self.database)
        return self.inspector.run(sql)

    # Statements.

    def refresh_world(self):
        self.world = world_module.parse(self._inspect(world_module.WORLD_SQL).value())
        return self.world

    def statement(self, role, sql, call=None, form=None, limit=1):
        """Runs `sql` as `role`, records it and judges its outcome; the
        outcome, or None if the backend was lost (a finding, and the run
        recovers)."""
        executed = Executed(role, sql, call, form, limit)
        self.action.append(executed)
        self.statements += 1
        try:
            done = self._run(role, sql)
        except Lost as lost:
            self._found(Finding("hang" if lost.hung else "crash",
                                "still running well past its statement timeout" if lost.hung else "backend lost"),
                        executed)
            self.recover()
            return None
        if call is not None:
            self.outcomes[(call.function.name, done.sqlstate)] += 1
        found = judge(role, call, done, self.world)
        if found is not None:
            self._found(found, executed)
        return done

    def _run(self, role, sql):
        session = self._session(role)
        timer = threading.Timer(WATCHDOG_SECONDS, session.process.kill)
        timer.start()
        try:
            return session.run(sql)
        except BackendLost as lost:
            raise Lost(hung=not timer.is_alive()) from lost
        finally:
            timer.cancel()

    def _found(self, finding, executed):
        failure = Failure(finding, executed, [*self.transcript, list(self.action)])
        known = self.failures.get(failure.signature)
        if known is None:
            self.failures[failure.signature] = failure
            if not self.quiet:
                print(f"found {failure.signature}: {finding.detail[:300]}", flush=True)
        else:
            known.occurrences += 1

    def end_action(self):
        self.transcript.append(self.action)
        self.action = []

    def recover(self):
        """After a lost backend: drops every session and waits for the server
        to accept connections again."""
        for session in [*self.sessions.values(), self.inspector]:
            if session is not None:
                session.close(seconds=1)
        self.sessions, self.inspector = {}, None
        deadline = time.monotonic() + RECOVERY_SECONDS
        while True:
            try:
                self._inspect("SELECT 1").value()
                return
            except (BackendLost, RuntimeError, BrokenPipeError):
                self.inspector = None
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.5)

    # Actions.

    def perform(self, action):
        """Runs planned `action` (`actions`), then judges what it changed
        and checks for leaks; the world is then read again."""
        before = self.world
        if action.kind == "connect":
            self.connect(action.role, action.search_path)
        elif action.kind == "call":
            self._unit(action.role, action.units[0])
        elif action.kind == "build":
            for sql in action.script:
                done = self.statement(action.role, sql)
                if done is None or not done.ok:
                    break
        else:
            self._transaction(action)
        self.end_action()
        self.after_action(action.role, before)

    def after_action(self, role, before):
        """Reads the world again, judges what `role`'s last action changed
        in it since `before` (`verdict.changes`), and checks for leaks."""
        for finding in changes(role, before, self.refresh_world()):
            self._found(finding, self._culprit(finding))
        self.check_leaks()

    def _culprit(self, finding):
        """The last action's last call that names the graph `finding` is
        about (else its last statement): what the finding is filed under."""
        action = self.transcript[-1] if self.transcript else [Executed(SUPERUSER, "")]
        calls = [e for e in action if e.call is not None]
        named = [e for e in calls
                 if any(f"graph {g}," in finding.detail or f" of {g}," in finding.detail for g in e.call.graphs)]
        return (named or calls or action)[-1]

    def _unit(self, role, unit):
        """Runs `unit`'s call; its outcome, or None if the backend was lost."""
        return self.statement(role, unit.call.statement(unit.form, unit.limit), unit.call, unit.form, unit.limit)

    def _transaction(self, action):
        """BEGIN, the settings, the units (a failure outside a savepoint
        rolls the transaction back and ends it), then COMMIT or ROLLBACK."""
        role = action.role
        for setting in ("BEGIN", *action.begin):
            done = self.statement(role, setting)
            if done is None:
                return
            if not done.ok:
                self.statement(role, "ROLLBACK")
                return
        for unit in action.units:
            if unit.savepoint and self.statement(role, "SAVEPOINT s") is None:
                return
            done = self._unit(role, unit)
            if done is None:
                return
            if unit.savepoint:
                keep = done.ok and unit.savepoint == "release"
                if self.statement(role, "RELEASE SAVEPOINT s" if keep else "ROLLBACK TO SAVEPOINT s") is None:
                    return
            elif not done.ok:
                self.statement(role, "ROLLBACK")
                return
        self.statement(role, action.end)

    # Leaks, after each action.

    def check_leaks(self):
        """Nothing pinned between statements, and no activity row of a
        backend that has gone (allowing it a moment to exit)."""
        pinned, pids = self._inspect(LEAKS_SQL).rows[0]
        live = {s.pid for s in self.sessions.values()} | {self.inspector.pid}
        gone = {int(p) for p in pids.split(",") if p} - live
        if gone:
            left = until(self.inspector, "SELECT pg_catalog.count(*) FROM warren.backends() "
                                         f"WHERE pid IN ({', '.join(map(str, sorted(gone)))})", seconds=2)
            if left != "0":
                self._leak(f"warren.backends() lists backends that have exited: {sorted(gone)}")
        if pinned != "0":
            self._leak(f"{pinned} shared cache entries are pinned between statements: "
                       f"{self._inspect('SELECT * FROM warren.shared_cache() WHERE pins <> 0').rows}")

    def _leak(self, detail):
        last = self.transcript[-1][-1] if self.transcript and self.transcript[-1] else Executed(SUPERUSER, "")
        self._found(Finding("leak", detail), last)


def replay(run, actions, more=lambda: True):
    """Runs recorded `actions` again on `run` (a fresh database); the first
    failure it finds, or None (also once `more()` is false, checked before
    each action)."""
    if not more():
        return None
    run.refresh_world()
    for action in actions:
        if not more():
            return None
        before = run.world
        for executed in action:
            if executed.sql == CONNECT:
                old = run.sessions.pop(executed.role, None)
                if old is not None:
                    old.close()
                run.sessions[executed.role] = Session(run.cluster, run.database, SESSION_SETTINGS, user=executed.role)
                continue
            run.statement(executed.role, executed.sql, executed.call, executed.form, executed.limit)
            if run.failures:
                return next(iter(run.failures.values()))
        run.end_action()
        roles = [executed.role for executed in action if executed.sql != CONNECT]
        run.after_action(roles[-1] if roles else SUPERUSER, before)
        if run.failures:
            return next(iter(run.failures.values()))
    return None
