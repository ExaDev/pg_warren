"""A.4, typed multi-hop retrieval (`walk`), at about 10M nodes, keyed by
IRIs and then by bigint ids: the fixture's structure
(`bench/fixtures/citation.py`) generated in SQL.

- 8M papers; paper p cites 1–5 earlier papers drawn as
  `1 + (p - 1)·u³`, so citations pile onto old papers and in-degree is
  heavy-tailed (the fixture uses preferential attachment, which SQL draws
  cannot do in one pass); about 24M citations. Parallel citations are
  kept in the table and folded by the build.
- A root paper, the newest, citing 20 papers drawn uniformly (the fixture
  plants about 400 papers in the root's 1–3 hop neighbourhood).
- 1.9M people with 1–6 papers each, 70% active; 100,000 institutions,
  15% in GB; one affiliation a person.

Checks: `walk` ends against a chain of recursive CTEs over the integer
tables (the oracle, mapped to IRIs), `count`, `paths` validated hop by
hop, pruning by the step-two filter (`last_stats`), the reversed walk
against its own CTEs, and a 10-hop first segment against its CTE (a
benchmark, not asserted)."""

from .plan import Step, UseCase, at_least, at_most, equals, is_true

PAPERS = 8_000_000
PEOPLE = 1_900_000
INSTITUTIONS = 100_000
ROOT_CITES = 20
CTE_TIMEOUT = "120s"


def sizes(scale):
    papers, people, institutions = PAPERS // scale, PEOPLE // scale, INSTITUTIONS // scale
    return {"papers": papers + 1, "people": people, "institutions": institutions,
            "nodes": papers + 1 + people + institutions, "edges": papers * 3 + people * 3.5 + people}


def generate(papers, people, institutions):
    root = papers + 1
    return (
        "CREATE FUNCTION iri(kind text, id bigint) RETURNS text IMMUTABLE LANGUAGE sql "
        "RETURN 'https://example.org/' || kind || '/' || id",
        "CREATE FUNCTION iri_id(iri text) RETURNS bigint IMMUTABLE LANGUAGE sql RETURN split_part(iri, '/', 5)::bigint",
        f"""CREATE TABLE citations AS
SELECT p::int4 AS citing, (1 + floor((p - 1) * power(draw(16 * p + t, 2), 3)))::int4 AS cited
FROM generate_series(2::bigint, {papers}) p, generate_series(0, pick(p, 1, 5)) t
UNION ALL SELECT {root}::int4, (1 + pick(t, 3, {papers}))::int4 FROM generate_series(0, {ROOT_CITES - 1}) t""",
        f"""CREATE TABLE people AS
SELECT {root} + q AS id, draw(q, 4) < 0.7 AS active FROM generate_series(1::bigint, {people}) q""",
        f"""CREATE TABLE institutions AS
SELECT {root + people} + i AS id, CASE WHEN draw(i, 5) < 0.15 THEN 'GB'
       ELSE (ARRAY['US', 'DE', 'FR', 'JP', 'CN', 'NL', 'CA'])[1 + pick(i, 9, 7)] END AS country
FROM generate_series(1::bigint, {institutions}) i""",
        f"""CREATE TABLE authorship AS
SELECT (1 + pick(8 * q + t, 7, {papers}))::int4 AS paper_id, ({root} + q)::int4 AS person_id
FROM generate_series(1::bigint, {people}) q, generate_series(0, pick(q, 6, 6)) t""",
        f"""CREATE TABLE affiliations AS
SELECT ({root} + q)::int4 AS person_id, ({root + people} + 1 + pick(q, 8, {institutions}))::int4 AS institution_id
FROM generate_series(1::bigint, {people}) q""",
        "CREATE INDEX ON citations (citing)",
        "CREATE INDEX ON authorship (paper_id)",
        "CREATE INDEX ON affiliations (person_id)",
        "ANALYZE",
    )


def graph_sql(name, ids, papers):
    """A.4's graph over the integer tables, its ids IRIs (`text`) or `bigint`."""
    if ids == "text":
        node, kinds = "iri('{kind}', {id})", True
    else:
        node, kinds = "{id}::bigint", False

    def n(kind, id):
        return node.format(kind=kind, id=id) if kinds else node.format(id=id)

    nodes = (f"SELECT {n('paper', 'id')} FROM generate_series(1::bigint, {papers + 1}) id "
             f"UNION ALL SELECT {n('person', 'id')} FROM people UNION ALL SELECT {n('institution', 'id')} FROM institutions")
    edges = (f"SELECT {n('paper', 'citing')}, {n('paper', 'cited')}, 'cites' FROM citations "
             f"UNION ALL SELECT {n('paper', 'paper_id')}, {n('person', 'person_id')}, 'authored_by' FROM authorship "
             f"UNION ALL SELECT {n('person', 'person_id')}, {n('institution', 'institution_id')}, 'affiliated_with' "
             f"FROM affiliations")
    return f"SELECT warren.create_graph('{name}', $n${nodes}$n$, $e${edges}$e$, has_relations := true)"


def steps_sql(table, ids, max_hops=3):
    """The filtered and unfiltered step arrays, in a table."""
    person, institution = ("iri('person', id)", "iri('institution', id)") if ids == "text" else ("id", "id")
    first = f"warren.s(ARRAY['cites'], 'out', min_hops := 1, max_hops := {max_hops})"
    return f"""CREATE TABLE {table} AS
SELECT ARRAY[{first},
             warren.s(ARRAY['authored_by'], 'out', ARRAY(SELECT {person} FROM people WHERE active)),
             warren.s(ARRAY['affiliated_with'], 'out', ARRAY(SELECT {institution} FROM institutions WHERE country = 'GB'))] AS filtered,
       ARRAY[{first},
             warren.s(ARRAY['authored_by'], 'out'),
             warren.s(ARRAY['affiliated_with'], 'out', ARRAY(SELECT {institution} FROM institutions WHERE country = 'GB'))] AS unfiltered"""


def oracle_sql(root, hops):
    """The institutions the walk reaches, by one recursive CTE per step over the integer tables."""
    return f"""WITH RECURSIVE near (id, depth) AS (
  SELECT {root}::int4, 0
  UNION
  SELECT c.cited, n.depth + 1 FROM near n JOIN citations c ON c.citing = n.id WHERE n.depth < {hops}),
authors AS (
  SELECT DISTINCT a.person_id AS id
  FROM (SELECT DISTINCT id FROM near WHERE depth >= 1) p
  JOIN authorship a ON a.paper_id = p.id JOIN people pe ON pe.id = a.person_id AND pe.active)
SELECT DISTINCT af.institution_id AS id
FROM authors a JOIN affiliations af ON af.person_id = a.id JOIN institutions i ON i.id = af.institution_id AND i.country = 'GB'"""


REVERSE_ORACLE = """WITH RECURSIVE authored AS (
  SELECT DISTINCT au.paper_id AS id FROM walked w
  JOIN affiliations af ON af.institution_id = iri_id(w.end_id)
  JOIN people pe ON pe.id = af.person_id AND pe.active
  JOIN authorship au ON au.person_id = af.person_id),
back (id, depth) AS (
  SELECT id, 0 FROM authored
  UNION
  SELECT c.citing, b.depth + 1 FROM back b JOIN citations c ON c.cited = b.id WHERE b.depth < 3)
SELECT DISTINCT id FROM back WHERE depth >= 1"""


def iri_steps(papers):
    root = f"iri('paper', {papers + 1})"
    walk = f"warren.walk('kg', ARRAY[{root}], (SELECT {{steps}} FROM steps){{options}})"
    ends = walk.format(steps="filtered", options="")
    return (
        Step("graph", graph_sql("kg", "text", papers)),
        Step("refresh", "SELECT warren.refresh('kg')"),
        Step("version", "SELECT warren.info('kg') -> 'versions' -> 0", kind="measure", key="version"),
        Step("steps", steps_sql("steps", "text")),
        Step("walk cold", f"CREATE TABLE walked AS SELECT end_id FROM {ends}"),
        Step("visited filtered", "SELECT warren.last_stats() ->> 'nodes_visited'", kind="measure",
             key="visited_filtered"),
        Step("walk warm", f"SELECT count(*) FROM {ends}", repeat=5, target=2),
        Step("root neighbourhood",
             f"SELECT count(*) FROM warren.walk('kg', ARRAY[{root}], ARRAY[warren.s(ARRAY['cites'], 'out', "
             f"min_hops := 1, max_hops := 3)])", kind="measure", key="root_neighbourhood"),
        Step("oracle", f"CREATE TABLE oracle AS SELECT iri('institution', id) AS id FROM ({oracle_sql(papers + 1, 3)}) o"),
        Step("ends found", "SELECT count(*) FROM walked", kind="check", expect=at_least(5),
             claim="the walk reaches several institutions"),
        Step("ends equal the oracle", "SELECT count(*) FROM ((TABLE walked EXCEPT TABLE oracle) "
                                      "UNION ALL (TABLE oracle EXCEPT TABLE walked)) x",
             kind="check", expect=equals("0"), claim="walk ends equal the chain of recursive CTEs over the tables"),
        Step("count", f"SELECT (SELECT count FROM {walk.format(steps='filtered', options=', mode := ' + chr(39) + 'count' + chr(39))}) "
                      f"= (SELECT count(*) FROM walked)", kind="check", expect=is_true,
             claim="mode 'count' equals the cardinality of 'ends'"),
        Step("paths", "CREATE TABLE paths AS SELECT end_id, path, rels FROM "
                      + walk.format(steps="filtered", options=", 'paths', distinct_ends := false, max_results := 1000")),
        Step("paths bounded", "SELECT count(*) FROM paths", kind="check", expect=lambda v: 1 <= int(v or 0) <= 1000,
             claim="'paths' with max_results 1000 returns 1 to 1000 rows"),
        Step("paths are edges", """SELECT count(*) FROM paths p, generate_subscripts(p.rels, 1) i
WHERE NOT CASE p.rels[i]
  WHEN 'cites' THEN EXISTS (SELECT FROM citations c WHERE c.citing = iri_id(p.path[i]) AND c.cited = iri_id(p.path[i + 1]))
  WHEN 'authored_by' THEN EXISTS (SELECT FROM authorship a WHERE a.paper_id = iri_id(p.path[i]) AND a.person_id = iri_id(p.path[i + 1]))
  ELSE EXISTS (SELECT FROM affiliations a WHERE a.person_id = iri_id(p.path[i]) AND a.institution_id = iri_id(p.path[i + 1]))
END""", kind="check", expect=equals("0"), claim="every hop of every path is an edge of its relation"),
        Step("paths pass the filters", """SELECT count(*) FROM paths p
WHERE NOT EXISTS (SELECT FROM people pe WHERE pe.id = iri_id(p.path[array_length(p.path, 1) - 1]) AND pe.active)
   OR NOT EXISTS (SELECT FROM institutions i WHERE i.id = iri_id(p.end_id) AND i.country = 'GB')
   OR p.end_id <> p.path[array_length(p.path, 1)]""", kind="check", expect=equals("0"),
             claim="every path's author is active and its end a GB institution"),
        Step("walk unfiltered", f"SELECT count(*) FROM {walk.format(steps='unfiltered', options='')}"),
        Step("filters prune", "SELECT (warren.last_stats() ->> 'nodes_visited')::bigint > @@visited_filtered@@",
             kind="check", expect=is_true, claim="dropping the step-two filter visits more nodes (filters prune)"),
        Step("steps 10 hops", steps_sql("steps10", "text", max_hops=10)),
        Step("walk 10 hops", f"SELECT count(*) FROM warren.walk('kg', ARRAY[{root}], (SELECT filtered FROM steps10))",
             repeat=3, target=1),
        Step("cte 10 hops", f"SET statement_timeout = '{CTE_TIMEOUT}'; SELECT count(*) FROM ({oracle_sql(papers + 1, 10)}) o",
             allowed_error="statement timeout"),
        Step("reset timeout", "RESET statement_timeout"),
        Step("10 hops agree", f"SELECT (SELECT count(*) FROM warren.walk('kg', ARRAY[{root}], "
                              f"(SELECT filtered FROM steps10))) >= (SELECT count(*) FROM walked)",
             kind="check", expect=is_true, claim="10 hops reach at least the institutions 3 hops reach"),
        Step("reverse index", "DROP INDEX citations_citing_idx; CREATE INDEX ON citations (cited); ANALYZE citations"),
        Step("walk reversed", "CREATE TABLE reversed AS SELECT end_id FROM warren.walk('kg', "
                              "ARRAY(SELECT end_id FROM walked), (SELECT filtered FROM steps), reverse := true)"),
        Step("reverse oracle", f"CREATE TABLE reverse_oracle AS SELECT iri('paper', id) AS end_id FROM ({REVERSE_ORACLE}) o"),
        Step("reversed reaches the root", f"SELECT {root} IN (SELECT end_id FROM reversed)", kind="check",
             expect=is_true, claim="the reversed walk reports the root among its starts"),
        Step("reversed equals its oracle", "SELECT count(*) || '/' || (SELECT count(*) FROM reversed) FROM "
                                           "((TABLE reversed EXCEPT TABLE reverse_oracle) "
                                           "UNION ALL (TABLE reverse_oracle EXCEPT TABLE reversed)) x",
             kind="check", expect=lambda v: v is not None and v.startswith("0/"),
             claim="reverse := true returns exactly the starts the reversed CTE chain finds"),
    )


def bigint_steps(papers):
    root = papers + 1
    return (
        Step("drop iri graph", "SELECT warren.drop_graph('kg'); VACUUM warren.segments, warren.vector_chunks; "
                               "DROP INDEX citations_cited_idx; CREATE INDEX ON citations (citing)"),
        Step("graph bigint", graph_sql("kg_int", "bigint", papers)),
        Step("refresh bigint", "SELECT warren.refresh('kg_int')"),
        Step("steps bigint", steps_sql("steps_int", "bigint")),
        Step("walk cold bigint", f"CREATE TABLE walked_int AS SELECT end_id FROM warren.walk('kg_int', "
                                 f"ARRAY[{root}::bigint], (SELECT filtered FROM steps_int))"),
        Step("walk warm bigint", f"SELECT count(*) FROM warren.walk('kg_int', ARRAY[{root}::bigint], "
                                 f"(SELECT filtered FROM steps_int))", repeat=5, target=2),
        Step("bigint ends equal the oracle", f"SELECT count(*) FROM ((SELECT end_id FROM walked_int EXCEPT "
                                             f"SELECT iri_id(id) FROM oracle) UNION ALL (SELECT iri_id(id) FROM oracle "
                                             f"EXCEPT SELECT end_id FROM walked_int)) x",
             kind="check", expect=equals("0"), claim="with bigint ids the walk ends equal the oracle too"),
        Step("cte 3 hops", f"SELECT count(*) FROM ({oracle_sql(root, 3)}) o", repeat=3),
    )


def use_case(scale):
    s = sizes(scale)
    papers = s["papers"] - 1
    steps = (
        Step("generate", ";\n".join(generate(papers, s["people"], s["institutions"]))),
        *iri_steps(papers),
        *bigint_steps(papers),
    )
    return UseCase("a4", "A.4", s, steps)
