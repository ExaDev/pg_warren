"""A.3, entity resolution by neighbourhood accumulation (tier 2), at about
10M nodes: the fixture's structure (`bench/fixtures/entity_resolution.py`)
generated in SQL.

- 1.45M true entities with 1–5 records each (uniform, so 4.35M records;
  the fixture has 3.33 a entity), from 3 sources; one identifier of each
  of 4 kinds per entity, carried by each of its records with a coverage
  drawn per (source, kind) in 0.4–0.9: about 5.6M identifiers and 11M
  links, so about 10M nodes.
- 2% of identifiers also carried by one record of an unrelated entity
  (the shared office phone).

The program is the one `test/sql/a3_entity_resolution.sql` runs, with the
tables it keeps cut to what fits the disk beside the graph: links are
kept once (a < b), a table is dropped once read, and the kept links are a
table of their own rather than an UPDATE of every link. The oracle for
each superstep (the SQL join and `array_agg`) runs on a sample of records
and identifiers (ids divisible by `sample`, about 0.1%)."""

from .plan import Step, UseCase, at_least, at_most, equals

ENTITIES = 1_450_000
KINDS = ("phone", "email", "address", "card")
SAMPLE = 1000  # the oracle's sample: ids divisible by this, at full scale


def sizes(scale):
    entities = ENTITIES // scale
    records = entities * 3
    identifiers = round(entities * 4 * 0.96)
    return {"entities": entities, "records": records, "nodes": records + identifiers,
            "edges": round(records * 4 * 0.65 * 1.02), "sample_modulus": max(1, SAMPLE // scale)}


def generate(entities):
    slots = 5 * entities  # record slots: entity e's records are slots 5e .. 5e + 4, shuffled onto ids
    first = slots + 1  # identifier ids: first + 4e + kind
    kind = f"(id - {first}) % 4"
    entity = f"(id - {first}) / 4"
    return (
        f"""CREATE TABLE records AS
SELECT shuffle(5 * e + j, {slots}) AS id, (1 + pick(5 * e + j, 2, 3))::int AS source, e AS truth_entity
FROM generate_series(0::bigint, {entities - 1}) e, generate_series(0, pick(e, 1, 5)) j""",
        f"""CREATE TABLE record_identifiers AS
SELECT r.id AS record_id, {first} + 4 * r.truth_entity + k AS identifier_id
FROM records r, generate_series(0, 3) k
WHERE draw(4 * r.id + k, 4) < 0.4 + 0.5 * draw(4 * r.source + k, 3)""",
        # The shared identifiers: each also carried by an unrelated entity's first record.
        f"""INSERT INTO record_identifiers
SELECT shuffle(5 * other, {slots}), id FROM (
  SELECT DISTINCT identifier_id AS id, pick(identifier_id, 6, {entities}) AS other FROM record_identifiers
) i WHERE draw(id, 5) < 0.02 AND other <> {entity}""",
        f"""CREATE FUNCTION ident_kind(id bigint) RETURNS text IMMUTABLE LANGUAGE sql
RETURN (ARRAY{list(KINDS)})[1 + {kind}]""",
        f"""CREATE FUNCTION ident_value(id bigint) RETURNS text IMMUTABLE LANGUAGE sql
RETURN CASE {kind} WHEN 0 THEN '+44 7700 ' || {entity} WHEN 1 THEN 'person' || {entity} || '@example.org'
                   WHEN 2 THEN {entity} || ' Example Road' ELSE 'card ' || {entity} END""",
        """CREATE VIEW identifiers AS
SELECT id, ident_kind(id) AS kind, ident_value(id) AS value
FROM (SELECT DISTINCT identifier_id AS id FROM record_identifiers) i""",
        "CREATE VIEW er_nodes AS SELECT id FROM records UNION ALL SELECT id FROM identifiers",
        "ANALYZE",
    )


def supersteps(m):
    """The three supersteps, each with its oracle on the sample."""
    distinct_links = "(SELECT DISTINCT record_id, identifier_id FROM record_identifiers WHERE {where}) ri"
    return (
        Step("superstep 1", "CREATE TABLE idents AS SELECT node_id AS record_id, value AS idents "
                            "FROM warren.gather('er', $$SELECT id, kind || ':' || value FROM identifiers$$, "
                            "NULL::bigint, NULL::text[])", target=20),
        Step("sample dense ids",
             f"CREATE TABLE dense AS SELECT node_id, dense_id FROM warren.id_map('er', NULL::bigint) "
             f"WHERE node_id IN (SELECT identifier_id FROM record_identifiers WHERE record_id % {m} = 0) "
             f"OR node_id IN (SELECT record_id FROM record_identifiers WHERE identifier_id % {m} = 0); "
             f"CREATE INDEX ON dense (node_id); ANALYZE dense"),
        Step("superstep 1 oracle", f"""WITH warren AS (SELECT * FROM idents WHERE record_id % {m} = 0),
oracle AS (SELECT ri.record_id, array_agg(ident_kind(ri.identifier_id) || ':' || ident_value(ri.identifier_id)
                                          ORDER BY d.dense_id) AS idents
           FROM {distinct_links.format(where=f'record_id % {m} = 0')}
           JOIN dense d ON d.node_id = ri.identifier_id GROUP BY ri.record_id)
SELECT (SELECT count(*) FROM ((TABLE warren EXCEPT TABLE oracle) UNION ALL (TABLE oracle EXCEPT TABLE warren)) x)
       || '/' || (SELECT count(*) FROM oracle)""",
             kind="check", expect=lambda v: v is not None and v.startswith("0/") and int(v[2:]) > 0,
             claim="superstep 1: gather collect equals the SQL join and array_agg (sampled records)"),
        Step("superstep 2", "CREATE TABLE holders AS SELECT node_id AS identifier_id, value AS records "
                            "FROM warren.gather('er', 'SELECT id, id FROM records', NULL::bigint, NULL::bigint[])",
             target=20),
        Step("superstep 2 oracle", f"""WITH warren AS (SELECT * FROM holders WHERE identifier_id % {m} = 0),
oracle AS (SELECT ri.identifier_id, array_agg(ri.record_id ORDER BY d.dense_id) AS records
           FROM {distinct_links.format(where=f'identifier_id % {m} = 0')}
           JOIN dense d ON d.node_id = ri.record_id GROUP BY ri.identifier_id)
SELECT (SELECT count(*) FROM ((TABLE warren EXCEPT TABLE oracle) UNION ALL (TABLE oracle EXCEPT TABLE warren)) x)
       || '/' || (SELECT count(*) FROM oracle)""",
             kind="check", expect=lambda v: v is not None and v.startswith("0/") and int(v[2:]) > 0,
             claim="superstep 2: gather collect equals the SQL join and array_agg (sampled identifiers)"),
        Step("superstep 3", "CREATE TABLE neighbourhood AS SELECT node_id AS record_id, value AS records "
                            "FROM warren.gather('er', 'SELECT identifier_id, unnest(records) FROM holders', "
                            "NULL::bigint, NULL::bigint[])", target=20),
        Step("superstep 3 oracle", f"""WITH warren AS (SELECT * FROM neighbourhood WHERE record_id % {m} = 0),
oracle AS (SELECT ri.record_id, array_agg(h.r ORDER BY d.dense_id, h.ord) AS records
           FROM {distinct_links.format(where=f'record_id % {m} = 0')}
           JOIN holders ON holders.identifier_id = ri.identifier_id
           CROSS JOIN LATERAL unnest(holders.records) WITH ORDINALITY h(r, ord)
           JOIN dense d ON d.node_id = ri.identifier_id GROUP BY ri.record_id)
SELECT (SELECT count(*) FROM ((TABLE warren EXCEPT TABLE oracle) UNION ALL (TABLE oracle EXCEPT TABLE warren)) x)
       || '/' || (SELECT count(*) FROM oracle)""",
             kind="check", expect=lambda v: v is not None and v.startswith("0/") and int(v[2:]) > 0,
             claim="superstep 3: gather collect equals the SQL join and array_agg (sampled records)"),
    )


TRUST = (
    Step("carried", "CREATE TABLE carried AS SELECT record_id, split_part(x, ':', 1) AS kind, x "
                    "FROM idents, unnest(idents) x; DROP TABLE idents; CREATE INDEX ON carried (record_id)"),
    Step("links", "CREATE TABLE links AS SELECT DISTINCT n.record_id AS a, r AS b FROM neighbourhood n, "
                  "unnest(n.records) r WHERE r > n.record_id; DROP TABLE neighbourhood; ANALYZE links, carried"),
    Step("conflicts", """CREATE TABLE conflicts AS
SELECT DISTINCT a, b FROM (
  SELECT l.a, l.b FROM links l JOIN carried ca ON ca.record_id = l.a JOIN carried cb ON cb.record_id = l.b AND cb.kind = ca.kind
  GROUP BY l.a, l.b, ca.kind HAVING bool_and(ca.x <> cb.x)) c; DROP TABLE carried"""),
    Step("held", "CREATE TABLE held AS SELECT identifier_id, r AS record_id FROM holders, unnest(records) r; "
                 "ANALYZE held, conflicts"),
    # Before holders is dropped: a superstep over the compute memory cap.
    Step("lower compute memory", "SET warren.max_compute_memory = '64MB'"),
    Step("compute memory cap", "SELECT sqlstate_of($$SELECT count(*) FROM "
                               "warren.gather('er', 'SELECT identifier_id, unnest(records) FROM holders', NULL::bigint, "
                               "NULL::bigint[])$$)", kind="check", expect=equals("53400"),
         claim="with warren.max_compute_memory at 64MB a superstep fails with 53400 rather than OOMing"),
    Step("reset compute memory", "RESET warren.max_compute_memory; DROP TABLE holders"),
    # Conflicts are of pairs a < b, as the links are; a pair of holders is looked up in that order.
    Step("untrusted", "CREATE TABLE untrusted AS SELECT DISTINCT h1.identifier_id FROM held h1 "
                      "JOIN held h2 USING (identifier_id) JOIN conflicts c ON (c.a, c.b) = (h1.record_id, h2.record_id)"),
    Step("trusted links", "CREATE TABLE trusted AS SELECT DISTINCT h1.record_id AS a, h2.record_id AS b "
                          "FROM held h1 JOIN held h2 USING (identifier_id) WHERE h1.record_id < h2.record_id "
                          "AND identifier_id NOT IN (TABLE untrusted); DROP TABLE held"),
    Step("untrusted identifiers", "SELECT count(*) FROM untrusted", kind="check", expect=at_least(1),
         claim="the trust rule finds untrusted identifiers"),
)

QUALITY = (
    """CREATE FUNCTION resolve(name text, kept text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  PERFORM warren.create_graph(name, 'SELECT id FROM records', format('SELECT a, b FROM %I', kept), directed := false);
  PERFORM warren.refresh(name);
END $$""",
    "CREATE FUNCTION pairs(n bigint) RETURNS numeric LANGUAGE sql AS 'SELECT n * (n - 1) / 2.0'",
    """CREATE FUNCTION quality(name text, OUT pair_precision numeric, OUT pair_recall numeric) LANGUAGE sql AS $$
  WITH resolved AS (SELECT v.node_id, v.value::bigint AS entity, r.truth_entity
                    FROM warren.vector_values(name, 'entity', NULL::bigint) v JOIN records r ON r.id = v.node_id),
  both_ AS (SELECT sum(pairs(c)) AS n FROM (SELECT count(*) c FROM resolved GROUP BY entity, truth_entity) x),
  predicted AS (SELECT sum(pairs(c)) AS n FROM (SELECT count(*) c FROM resolved GROUP BY entity) x),
  actual AS (SELECT sum(pairs(c)) AS n FROM (SELECT count(*) c FROM resolved GROUP BY truth_entity) x)
  SELECT both_.n / predicted.n, both_.n / actual.n FROM both_, predicted, actual
$$""",
)


def use_case(scale):
    s = sizes(scale)
    steps = (
        Step("generate", ";\n".join(generate(s["entities"]))),
        Step("graph", "SELECT warren.create_graph('er', 'SELECT id FROM er_nodes', "
                      "'SELECT record_id, identifier_id FROM record_identifiers', directed := false)"),
        Step("refresh", "SELECT warren.refresh('er')"),
        Step("version", "SELECT warren.info('er') -> 'versions' -> 0", kind="measure", key="version"),
        # Superstep 1's time without the version's first load; its message
        # query (two SQL functions an identifier) on its own; and the same
        # messages as bigint, which take the fixed-width path where text
        # takes the binary send and receive (DESIGN.md §6.4).
        Step("prewarm", "SELECT warren.prewarm('er')"),
        Step("identifier messages", "SELECT count(*) FROM (SELECT id, kind || ':' || value FROM identifiers) m"),
        Step("superstep 1 as bigint", "CREATE TABLE idents_bigint AS SELECT node_id, value "
                                      "FROM warren.gather('er', 'SELECT id, id FROM identifiers', "
                                      "NULL::bigint, NULL::bigint[]); DROP TABLE idents_bigint"),
        *supersteps(s["sample_modulus"]),
        Step("drop sample dense ids", "DROP TABLE dense"),
        *TRUST,
        Step("quality functions", ";\n".join(QUALITY)),
        Step("trusted graph", "SELECT resolve('trusted', 'trusted')"),
        Step("trusted components", "SELECT warren.components_vector('trusted', 'entity')", target=30),
        Step("trusted quality", "SELECT pair_precision || '/' || pair_recall FROM quality('trusted')",
             kind="measure", key="trusted"),
        Step("trusted precision", "SELECT split_part('@@trusted@@', '/', 1)", kind="check", expect=at_least(0.98),
             claim="pairwise precision ≥ 0.98 after the trust rules"),
        Step("trusted recall", "SELECT split_part('@@trusted@@', '/', 2)", kind="check", expect=at_least(0.95),
             claim="pairwise recall ≥ 0.95 after the trust rules"),
        Step("drop trusted graph", "SELECT warren.drop_graph('trusted'); DROP TABLE trusted; "
                                   "VACUUM warren.segments, warren.vector_chunks"),
        Step("untrusted graph", "SELECT resolve('untrusted', 'links')"),
        Step("untrusted components", "SELECT warren.components_vector('untrusted', 'entity')"),
        Step("untrusted quality", "SELECT pair_precision || '/' || pair_recall FROM quality('untrusted')",
             kind="measure", key="untrusted"),
        Step("untrusted precision", "SELECT split_part('@@untrusted@@', '/', 1)", kind="check", expect=at_most(0.9),
             claim="without the trust rules precision drops below 0.9"),
    )
    return UseCase("a3", "A.3", s, steps, settings=("SET temp_file_limit = '3GB'",))
