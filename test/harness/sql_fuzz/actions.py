"""What the fuzzer does next, as one *action* of one role, which never
leaves a transaction open: a call on its own (autocommit), a transaction
of calls (some in savepoints that are rolled back or released, after
`SET LOCAL`s that bound resources or change the plan) that commits or
rolls back, or a new session (a new backend, a new `search_path`); and,
once one of the fixture's graphs has been dropped, its owner building it
again, so that the run keeps graphs with versions and vectors to call.

How an action runs, and what it does when a call fails, is the driver's
(`run`); here it is only planned, from the seed."""

from dataclasses import dataclass, field

from .arguments import Arguments
from .fixture import BUILDS, OWNER, ROLES

ROLE_WEIGHTS = (3, 4, 3, 2)
HOSTILITY = (0.0, 0.1, 0.25, 0.6)
CONNECT = "\\connect"
REBUILD_SHARE = 0.3
SEARCH_PATHS = (None, "hostile, pg_catalog, public", "public", "pg_temp, public", '""')
# Statement timeout of every fuzz session: long enough for any call on the
# fixture's graphs, short enough that a call that spins wastes little.
STATEMENT_TIMEOUT = "3s"


def settings(parallel_setting):
    """The `SET LOCAL`s a transaction may start with; `parallel_setting`
    is the server's name for forcing parallel plans."""
    return (
        "SET LOCAL warren.max_compute_memory = '64kB'", "SET LOCAL warren.max_compute_memory = '1MB'",
        "SET LOCAL warren.max_cache_memory = '64kB'", "SET LOCAL warren.max_build_memory = '64kB'",
        "SET LOCAL warren.max_walk_paths = 1", "SET LOCAL warren.threads = 1", "SET LOCAL warren.threads = 8",
        "SET LOCAL statement_timeout = '20ms'", "SET LOCAL lock_timeout = '10ms'",
        f"SET LOCAL {parallel_setting} = on", "SET LOCAL search_path = hostile, pg_catalog, public",
        "SET LOCAL transaction_read_only = on", "SET LOCAL warren.max_compute_memory = '0'",
    )


@dataclass(frozen=True)
class Unit:
    """One call in a transaction, in a savepoint that is rolled back or
    released after it, or in none."""
    call: object
    form: str
    limit: int = 1
    savepoint: str | None = None  # None, "rollback" or "release"


@dataclass(frozen=True)
class Action:
    role: str
    kind: str  # call, transaction, connect, build
    units: tuple = ()
    begin: tuple = ()  # what follows BEGIN: isolation, then SET LOCALs
    end: str = "COMMIT"
    search_path: str | None = None
    script: tuple = ()  # a build's statements


@dataclass
class Planner:
    rng: object
    surface: tuple
    parallel_setting: str = "debug_parallel_query"
    made: int = field(default=0)

    def next(self, world):
        """The next action, generated in `world`."""
        self.made += 1
        missing = [name for name in BUILDS if world.graph(name) is None]
        if missing and self.rng.random() < REBUILD_SHARE:
            return Action(OWNER, "build", script=BUILDS[missing[0]])
        role = self.rng.choices(ROLES, weights=ROLE_WEIGHTS)[0]
        kind = self.rng.choices(("call", "transaction", "connect"), weights=(55, 40, 5))[0]
        if kind == "connect":
            return Action(role, kind, search_path=self.rng.choice(SEARCH_PATHS))
        arguments = Arguments(self.rng, world, self.rng.choice(HOSTILITY))
        if kind == "call":
            return Action(role, kind, (self._unit(arguments, None),))
        begin = []
        if self.rng.random() < 0.15:
            begin.append("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        begin += self.rng.sample(settings(self.parallel_setting), self.rng.choice((0, 0, 1, 2)))
        units = tuple(self._unit(arguments, self.rng.choice((None, None, "rollback", "release")))
                      for _ in range(self.rng.randint(1, 4)))
        return Action(role, kind, units, tuple(begin), self.rng.choice(("COMMIT", "COMMIT", "ROLLBACK")))

    def _unit(self, arguments, savepoint):
        function = self.rng.choice(self.surface)
        call = arguments.call(function)
        if not function.returns_set:
            return Unit(call, "value", savepoint=savepoint)
        return Unit(call, self.rng.choice(("rows", "first")), self.rng.randint(1, 3), savepoint)
