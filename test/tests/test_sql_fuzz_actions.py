"""The fuzzer's plan of actions: reproducible from the seed, transactions
always ended, and the fixture's graphs built again once dropped."""

import random

from harness.sql_fuzz.actions import Planner
from harness.sql_fuzz.fixture import BUILDS, OWNER
from harness.sql_fuzz.surface import Argument, Function
from harness.sql_fuzz.world import Graph, World

SURFACE = (Function("info", (Argument("name", "text"),), 0, False, "jsonb"),
           Function("backends", (), 0, True, "record"))
WHOLE = World(tuple(Graph(name, "bigint", OWNER) for name in BUILDS))


def plan(seed, world=WHOLE, count=300):
    planner = Planner(random.Random(seed), SURFACE)
    return [planner.next(world) for _ in range(count)]


def test_the_same_seed_plans_the_same_actions():
    assert plan(4) == plan(4)


def test_a_transaction_has_one_to_four_units_and_ends():
    for action in plan(1):
        if action.kind == "transaction":
            assert 1 <= len(action.units) <= 4
            assert action.end in ("COMMIT", "ROLLBACK")


def test_a_set_is_counted_and_a_scalar_measured():
    for action in plan(2):
        for unit in action.units:
            assert unit.form in (("rows", "first") if unit.call.function.returns_set else ("value",))


def test_a_dropped_fixture_graph_is_built_again_by_its_owner():
    without_gq = World(tuple(g for g in WHOLE.graphs if g.name != "gq"))
    builds = [a for a in plan(3, without_gq) if a.kind == "build"]
    assert builds and all(a.role == OWNER and a.script == BUILDS["gq"] for a in builds)
    assert not [a for a in plan(3) if a.kind == "build"]
