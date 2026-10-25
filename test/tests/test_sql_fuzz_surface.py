"""The SQL fuzzer's view of the surface: every `warren.*` function as
`pg_proc` describes it, and the statements that call one."""

import json

from harness.sql_fuzz.surface import Argument, Call, Function, ident, parse

ROWS = json.dumps([
    {"name": "degree", "defaults": 3, "set": False, "result": "bigint",
     "strict": True, "args": [{"name": "graph", "type": "text"}, {"name": "node", "type": "anyelement"},
              {"name": "direction", "type": "text"}, {"name": "rels", "type": "text[]"},
              {"name": "version", "type": "bigint"}]},
    {"name": "backends", "defaults": 0, "set": True, "result": "record", "args": None, "strict": False},
])


def test_parse_reads_each_function_with_its_input_arguments_and_defaults():
    degree, backends = parse(ROWS)
    assert degree.name == "degree"
    assert degree.required == (Argument("graph", "text"), Argument("node", "anyelement"))
    assert [a.name for a in degree.optional] == ["direction", "rels", "version"]
    assert not degree.returns_set
    assert backends.arguments == () and backends.returns_set


def test_an_identifier_is_always_quoted():
    assert ident("out") == '"out"'
    assert ident('we"ird') == '"we""ird"'


def test_a_call_passes_required_arguments_by_position_and_optional_ones_by_name():
    degree = parse(ROWS)[0]
    call = Call(degree, ("'g'", "1::bigint"), (("version", "NULL"),))
    assert call.expression() == "warren.\"degree\"('g', 1::bigint, \"version\" => NULL)"


def test_a_value_is_measured_so_any_result_type_prints_one_short_row():
    call = Call(Function("info", (Argument("name", "text"),), 0, False, "jsonb"), ("'g'",), ())
    assert call.statement("value") == "SELECT pg_catalog.length(CAST(warren.\"info\"('g') AS text))"


def test_a_set_is_counted_in_full_or_streamed_from_the_select_list_under_a_limit():
    call = Call(parse(ROWS)[1], (), ())
    assert call.statement("rows") == "SELECT pg_catalog.count(*) FROM warren.\"backends\"() AS r"
    assert call.statement("first", limit=3) == ("SELECT pg_catalog.count(*) FROM "
                                                "(SELECT warren.\"backends\"() LIMIT 3) AS r")


def test_a_call_drops_a_named_argument_for_shrinking():
    degree = parse(ROWS)[0]
    call = Call(degree, ("'g'", "1::bigint"), (("direction", "'in'"), ("version", "7")))
    assert call.without("direction").named == (("version", "7"),)


def test_a_strict_function_given_a_null_literal_is_not_called():
    degree = parse(ROWS)[0]
    assert degree.strict
    assert Call(degree, ("'g'", "NULL::bigint")).skipped()
    assert Call(degree, ("'g'", "1::bigint"), (("version", "NULL"),)).skipped()
    assert not Call(degree, ("'g'", "1::bigint")).skipped()
    assert not Call(parse(ROWS)[1], ("NULL",)).skipped()
