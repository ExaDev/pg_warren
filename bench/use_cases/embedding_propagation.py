"""A.5, embedding propagation (tier 1, matrices), at 10M entities: the
block model of A.2 (20 communities, 4 edges inside and 1 anywhere per
entity, about 50M undirected edges) with each edge's relation drawn from
`related_to`, `part_of` and `mentions`, generated in SQL.

The program is A.5's, its embeddings in a `real[]` column (pgvector is not
installed on the benchmark machine; `real[]` is what A.5 casts it to) of
width 8: A.5's d = 768 is 31 GB a matrix at 10M rows, and the program
persists three matrices beside the table and its rewrite, which at 8 is
what fits the disk. `aggregate_neighbours` is also timed at width 32 from a
transient matrix, to show how its time grows with d. The SQL `AVG` oracle
and the cosine similarities run on samples."""

from .draws import centroids, embedding
from .plan import Step, UseCase, at_most, equals, is_true

ENTITIES = 10_000_000
COMMUNITIES = 20
INSIDE, ANYWHERE = 4, 1
RELATIONS = ("related_to", "part_of", "mentions")
D = 8
WIDE = 32  # the width aggregate_neighbours is also timed at
NOISE = 0.5
SAMPLE = 1000  # the oracle's sample: ids divisible by this, at full scale
PAIRS = 100_000  # sampled pairs for each cosine similarity
SMOOTH = "0.7 * emb + 0.3 * COALESCE(NULLIF(emb_nbr, 'NaN'), emb)"


def sizes(scale):
    n = ENTITIES // scale
    return {"nodes": n, "edges": n * (INSIDE + ANYWHERE), "communities": COMMUNITIES,
            "community_size": n // COMMUNITIES, "embedding_dim": D, "wide_dim": WIDE,
            "sample_modulus": max(1, SAMPLE // scale), "pairs": min(PAIRS, n)}


def generate(n, size):
    return (
        centroids("centroids", COMMUNITIES, D),
        centroids("wide_centroids", COMMUNITIES, WIDE),
        f"""CREATE VIEW kg_edges AS
SELECT n AS src, CASE WHEN t < {INSIDE} THEN (n - 1) / {size} * {size} + 1 + pick(8 * n + t, 31, {size})
                      ELSE 1 + pick(8 * n + t, 32, {n}) END AS dst,
       (ARRAY{list(RELATIONS)})[1 + pick(8 * n + t, 33, 3)] AS rel
FROM generate_series(1::bigint, {n}) n, generate_series(0, {INSIDE + ANYWHERE - 1}) t""",
        """CREATE FUNCTION cosine(x real[], y real[]) RETURNS float8 IMMUTABLE LANGUAGE sql
RETURN (SELECT sum(a * b) / sqrt(sum(a * a) * sum(b * b)) FROM unnest(x, y) u(a, b))""",
        "ANALYZE centroids, wide_centroids",
    )


def entities_sql(n, size):
    return (f"CREATE TABLE entities AS SELECT id, {embedding(D, NOISE, 'id', 'ce.centroid')} "
            f"AS embedding, NULL::real[] AS embedding_smooth FROM generate_series(1::bigint, {n}) id "
            f"JOIN centroids ce ON ce.c = (id - 1) / {size}")


def cosine_sql(n, size, pairs, within, column):
    """Mean cosine similarity of `pairs` sampled pairs, inside one community or across two."""
    other = (f"(i - 1) / {size} * {size} + 1 + pick(i, 81, {size})" if within
             else f"1 + ((i - 1) / {size} + 1 + pick(i, 82, {COMMUNITIES - 1})) % {COMMUNITIES} * {size} "
                  f"+ pick(i, 83, {size})")
    return (f"SELECT avg(cosine(a.{column}, b.{column})) FROM (SELECT 1 + pick(p, 80, {n}) AS i FROM "
            f"generate_series(1, {pairs}) p) s JOIN entities a ON a.id = s.i JOIN entities b ON b.id = {other}")


def use_case(scale):
    s = sizes(scale)
    n, size, m, pairs = s["nodes"], s["community_size"], s["sample_modulus"], s["pairs"]
    rels = "ARRAY['related_to', 'part_of']"
    wide = (f"SELECT id, {embedding(WIDE, NOISE, 'id', 'ce.centroid')} FROM generate_series(1::bigint, {n}) id "
            f"JOIN wide_centroids ce ON ce.c = (id - 1) / {size}")
    steps = (
        Step("generate", ";\n".join(generate(n, size))),
        Step("graph", "SELECT warren.create_graph('kg', 'SELECT id FROM generate_series(1::bigint, "
                      f"{n}) id', 'SELECT src, dst, rel FROM kg_edges', directed := false, has_relations := true)"),
        Step("refresh", "SELECT warren.refresh('kg')"),
        Step("version", "SELECT warren.info('kg') -> 'versions' -> 0", kind="measure", key="version"),
        # aggregate_neighbours at width 32, from a transient matrix, before the table takes the disk.
        Step(f"embeddings d={WIDE}", f"SELECT warren.vector_from_query('kg', 'wide', $q${wide}$q$, 'f32', "
                                     f"persist := false)"),
        Step(f"aggregate_neighbours d={WIDE}", f"SELECT warren.aggregate_neighbours('kg', 'wide', 'wide_nbr', "
                                               f"agg := 'mean', rels := {rels})", target=300),
        Step(f"drop d={WIDE}", "SELECT warren.drop_vector('kg', 'wide', NULL, true), "
                               "warren.drop_vector('kg', 'wide_nbr', NULL, true); VACUUM warren.vector_chunks"),
        Step("entities", entities_sql(n, size) + "; CREATE INDEX ON entities (id); ANALYZE entities"),
        # A.5's program.
        Step("embeddings from the table", "SELECT warren.vector_from_query('kg', 'emb', "
                                          "'SELECT id, embedding::float4[] FROM entities', dtype := 'f32')", target=300),
        Step("aggregate_neighbours", f"SELECT warren.aggregate_neighbours('kg', 'emb', 'emb_nbr', agg := 'mean', "
                                     f"rels := {rels})", target=300),
        # A node with no neighbour in the relations has a NaN mean (SQL AVG
        # of nothing, DESIGN.md §6.7); the program keeps its own row there.
        Step("rows without neighbours", "SELECT count(*) FROM warren.matrix_rows('kg', 'emb_nbr', NULL::bigint) "
                                        "WHERE row[1] = 'NaN'", kind="measure", key="without_neighbours"),
        Step("smooth", f"SELECT warren.vector_map('kg', 'emb_smooth', $${SMOOTH}$$)"),
        Step("compiled", f"SELECT warren.explain_map('kg', $${SMOOTH}$$) ->> 'path'", kind="check",
             expect=equals("compiled"), claim="the smoothing expression takes the compiled path"),
        Step("write back", "UPDATE entities e SET embedding_smooth = r.row::real[] "
                           "FROM warren.matrix_rows('kg', 'emb_smooth', NULL::bigint) r WHERE r.node_id = e.id",
             target=300),
        Step("every row written", "SELECT count(*) FROM entities WHERE embedding_smooth IS NULL", kind="check",
             expect=equals("0"), claim="the round trip writes every entity's smoothed row back"),
        # The SQL AVG oracle on a sample of nodes.
        Step("sample neighbours", f"""CREATE TABLE sample_neighbours AS SELECT DISTINCT a, b FROM (
  SELECT src AS a, dst AS b FROM kg_edges WHERE rel IN ('related_to', 'part_of') AND src % {m} = 0
  UNION ALL SELECT dst, src FROM kg_edges WHERE rel IN ('related_to', 'part_of') AND dst % {m} = 0) u"""),
        Step("aggregate equals AVG", f"""WITH sql_mean AS (
  SELECT n.a AS node_id, k, avg(e.embedding[k]) AS value
  FROM sample_neighbours n JOIN entities e ON e.id = n.b, generate_series(1, {D}) k GROUP BY n.a, k)
SELECT max(abs(r.row[s.k] - s.value)) || '/' || count(DISTINCT node_id)
FROM warren.matrix_rows('kg', 'emb_nbr', NULL::bigint) r JOIN sql_mean s USING (node_id)
WHERE r.node_id % {m} = 0""",
             kind="check", expect=lambda v: v is not None and float(v.split("/")[0]) <= 1e-5,
             claim="aggregate_neighbours equals SQL AVG over neighbour rows within 1e-5 (sampled nodes)"),
        # Cosine similarities inside and across communities, before and after.
        Step("within before", cosine_sql(n, size, pairs, True, "embedding"), kind="measure", key="within_before"),
        Step("within after", cosine_sql(n, size, pairs, True, "embedding_smooth"), kind="measure",
             key="within_after"),
        Step("between before", cosine_sql(n, size, pairs, False, "embedding"), kind="measure", key="between_before"),
        Step("between after", cosine_sql(n, size, pairs, False, "embedding_smooth"), kind="measure",
             key="between_after"),
        Step("within gains", "SELECT @@within_after@@ > @@within_before@@", kind="check", expect=is_true,
             claim="mean within-community cosine similarity increases after smoothing"),
        Step("between bounded", "SELECT (@@between_after@@ - @@between_before@@) "
                                "/ (@@within_after@@ - @@within_before@@)", kind="check", expect=at_most(0.1),
             claim="between-community similarity grows by at most 10% of the within-community gain"),
        # Broadcasting over width-d matrices, and refusing mismatched widths.
        Step("broadcast", "SELECT warren.vector_map('kg', 'shifted', '0.5 * emb + 1', persist := false)"),
        Step("broadcast is elementwise", f"""SELECT max(abs(r.row[k] - (0.5 * e.embedding[k] + 1)))
FROM warren.matrix_rows('kg', 'shifted', NULL::bigint) r JOIN entities e ON e.id = r.node_id, generate_series(1, {D}) k
WHERE r.node_id % {m} = 0""", kind="check", expect=at_most(1e-6),
             claim="vector_map broadcasts scalars over width-d rows"),
        Step("narrow matrix", f"""SELECT warren.vector_from_query('kg', 'narrow', 'SELECT id, ARRAY[1, 2]::float4[]
FROM generate_series(1::bigint, {n}) id', 'f32', persist := false)"""),
        Step("mismatched widths", "SELECT sqlstate_of($$SELECT warren.vector_map('kg', 'bad', 'emb + narrow', "
                                  "persist := false)$$)",
             kind="check", expect=lambda v: v not in (None, ""),
             claim="vector_map refuses mismatched widths with an error"),
    )
    return UseCase("a5", "A.5", s, steps, settings=("SET warren.max_cache_memory = '8GB'",))
