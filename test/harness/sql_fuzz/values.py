"""The values the fuzzer draws arguments from, as SQL text: the plausible
ones by argument name (directions, semirings, dtypes, sensible numbers,
source queries over the fixture's tables, map expressions) and the
hostile ones by type (NULLs, empty and huge values, extremes, NaN and the
infinities, wrong types, injections, SQL that writes or reads the
secret)."""

from .fixture import RELATIONS, SECRET, literal

# Argument names, by what they name.
GRAPH_ARGUMENTS = frozenset({"graph", "out_graph", "graph2"})
VECTOR_ARGUMENTS = frozenset({"out", "x", "y", "a", "b", "labels", "communities", "initial", "keys", "mask",
                              "hubs", "authorities", "label_vector", "seed", "sources", "nodes",
                              "vectors_prefix", "levels_prefix", "name"})
VERSION_ARGUMENTS = frozenset({"version", "v1", "v2", "a_version", "b_version"})
QUERY_ARGUMENTS = frozenset({"node_query", "edge_query", "query", "messages", "pairs"})

# The words each enumerated argument takes, and some it does not.
WORDS = {
    "direction": ("out", "in", "both"),
    "agg": ("sum", "mean", "max", "min", "count"),
    "dtype": ("f64", "f32", "i64", "bool"),
    "semiring": ("plus_times", "min_plus", "max_plus", "max_times", "min_times", "lor_land", "plus_pair",
                 "plus_first", "plus_second", "min_first", "min_second", "max_second", "any_pair"),
    "combine": ("collect", "sum", "min", "max", "count", "mean", "first", "any", "concat", "mode", "bool_and",
                "bool_or"),
    "accumulate": ("first", "second", "pair", "plus", "times", "min", "max", "lor", "land", "any"),
    "measure": ("jaccard", "adamic_adar", "common_neighbours"),
    "method": ("edit", "degree", "subspace"),
    "matrix": ("adjacency", "laplacian", "normalized_laplacian", "random_walk"),
    "which": ("largest", "smallest"),
    "norm": ("l1", "l2", "linf"),
    "op": ("sum", "min", "max", "mean", "count", "nnz"),
    "mode": ("ends", "count", "paths"),
    "node_order": ("id", "locality"),
    "compared_rows": ("all", "shared"),
}
ODD_WORDS = ("SUM", " out", "sideways", "nope", "plus_times_plus", "f16", "")

# Sensible numbers, by argument name; anything else gets SMALL.
SMALL = ("0", "1", "2", "3", "10")
NUMBERS = {
    "k": ("1", "2", "3", "5", "50"),
    "max_iter": ("1", "10", "100"),
    "max_levels": ("1", "2", "10"), "iterations": ("1", "2"), "max_sweeps": ("1", "5", "50"),
    "max_results": ("1", "10", "1000"),
    "retain_versions": ("1", "2", "3"),
    "bytes": ("0", "1000", "1000000"),
    "seed": ("0", "1", "42"),
    "done": ("0", "5"), "total": ("10", "100"),
}
REALS = {
    "damping": ("0.85", "0.5", "1", "0"),
    "tol": ("0.000001", "0.001", "0"), "tolerance": ("0.0000001", "0.01"),
    "alpha": ("0.1", "0.01"), "beta": ("1", "0.5"),
    "resolution": ("1", "0.5", "2"), "randomness": ("0.01", "1"),
    "value": ("1", "0", "-2.5", "3"), "fill": ("0", "1"),
}

HOSTILE = {
    "text": ("NULL", "''", "' '", "pg_catalog.repeat('x', 100000)", "'ñ✓𝔘'", "'it''s'",
             literal("'); DROP TABLE public.canary; --"), literal('"; DROP TABLE public.canary; --'),
             "'NaN'", "E'\\n\\t'", "'a' || pg_catalog.chr(0)", "pg_catalog.repeat('é', 70)"),
    "text[]": ("NULL", "'{}'::text[]", "ARRAY[NULL]::text[]", "ARRAY['']::text[]", "'{{a},{b}}'::text[]",
               "pg_catalog.array_fill('a'::text, ARRAY[100000])", "ARRAY['a', 'a', NULL]",
               "'[0:1]={a,b}'::text[]"),
    "integer": ("NULL", "0", "-1", "2147483647", "-2147483648"),
    "bigint": ("NULL", "0", "-1", "9223372036854775807", "-9223372036854775808", "4294967296"),
    "double precision": ("NULL", "'NaN'::float8", "'Infinity'::float8", "'-Infinity'::float8", "'-0'::float8",
                         "1e308", "5e-324", "-1"),
    "boolean": ("NULL",),
    "regrole": ("NULL", "'pg_database_owner'", "'pg_read_all_data'", "0::oid::regrole", "'fuzz_stranger'",
                "'postgres'", "'pg_signal_backend'"),
    "anyelement": ("NULL", "NULL::bigint", "1", "1.5", "'1'::text", "'x'::text", "1::smallint", "1::numeric",
                   "'00000000-0000-0000-0000-000000000000'::uuid", "ARRAY[1::bigint]", "true", "'(1,2)'::point",
                   "'{}'::jsonb", "9223372036854775807::bigint", "(-1)::bigint", "2147483647"),
    "anyarray": ("NULL", "NULL::bigint[]", "'{}'::bigint[]", "ARRAY[NULL::bigint]", "ARRAY[[1, 2], [3, 4]]::bigint[]",
                 "ARRAY[1, 2]", "ARRAY['a', 'b']", "ARRAY(SELECT g::bigint FROM pg_catalog.generate_series(1, 100000) g)",
                 "ARRAY[1::bigint, 1::bigint]", "'[0:1]={1,2}'::bigint[]", "ARRAY[1.5]", "'{}'::uuid[]"),
    '"any"': ("NULL", "NULL::bigint[]", "'{}'::bigint[]", "1::bigint", "'x'::text", "ARRAY[NULL::bigint]",
              "ARRAY[[1, 2]]::bigint[]", "ARRAY[1, 2]", "ARRAY['a']", "'{}'::jsonb"),
    "anycompatible": ("NULL", "NULL::bigint", "NULL::text", "NULL::jsonb", "NULL::bigint[]", "NULL::numeric",
                      "NULL::boolean", "NULL::uuid", "NULL::point", "NULL::float8[]", "NULL::real"),
    "warren.step[]": ("NULL", "'{}'::warren.step[]", "ARRAY[NULL::warren.step]",
                      "pg_catalog.array_fill(warren.s(NULL), ARRAY[200])",
                      "ARRAY[warren.s(NULL, 'out', NULL::bigint[], 0, 0)]",
                      "ARRAY[warren.s(NULL, 'out', NULL::bigint[], 5, 1)]",
                      "ARRAY[warren.s(ARRAY['a'], 'both', NULL::bigint[], 1, 2147483647)]",
                      "ARRAY[warren.s(NULL, 'out', NULL::bigint[], -1, NULL)]",
                      "ARRAY[warren.s(NULL, 'out', 'x'::text, 1, 2)]"),
    "jsonb": ("NULL", "'null'", "'{}'", "'[1]'", "'[{\"bogus\": 1}]'", "'[{\"rels\": \"a\"}]'",
              "'[{\"min_hops\": -1}]'", "'[{\"max_hops\": 1e100}]'", "'[{\"direction\": \"up\"}]'",
              "pg_catalog.repeat('[', 5000)::jsonb", "'[{\"filter\": [1, \"a\"]}]'"),
}

QUERIES = (
    "''", "' '", "'SELECT'", "'COMMIT'", "'ROLLBACK'", "'SAVEPOINT s'", "'SELECT 1; SELECT 2'", "'not sql'",
    literal("INSERT INTO public.canary VALUES ('x') RETURNING 1::bigint, 1::float8"),
    literal("WITH w AS (INSERT INTO public.canary VALUES ('x') RETURNING 1::bigint AS id) SELECT id, id FROM w"),
    literal("SELECT public.scribble(), 1::float8"), literal("SELECT public.scribble()"),
    literal(f"SELECT src, dst FROM {SECRET}"), literal(f"SELECT id, 1::float8 FROM {SECRET}"),
    literal(f"SELECT id FROM {SECRET}"),
    literal("SELECT 1 / 0, 1"), literal("SELECT pg_catalog.pg_sleep(0.2)::text, 1"),
    literal("SELECT NULL::bigint, NULL::float8"), literal("SELECT NULL, NULL"),
    literal("SELECT x::bigint, x::float8 FROM pg_catalog.generate_series(1, 200000) x"),
    literal("SELECT id, id FROM public.nodes_text"), literal("SELECT 1::bigint"),
    literal("SELECT id, 1::float8 FROM public.nodes8 WHERE id = 1"),
    literal("SELECT id, id || 'y' FROM public.nodes_text"),
    literal("VALUES (1::bigint, 2::float8), (1, 3)"), literal("TABLE public.edges8"),
    literal("SELECT * FROM public.edges8"),
    literal("SELECT pg_catalog.set_config('search_path', 'hostile', true), 1"),
    literal("SELECT pg_catalog.txid_current(), 1::float8"),
    literal("CREATE TEMP TABLE t AS SELECT 1"), literal("DROP TABLE public.canary"), literal("VACUUM"),
    literal("SELECT id, 1::float8 FROM public.nodes8 FOR UPDATE"),
    literal("SELECT id, 'Infinity'::float8 FROM public.nodes8"), literal("SELECT id, 'NaN'::float8 FROM public.nodes8"),
    literal("SELECT 1::bigint, " + " + ".join(["1"] * 3000)),
    literal("SELECT id FROM public.nodes8 UNION ALL SELECT NULL"),
    literal("SELECT src, dst, rel, w FROM public.edges8 ORDER BY w DESC"),
)

EXPRESSIONS = ("{v} + 1", "{v} * 2", "{v} / 2", "abs({v})", "{v} > 0", "CASE WHEN {v} > 0 THEN {v} ELSE 0 END",
               "greatest({v}, 1)", "sqrt({v})", "{v} / 0", "ln({v})", "{v}::bigint", "coalesce({v}, 0)", "1",
               "NULL", "{v} + {w}", "-{v}")
HOSTILE_EXPRESSIONS = ("''", "'('", literal("1; DROP TABLE public.canary"), "'public.scribble()'", "'(SELECT 1)'",
                       "'pg_catalog.pg_sleep(0.1)'", "'nosuch + 1'",
                       "pg_catalog.repeat('(', 3000) || '1' || pg_catalog.repeat(')', 3000)",
                       "'random()'", "'now()'", literal("{v} || 'x'"), "'{v} = {v}'", "'{v}::text'",
                       "pg_catalog.repeat('{v} + ', 3000) || '1'")

RELATION_ARRAYS = ("NULL", "ARRAY['a']", "ARRAY['a', 'b']", "ARRAY['c', 'zz']", "'{}'::text[]",
                   f"ARRAY[{', '.join(literal(r) for r in RELATIONS)}]")
FREE_TEXT = ("'fuzz'", "'phase one'", "NULL", "'ü'")
