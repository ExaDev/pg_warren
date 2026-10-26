"""A use case's plan: the steps `bench/appendix_a.py` runs in one backend,
in order. A *stage* is timed with the backend's peak memory (and has a
target where Appendix A states one); a *measure* records its value, which
later statements use as `@@key@@`; a *check* is one of the use case's
expected outcomes, a query whose row the expectation judges. A measure's
or check's value is the last row printed (its last statement's); a
stage's, its first."""

import re
from dataclasses import dataclass, field
from typing import Callable, Optional

USE = re.compile(r"@@(\w+)@@")


@dataclass(frozen=True)
class Step:
    name: str
    sql: str
    kind: str = "stage"  # "stage", "measure" or "check"
    key: Optional[str] = None  # a measure's or check's value is recorded under this name
    expect: Optional[Callable[[Optional[str]], bool]] = None  # a check's verdict on its value
    claim: str = ""  # what a check asserts, in Appendix A's words
    target: Optional[float] = None  # a stage's target, in seconds
    repeat: int = 1
    allowed_error: Optional[str] = None  # an error this stage may end with (a timeout it records)

    def uses(self):
        """The recorded values this step's SQL names."""
        return set(USE.findall(self.sql))


@dataclass(frozen=True)
class UseCase:
    name: str
    section: str  # the Appendix A section, e.g. "A.1"
    sizes: dict  # what was generated (nodes, edges, ...), as planned
    steps: tuple
    settings: tuple = field(default=())  # the session's SET statements beyond the threads


def resolve(sql, values):
    """`sql` with each `@@key@@` replaced by its recorded value."""
    return USE.sub(lambda m: values[m.group(1)], sql)


def _number(value):
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def at_least(bound):
    return lambda value: (n := _number(value)) is not None and n >= bound


def at_most(bound):
    return lambda value: (n := _number(value)) is not None and n <= bound


def between(low, high):
    return lambda value: (n := _number(value)) is not None and low <= n <= high


def equals(expected):
    return lambda value: value == expected


def is_true(value):
    return value == "t"
