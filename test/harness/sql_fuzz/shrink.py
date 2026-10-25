"""Shrinking a failure to the fewest statements that reproduce it, each
attempt replayed (`run.replay`) in a fresh copy of the fixture's database:
first the failing action alone, then with ever more of the actions before
it until it reproduces; then delta debugging (`ddmin`) over those earlier
actions and over the failing action's own earlier statements; then the
failing call without each optional argument in turn. Bounded by a time
budget: what is left when it runs out is reported as it stands."""

import math
import time
from dataclasses import replace

from ..cluster import SUPERUSER
from .actions import CONNECT
from .fixture import steps
from .run import Run
from .run import replay as replay_actions

REPLAY_DATABASE = "fuzz_replay"


def ddmin(items, test, more=lambda: True):
    """The smallest sublist of `items` delta debugging finds for which
    `test` still holds (`test(items)` is assumed to); `more()` false stops
    it early."""
    granularity = 2
    while items and more():
        chunk = math.ceil(len(items) / granularity)
        for start in range(0, len(items), chunk):
            complement = items[:start] + items[start + chunk:]
            if not more():
                return items
            if test(complement):
                items, granularity = complement, max(granularity - 1, 2)
                break
        else:
            if granularity >= len(items):
                return items
            granularity = min(len(items), granularity * 2)
    return items


class Shrinker:
    """Shrinks failures found in a database copied from `template` of
    `cluster`, spending at most `seconds` on each."""

    def __init__(self, cluster, template, seconds):
        self.cluster, self.template, self.seconds = cluster, template, seconds
        self.deadline = 0.0
        self.replays = 0

    def _more(self):
        return time.monotonic() < self.deadline

    def reproduces(self, actions, target):
        """Whether replaying `actions` on a fresh copy of the template fails
        with the signature `target`."""
        if not self._more():
            return False
        self.replays += 1
        admin = self.cluster.database()
        admin.execute(f"DROP DATABASE IF EXISTS {REPLAY_DATABASE} WITH (FORCE)")
        admin.execute(f"CREATE DATABASE {REPLAY_DATABASE} TEMPLATE {self.template}")
        run = Run(self.cluster, REPLAY_DATABASE, quiet=True)
        try:
            found = replay_actions(run, actions)
        finally:
            run.close()
        return found is not None and found.signature == target

    def shrink(self, failure):
        """The fewest actions found that reproduce `failure`, or None if even
        its whole transcript does not (within the budget)."""
        self.deadline = time.monotonic() + self.seconds
        target, earlier, last = failure.signature, failure.transcript[:-1], failure.transcript[-1]
        before = None
        for count in _growing(len(earlier)):
            if self.reproduces([*earlier[len(earlier) - count:], last], target):
                before = earlier[len(earlier) - count:]
                break
            if not self._more():
                return None
        if before is None:
            return None
        before = ddmin(before, lambda sub: self.reproduces([*sub, last], target), self._more)
        *setup, failing = last
        setup = ddmin(setup, lambda sub: self.reproduces([*before, [*sub, failing]], target), self._more)
        if failing.call is not None:
            for name, _ in failing.call.named:
                smaller = failing.call.without(name)
                candidate = replace(failing, sql=smaller.statement(failing.form, failing.limit), call=smaller)
                if self.reproduces([*before, [*setup, candidate]], target):
                    failing = candidate
        return [*before, [*setup, failing]]


def _growing(limit):
    """0, 1, 2, 4, ... up to `limit`, which is always last."""
    count = 0
    while count < limit:
        yield count
        count = 1 if count == 0 else count * 2
    yield limit


def script(actions, role=None):
    """`actions` as a psql script: `\\c - <role>` wherever the role changes
    or a new session was opened."""
    lines = []
    for action in actions:
        for executed in action:
            if executed.sql == CONNECT or executed.role != role:
                lines.append(f"\\c - {executed.role}")
                role = executed.role
            if executed.sql != CONNECT:
                lines.append(executed.sql.rstrip().rstrip(";") + ";")
    return "\n".join(lines)


def fixture_script():
    """The fixture as a psql script, for a fresh cluster's database with
    pg_warren."""
    lines = []
    role = None
    for step_role, sql in steps():
        if step_role != role:
            lines.append(f"\\c - {step_role}")
            role = step_role
        lines.append(sql.strip().rstrip(";") + ";")
    return "\n".join(lines) + f"\n\\c - {SUPERUSER}\n"
