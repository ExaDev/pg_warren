"""What every fuzz run starts from: roles of three privileges, small
source tables of each node id type, the graphs, versions and vectors
built from them, and the traps the oracle watches.

- `fuzz_owner` owns the graphs; `fuzz_reader` reads `g8`, `gt` and `gq`;
  `fuzz_stranger` reads none, owns schema `hostile` and has no access to
  the source tables. The cluster superuser is the fourth caller.
- `public.secret` is readable by the superuser alone, so a non-superuser
  call whose SQL reads it must fail (`verdict`).
- `public.canary` is writable by anyone and `public.scribble()` writes to
  it: the caller's SQL a `warren.*` function runs is read-only (DESIGN.md
  §7), so the canary stays empty (`settled`).
- `public.stored_ids` holds id arrays of each id type in a table column,
  short (stored with a one-byte header) and large (compressed and out of
  line), which a function reads detoasted, unlike a literal (`arguments`).
- Schema `hostile` defines `=` and `||` that call `hostile.escalated()`,
  which raises SQLSTATE `WF001` if it runs as a superuser on behalf of a
  session that is not one: a caller's operator run elevated (§7).

The data is fixed (its own seed), so every run and every replay of a
failure starts from the same state."""

import random
import uuid

from ..cluster import SUPERUSER

OWNER, READER, STRANGER = "fuzz_owner", "fuzz_reader", "fuzz_stranger"
ROLES = (SUPERUSER, OWNER, READER, STRANGER)
ESCALATED = "WF001"
SECRET = "public.secret"
RELATIONS = ("a", "b", "c")
SEED = 7
# Edge rows of each source table; with the node ids below, what the
# source tables hold (and still hold after a run, `settled`).
EDGE_ROWS = {"public.edges8": 160, "public.edges4": 80, "public.edges_text": 70, "public.edges_uuid": 40}
SECRET_ROWS = 40
STORED_IDS = "public.stored_ids"
# Its column of each id type's arrays, and how many times the large row
# repeats the ids (enough to be compressed and stored out of line).
STORED_COLUMNS = {"bigint": "b", "integer": "i", "text": "t", "uuid": "u"}
STORED_REPEATS = 400

# Node ids by id type, as SQL literals of that type.
BIGINT_IDS = tuple(range(1, 41)) + (1_000_000_000_000,)
INTEGER_IDS = tuple(range(1, 31))
TEXT_IDS = ("a", "b", "c", "d", "ñandú", "with space", "it's", "", "x" * 200, "0", "NULL", "𝔘nicode",
            *(f"n{i}" for i in range(20)))
UUID_IDS = tuple(str(uuid.UUID(int=(i * 0x9E3779B97F4A7C15) % (1 << 128))) for i in range(1, 21))


def literal(text):
    """`text` as a SQL string literal."""
    return "'" + text.replace("'", "''") + "'"


NODES = {
    "bigint": tuple(f"{i}::bigint" for i in (*BIGINT_IDS, 0, 3)),
    "integer": tuple(f"{i}::integer" for i in INTEGER_IDS),
    "text": tuple(f"{literal(i)}::text" for i in TEXT_IDS),
    "uuid": tuple(f"{literal(i)}::uuid" for i in UUID_IDS),
}


def _rows(values):
    return ", ".join(f"({', '.join(row)})" for row in values)


def _edges(rng, ids, count, literal_of, weighted=True, related=False, dangling=()):
    pool = list(ids) + list(dangling)
    rows = []
    for _ in range(count):
        row = [literal_of(rng.choice(pool)), literal_of(rng.choice(pool))]
        if related:
            row.append(literal(rng.choice(RELATIONS)))
        if weighted:
            row.append(rng.choice(("0", "0.5", "1", "2.5", "10")))
        rows.append(row)
    return _rows(rows)


def source_sql():
    """The superuser's setup: roles, source tables and their grants, the
    secret, the canary and the hostile schema."""
    rng = random.Random(SEED)
    text = literal
    as_uuid = lambda i: f"{literal(i)}::uuid"  # noqa: E731: one use, beside its siblings
    return f"""
CREATE ROLE {OWNER} LOGIN;
CREATE ROLE {READER} LOGIN;
CREATE ROLE {STRANGER} LOGIN;
CREATE TABLE public.nodes8 (id bigint);
INSERT INTO public.nodes8 VALUES {_rows([[str(i)] for i in BIGINT_IDS])};
CREATE TABLE public.edges8 (src bigint, dst bigint, rel text, w float8);
INSERT INTO public.edges8 VALUES {_edges(rng, BIGINT_IDS[:40], EDGE_ROWS['public.edges8'], str, related=True, dangling=(41, 42))};
CREATE TABLE public.nodes4 (id integer);
INSERT INTO public.nodes4 VALUES {_rows([[str(i)] for i in INTEGER_IDS])};
CREATE TABLE public.edges4 (src integer, dst integer, w float8);
INSERT INTO public.edges4 VALUES {_edges(rng, INTEGER_IDS, EDGE_ROWS['public.edges4'], str)};
CREATE TABLE public.nodes_text (id text);
INSERT INTO public.nodes_text VALUES {_rows([[text(i)] for i in TEXT_IDS])};
CREATE TABLE public.edges_text (src text, dst text, w float8);
INSERT INTO public.edges_text VALUES {_edges(rng, TEXT_IDS, EDGE_ROWS['public.edges_text'], text, dangling=("gone",))};
CREATE TABLE public.nodes_uuid (id uuid);
INSERT INTO public.nodes_uuid VALUES {_rows([[as_uuid(i)] for i in UUID_IDS])};
CREATE TABLE public.edges_uuid (src uuid, dst uuid);
INSERT INTO public.edges_uuid VALUES {_edges(rng, UUID_IDS, EDGE_ROWS['public.edges_uuid'], as_uuid, weighted=False)};
GRANT SELECT ON public.nodes8, public.edges8, public.nodes4, public.edges4, public.nodes_text,
    public.edges_text, public.nodes_uuid, public.edges_uuid TO {OWNER}, {READER};
CREATE TABLE {STORED_IDS} (kind text, b bigint[], i integer[], t text[], u uuid[]);
INSERT INTO {STORED_IDS} VALUES
    ('short', {_short()}),
    ('large', {_large()});
GRANT SELECT ON {STORED_IDS} TO PUBLIC;
CREATE TABLE {SECRET} (id bigint, src bigint, dst bigint, w float8);
INSERT INTO {SECRET} SELECT i, i, i % 40 + 1, 1 FROM pg_catalog.generate_series(1, {SECRET_ROWS}) i;
CREATE TABLE public.canary (note text);
GRANT SELECT, INSERT ON public.canary TO PUBLIC;
CREATE FUNCTION public.scribble() RETURNS bigint LANGUAGE sql VOLATILE
    AS $f$INSERT INTO public.canary VALUES ('scribbled') RETURNING 1::bigint$f$;
CREATE SCHEMA hostile AUTHORIZATION {STRANGER};
GRANT USAGE ON SCHEMA hostile TO PUBLIC;
SET ROLE {STRANGER};
{_hostile_sql()}
RESET ROLE;
"""


def _short():
    """The short row's arrays: two ids of each type, small enough for a
    one-byte header."""
    return ", ".join(f"ARRAY[{', '.join(NODES[t][:2])}]" for t in STORED_COLUMNS)


def _large():
    """The large row's arrays: every id of each type, repeated."""
    return ", ".join(f"pg_catalog.array_cat(ARRAY[{', '.join(NODES[t])}], pg_catalog.array_fill({NODES[t][-1]}, "
                     f"ARRAY[{STORED_REPEATS * len(NODES[t])}]))" for t in STORED_COLUMNS)


def _hostile_sql():
    operators = []
    for name, (left, right, result, body) in {
        "eq_text": ("text", "text", "boolean", "$1 OPERATOR(pg_catalog.=) $2"),
        "eq_bigint": ("bigint", "bigint", "boolean", "$1 OPERATOR(pg_catalog.=) $2"),
        "eq_integer": ("integer", "integer", "boolean", "$1 OPERATOR(pg_catalog.=) $2"),
        "eq_oid": ("oid", "oid", "boolean", "$1 OPERATOR(pg_catalog.=) $2"),
        "cat_text": ("text", "text", "text", "$1 OPERATOR(pg_catalog.||) $2"),
    }.items():
        symbol = "=" if name.startswith("eq") else "||"
        operators.append(f"""
CREATE FUNCTION hostile.{name}({left}, {right}) RETURNS {result} LANGUAGE plpgsql AS $f$
BEGIN
    PERFORM hostile.escalated();
    RETURN {body};
END $f$;
CREATE OPERATOR hostile.{symbol} (LEFTARG = {left}, RIGHTARG = {right}, FUNCTION = hostile.{name});""")
    return f"""
CREATE FUNCTION hostile.escalated() RETURNS boolean LANGUAGE plpgsql AS $f$
BEGIN
    IF (SELECT r.rolsuper FROM pg_catalog.pg_roles r WHERE r.rolname OPERATOR(pg_catalog.=) current_user)
       AND NOT (SELECT r.rolsuper FROM pg_catalog.pg_roles r WHERE r.rolname OPERATOR(pg_catalog.=) session_user)
    THEN
        RAISE EXCEPTION 'a hostile operator ran as % for %', current_user, session_user
            USING ERRCODE = '{ESCALATED}';
    END IF;
    RETURN true;
END $f$;""" + "".join(operators)


# Each graph, as its owner builds it (in this order: gq is contracted from
# g8). A run builds a graph again this way once it has been dropped.
BUILDS = {
    "g8": ("SELECT warren.create_graph('g8', 'SELECT id FROM public.nodes8', "
           "'SELECT src, dst, rel, w FROM public.edges8', weighted => true, has_relations => true, "
           "retain_versions => 3)",
           "SELECT warren.refresh('g8')",
           "SELECT warren.refresh('g8')",
           "SELECT warren.refresh('g8', promote => false, label => 'staged')",
           "SELECT warren.vector_const('g8', 'ones', 1)",
           "SELECT warren.pagerank('g8', 'pr')",
           f"SELECT warren.grant('g8', '{READER}')",
           "SELECT warren.set_vector_quota('g8', 1000000)"),
    "g4": ("SELECT warren.create_graph('g4', 'SELECT id FROM public.nodes4', 'SELECT src, dst FROM public.edges4', "
           "directed => false, build_reverse => false)",
           "SELECT warren.refresh('g4')",
           "SELECT warren.vector_const('g4', 'ones', 1)"),
    "gt": ("SELECT warren.create_graph('gt', 'SELECT id FROM public.nodes_text', "
           "'SELECT src, dst, w FROM public.edges_text', weighted => true, store_merged => false, "
           "node_order => 'locality')",
           "SELECT warren.refresh('gt')",
           "SELECT warren.vector_const('gt', 'ones', 1)",
           f"SELECT warren.grant('gt', '{READER}')"),
    "gu": ("SELECT warren.create_graph('gu', 'SELECT id FROM public.nodes_uuid', 'SELECT src, dst FROM public.edges_uuid')",
           "SELECT warren.refresh('gu')",
           "SELECT warren.vector_const('gu', 'ones', 1)"),
    "gq": ("SELECT warren.label_propagation('g8', 'lbl')",
           "SELECT warren.contract('g8', 'lbl', 'gq')",
           "SELECT warren.vector_const('gq', 'ones', 1)",
           f"SELECT warren.grant('gq', '{READER}')"),
}
READS = ("SELECT warren.vector_const('g8', 'mine', 2)",)


def steps():
    """The fixture as (role, statement) pairs, in order."""
    return [(SUPERUSER, source_sql()), *((OWNER, sql) for build in BUILDS.values() for sql in build),
            *((READER, sql) for sql in READS)]
