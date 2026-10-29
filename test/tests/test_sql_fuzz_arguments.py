"""The fuzzer's generated calls: reproducible from their seed, every
required argument given, and, when not hostile, consistent with the graph
they name (its id type, versions and vectors)."""

import random

from harness.sql_fuzz.arguments import Arguments
from harness.sql_fuzz.surface import Argument, Function
from harness.sql_fuzz.world import Graph, World

WORLD = World((Graph("g8", "bigint", "fuzz_owner", ("fuzz_reader",), True, True),
               Graph("gt", "text", "fuzz_owner")),
              versions=(("g8", 2, False), ("g8", 3, True), ("gt", 5, False)),
              vectors=(("g8", "pr", 2), ("gt", "ones", 5)))
DEGREE = Function("degree", (Argument("graph", "text"), Argument("node", "anyelement"),
                             Argument("direction", "text"), Argument("version", "bigint")), 2, False, "bigint")
CREATE = Function("create_graph", (Argument("name", "text"), Argument("node_query", "text"),
                                   Argument("edge_query", "text"), Argument("weighted", "boolean"),
                                   Argument("has_relations", "boolean")), 2, False, "void")
FROM_KEYS = Function("create_graph_from_keys", (Argument("name", "text"), Argument("tables", "regclass[]")),
                     0, False, "void")
BFS = Function("bfs", (Argument("graph", "text"), Argument("start", "anyarray"),
                       Argument("max_depth", "integer")), 0, True, "record")
QUERY = Function("vector_from_query", (Argument("graph", "text"), Argument("name", "text"),
                                       Argument("query", "text")), 0, False, "bigint")


def calls(function, hostility, seeds=range(200)):
    return [Arguments(random.Random(seed), WORLD, hostility).call(function) for seed in seeds]


def test_the_same_seed_makes_the_same_call():
    assert calls(DEGREE, 0.3, [5]) == calls(DEGREE, 0.3, [5])
    assert len({c.expression() for c in calls(DEGREE, 0.3)}) > 20


def test_every_required_argument_is_given():
    assert all(len(c.positional) == 2 for c in calls(DEGREE, 0.5))


def test_without_hostility_a_node_has_the_type_of_the_graph_it_names():
    for call in calls(DEGREE, 0):
        graph, node = call.positional
        if graph == "'g8'":
            assert node.endswith("::bigint"), call.expression()
        elif graph == "'gt'":
            assert node.endswith("::text"), call.expression()


def test_without_hostility_a_version_is_one_of_the_graphs_or_null():
    for call in calls(DEGREE, 0):
        named = dict(call.named)
        if "version" in named and call.positional[0] in ("'g8'", "'gt'"):
            graph = call.positional[0].strip("'")
            assert named["version"] in {"NULL", *map(str, WORLD.versions_of(graph))}, call.expression()


def test_a_call_records_the_existing_graphs_it_names():
    for call in calls(DEGREE, 0.3):
        name = call.positional[0].strip("'")
        assert call.graphs == ((name,) if WORLD.graph(name) else ())


def test_hostile_values_include_nulls_extremes_and_wrong_types():
    nodes = {c.positional[1] for c in calls(DEGREE, 1.0, range(400))}
    assert {"NULL", "1", "'x'::text", "NULL::bigint"} <= nodes


def test_a_new_graphs_flags_mostly_match_the_columns_its_edge_query_reads():
    matching = 0
    made = calls(CREATE, 0)
    for call in made:
        edges = call.positional[2]
        named = dict(call.named)
        weighted = named.get("weighted", "false") == "true"
        related = named.get("has_relations", "false") == "true"
        matching += (weighted == (", w" in edges)) and (related == (", rel" in edges))
    assert matching > len(made) // 2


def test_a_graph_from_keys_is_mostly_new_and_lists_tables():
    made = calls(FROM_KEYS, 0)
    assert sum(c.positional[0] not in ("'g8'", "'gt'") for c in made) > len(made) // 2
    assert all(c.positional[1].endswith("::regclass[]") for c in made)


def test_the_sql_a_call_runs_is_recorded():
    for call in calls(QUERY, 0.5):
        assert call.queries == (call.positional[2],)


def test_a_vector_name_with_a_quote_stays_inside_a_hostile_expressions_literal():
    world = World((Graph("g8", "bigint", "fuzz_owner"),), versions=(("g8", 2, True),),
                  vectors=(("g8", "it's", 2),))
    explain = Function("explain_map", (Argument("graph", "text"), Argument("expr", "text")), 0, False, "jsonb")
    for seed in range(300):
        expr = Arguments(random.Random(seed), world, 0.5).call(explain).positional[1]
        if "it'" in expr:
            assert "it's" not in expr.replace("''", ""), expr


def test_some_id_arrays_are_read_from_a_table_column_of_the_graphs_id_type():
    stored = [c.positional[1] for c in calls(BFS, 0) if "public.stored_ids" in c.positional[1]]
    assert stored, "no start array read from a table"
    for value in stored:
        assert "public.stored_ids" in value and "kind" in value
    for call in calls(BFS, 0):
        if call.positional[0] == "'gt'" and "stored_ids" in call.positional[1]:
            assert "SELECT t FROM" in call.positional[1]
