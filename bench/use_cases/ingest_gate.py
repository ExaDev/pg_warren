"""A.2, corpus ingest gated by structural change, at 10M entities: the
fixture's structure (`bench/fixtures/evolving.py`) generated in SQL.

- Base graph: a block model of 10M entities in 20 communities of 500,000
  (k = 20, A.2's scale target, so the k smallest eigenvectors of the
  normalised Laplacian span the communities). Each entity draws 4 edges
  inside its community and 1 anywhere, so about 50M undirected edges
  (average degree 10, 81% inside). The base edges are a view over the
  draws: only the ingest is stored, so the disk holds the versions.
- Three ingest variants of 1M entities and 5 edges each (A.2's 10% and
  ~25,000 edges per 5,000): *volume* joins existing communities in the
  base proportions, *bridge* links only into communities 3 and 11,
  *novel* forms a community of its own (2% of its edges reach the base).
- A fourth, *dense bridge*: the bridge with 20 edges an entity, as
  `test/sql/a2_spectral.sql` has its ingest (A.2's 5, against a base
  degree of 10, links the two communities about as strongly as any two
  are linked already, so it does not merge them; the 1/100 and 1/1000
  scale runs agree).
- Embeddings of width 16 (A.2 has 64: four matrices of 10M × 64 f32 rows
  and the solves' bases do not fit 15 GB together) around unit community
  centroids, noise of expected norm 0.5.

The §8 workflow per variant: ingest in a transaction, staged refresh,
diff, eigen on the staged version (the base's is solved once, before),
distance('subspace') twice, neighbour-aggregated embeddings and
distance('embedding'), ROLLBACK. Then a staged version committed without
promote, a second solve of the base (determinism) and the shadow-schema
pattern. Eigenvectors are transient (persist := false)."""

from .draws import centroids, embedding
from .plan import Step, UseCase, at_least, at_most, equals, is_true

ENTITIES = 10_000_000
COMMUNITIES = 20
INSIDE, ANYWHERE = 4, 1  # each entity's edge draws
SHARE_INSIDE = 0.81  # the base's share of an entity's edges inside its community
INGEST_EDGES = {"volume": 5, "bridge": 5, "novel": 5, "dense_bridge": 20}  # per new entity
K = 20
D = 16
NOISE = 0.5
BRIDGE = (3, 11)
VARIANTS = tuple(INGEST_EDGES)
EIGEN = "'normalized_laplacian', 'smallest', vectors_prefix := '{prefix}', persist := false"


def sizes(scale):
    n = ENTITIES // scale
    return {"nodes": n, "edges": n * (INSIDE + ANYWHERE), "communities": COMMUNITIES,
            "community_size": n // COMMUNITIES, "ingest_nodes": n // 10, "ingest_edges": {v: n // 10 * e for v, e in INGEST_EDGES.items()},
            "k": K, "embedding_dim": D}


def generate(n, size, new):
    edge = "32 * u + t"
    return (
        centroids("centroids", COMMUNITIES, D),  # one more centroid, COMMUNITIES, for the novel community
        "CREATE TABLE nodes (id bigint, community int)",
        "CREATE TABLE edges (src bigint, dst bigint)",
        f"""CREATE VIEW base_edges AS
SELECT n AS src, CASE WHEN t < {INSIDE} THEN (n - 1) / {size} * {size} + 1 + pick(8 * n + t, 31, {size})
                      ELSE 1 + pick(8 * n + t, 32, {n}) END AS dst
FROM generate_series(1::bigint, {n}) n, generate_series(0, {INSIDE + ANYWHERE - 1}) t""",
        f"""CREATE FUNCTION ingest_nodes(variant text) RETURNS TABLE (id bigint, community int) LANGUAGE sql STABLE AS $$
SELECT {n} + u, CASE variant WHEN 'volume' THEN pick(u, 41, {COMMUNITIES})::int
                             WHEN 'novel' THEN {COMMUNITIES}
                             ELSE (ARRAY{list(BRIDGE)})[1 + pick(u, 42, 2)]
                             END
FROM generate_series(1::bigint, {new}) u $$""",
        f"""CREATE FUNCTION ingest_edges(variant text) RETURNS TABLE (src bigint, dst bigint) LANGUAGE sql STABLE AS $$
SELECT {n} + u, CASE variant
  WHEN 'volume' THEN CASE WHEN draw({edge}, 43) < {SHARE_INSIDE}
                          THEN pick(u, 41, {COMMUNITIES}) * {size} + 1 + pick({edge}, 44, {size})
                          ELSE 1 + pick({edge}, 45, {n}) END
  WHEN 'novel' THEN CASE WHEN draw({edge}, 48) < 0.02 THEN 1 + pick({edge}, 49, {n}) ELSE {n} + 1 + pick({edge}, 50, {new}) END
  ELSE (ARRAY{list(BRIDGE)})[1 + pick({edge}, 46, 2)] * {size} + 1 + pick({edge}, 47, {size}) END
FROM generate_series(1::bigint, {new}) u,
     generate_series(0, CASE variant WHEN 'dense_bridge' THEN {INGEST_EDGES['dense_bridge'] - 1} ELSE {INGEST_EDGES['volume'] - 1} END) t $$""",
        "ANALYZE centroids",
    )


def embeddings_query(n, size, staged):
    base = (f"SELECT id, {embedding(D, NOISE, 'id', 'ce.centroid')} FROM generate_series(1::bigint, {n}) id "
            f"JOIN centroids ce ON ce.c = (id - 1) / {size}")
    if not staged:
        return base
    return base + (f" UNION ALL SELECT g.id, {embedding(D, NOISE, 'g.id', 'ce.centroid')} FROM nodes g "
                   f"JOIN centroids ce ON ce.c = g.community")


def drop_eigenvectors(version, prefix="lap"):
    return (f"SELECT count(warren.drop_vector('kg', '{prefix}_' || i, {version}, true)) "
            f"FROM generate_series(1, {K}) i")


def variant_steps(v, n, size, new):
    staged = f"@@staged_{v}@@"
    distance = f"warren.distance('kg', @@base@@, {staged}, 'subspace', vectors_prefix := 'lap', k := {K})"
    return (
        Step(f"begin {v}", "BEGIN"),
        Step(f"ingest {v}", f"INSERT INTO nodes SELECT * FROM ingest_nodes('{v}'); "
                            f"INSERT INTO edges SELECT * FROM ingest_edges('{v}')"),
        Step(f"ingested edges {v}", "SELECT count(*) FROM edges", kind="measure", key=f"edges_{v}"),
        Step(f"staged refresh {v}", f"SELECT warren.refresh('kg', promote := false, label := 'corpus-{v}')",
             key=f"staged_{v}", target=480),
        Step(f"diff {v}", f"SELECT warren.diff('kg', @@base@@, {staged})", key=f"diff_{v}", target=30),
        Step(f"diff nodes {v}", f"SELECT '@@diff_{v}@@'::jsonb #>> '{{nodes,added}}'", kind="check",
             expect=equals(str(new)), claim=f"{v}: diff reports the ingested entities"),
        Step(f"diff edges {v}",
             f"SELECT abs(('@@diff_{v}@@'::jsonb #>> '{{all_relations,added}}')::float8 - 2 * @@edges_{v}@@) "
             f"/ (2 * @@edges_{v}@@)", kind="check", expect=at_most(0.05),
             claim=f"{v}: diff reports the ingested edges (each undirected edge both ways) within 5%"),
        Step(f"dense ids {v}",
             f"SELECT count(*) FILTER (WHERE b.dense_id <> s.dense_id) || '/' || count(*) "
             f"FROM warren.id_map('kg', NULL::bigint, @@base@@) b "
             f"JOIN warren.id_map('kg', NULL::bigint, {staged}) s USING (node_id)",
             kind="check", expect=equals(f"0/{n}"),
             claim=f"{v}: every pre-existing entity keeps its dense id in the staged version"),
        Step(f"staged eigen {v}", f"CREATE TABLE eigen_{v} AS SELECT * FROM warren.eigen('kg', {K}, "
                                  f"{EIGEN.format(prefix='lap')}, version := {staged})", target=600),
        Step(f"staged residual {v}", f"SELECT max(residual) FROM eigen_{v}", kind="measure", key=f"residual_{v}"),
        Step(f"staged eigenvalues {v}", "SELECT string_agg(round(eigenvalue::numeric, 6)::text, ',' ORDER BY i) "
                                        f"FROM eigen_{v}", kind="measure", key=f"eigenvalues_{v}"),
        Step(f"subspace {v}", f"SELECT {distance} ->> 'projection'", key=f"d_{v}", target=30),
        Step(f"subspace again {v}", f"SELECT ({distance} ->> 'projection') = '@@d_{v}@@'", kind="check",
             expect=is_true, claim=f"{v}: the same comparison twice gives the same result"),
        Step(f"staged embeddings {v}", f"SELECT warren.vector_from_query('kg', 'emb', "
                                       f"$q${embeddings_query(n, size, staged=True)}$q$, 'f32', persist := false, "
                                       f"version := {staged})"),
        Step(f"aggregate staged {v}", f"SELECT warren.aggregate_neighbours('kg', 'emb', 'agg', version := {staged})"),
        Step(f"embedding distance {v}",
             f"SELECT warren.distance('kg', @@base@@, {staged}, 'embedding', x := 'agg')", key=f"embedding_{v}"),
        Step(f"embedding drift {v}", f"SELECT '@@embedding_{v}@@'::jsonb ->> 'mean_l2'", kind="measure",
             key=f"e_{v}"),
        Step(f"drop staged transients {v}", drop_eigenvectors(staged) +
             f"; SELECT warren.drop_vector('kg', 'emb', {staged}, true)"),
        Step(f"rollback {v}", "ROLLBACK"),
        Step(f"rolled back {v}",
             f"SELECT (SELECT count(*) FROM warren.versions WHERE version_id = {staged}) "
             f"+ (SELECT count(*) FROM warren.segments WHERE version_id = {staged}) "
             f"+ (SELECT count(*) FROM warren.vectors WHERE version_id = {staged}) "
             f"+ (SELECT count(*) FROM nodes) + (SELECT count(*) FROM edges)",
             kind="check", expect=equals("0"),
             claim=f"{v}: after ROLLBACK no version, segment, vector or ingested row of the staged version remains"),
        Step(f"vacuum {v}", "VACUUM warren.segments, warren.vector_chunks, warren.vectors, warren.versions, nodes, "
                            "edges"),
    )


def use_case(scale):
    s = sizes(scale)
    n, size, new = s["nodes"], s["community_size"], s["ingest_nodes"]
    shadow_nodes = f"SELECT id FROM generate_series(1::bigint, {n}) id UNION ALL SELECT id FROM ingest_nodes(''bridge'')"
    shadow_edges = "SELECT src, dst FROM base_edges UNION ALL SELECT src, dst FROM ingest_edges(''bridge'')"
    steps = (
        Step("generate", ";\n".join(generate(n, size, new))),
        Step("graph", f"SELECT warren.create_graph('kg', 'SELECT id FROM generate_series(1::bigint, {n}) id "
                      f"UNION ALL SELECT id FROM nodes', 'SELECT src, dst FROM base_edges UNION ALL "
                      f"SELECT src, dst FROM edges', directed := false)"),
        Step("base refresh", "SELECT warren.refresh('kg')", key="base", target=480),
        Step("base version", "SELECT warren.info('kg') -> 'versions' -> 0", kind="measure", key="base_version"),
        Step("base eigen", f"CREATE TABLE eigen_base AS SELECT * FROM warren.eigen('kg', {K}, "
                           f"{EIGEN.format(prefix='lap')})", target=600),
        Step("base residual", "SELECT max(residual) FROM eigen_base", kind="measure", key="residual_base"),
        Step("base eigenvalues", "SELECT string_agg(round(eigenvalue::numeric, 6)::text, ',' ORDER BY i) "
                                 "FROM eigen_base", kind="measure", key="eigenvalues_base"),
        Step("base embeddings", f"SELECT warren.vector_from_query('kg', 'emb', "
                                f"$q${embeddings_query(n, size, staged=False)}$q$, 'f32', persist := false)"),
        Step("aggregate base", "SELECT warren.aggregate_neighbours('kg', 'emb', 'agg')"),
        Step("drop base embeddings", "SELECT warren.drop_vector('kg', 'emb', NULL, true)"),
        *(step for v in VARIANTS for step in variant_steps(v, n, size, new)),
        Step("cache after the variants", "SELECT sum(bytes) FROM warren.cache()", kind="measure",
             key="cache_bytes"),
        Step("volume barely turns the subspace", "SELECT @@d_volume@@", kind="check", expect=at_most(0.5),
             claim="distance('subspace'): volume below the documented threshold 0.5"),
        Step("novel turns it further", "SELECT @@d_novel@@ / @@d_volume@@", kind="check", expect=at_least(2),
             claim="a novel community turns the subspace more than twice as far as volume"),
        Step("bridge turns it further", "SELECT @@d_bridge@@ / @@d_volume@@", kind="check", expect=at_least(2),
             claim="a bridge turns the subspace more than twice as far as volume"),
        Step("bridge turns it furthest", "SELECT @@d_bridge@@ > @@d_novel@@", kind="check", expect=is_true,
             claim="A.2's order volume < novel < bridge (DESIGN.md §8: novel against bridge depends on sizes)"),
        Step("bridge moves embeddings most", "SELECT @@e_bridge@@ > greatest(@@e_volume@@, @@e_novel@@)",
             kind="check", expect=is_true, claim="distance('embedding') after aggregate_neighbours ranks bridge highest"),
        Step("dense bridge turns it further", "SELECT @@d_dense_bridge@@ / @@d_volume@@", kind="check",
             expect=at_least(2), claim="a dense bridge (20 edges an entity) turns the subspace more than twice as "
                                       "far as volume"),
        Step("dense bridge turns it furthest", "SELECT @@d_dense_bridge@@ > @@d_novel@@", kind="check",
             expect=is_true, claim="… and further than a novel community"),
        # A staged version committed without promote.
        Step("begin commit", "BEGIN"),
        Step("ingest commit", "INSERT INTO nodes SELECT * FROM ingest_nodes('volume'); "
                              "INSERT INTO edges SELECT * FROM ingest_edges('volume')"),
        Step("staged refresh commit", "SELECT warren.refresh('kg', promote := false, label := 'corpus-commit')",
             key="staged_commit"),
        Step("commit", "COMMIT"),
        Step("committed staged is not current", "SELECT (warren.info('kg') ->> 'current_version') = '@@base@@'",
             kind="check", expect=is_true, claim="after COMMIT without promote the base stays current"),
        Step("committed staged by version", f"SELECT warren.degree('kg', {n + 1}::bigint, version := @@staged_commit@@)",
             kind="check", expect=at_least(1), claim="the staged version is queryable by explicit version"),
        Step("committed staged invisible by default",
             f"SELECT count(*) FROM warren.id_map('kg', NULL::bigint) WHERE node_id > {n}", kind="check",
             expect=equals("0"), claim="the staged version is invisible by default (its new entities are not there)"),
        Step("committed dense ids",
             f"SELECT count(*) FILTER (WHERE b.dense_id <> s.dense_id) || '/' || count(*) "
             f"FROM warren.id_map('kg', NULL::bigint, @@base@@) b "
             f"JOIN warren.id_map('kg', NULL::bigint, @@staged_commit@@) s USING (node_id)",
             kind="check", expect=equals(f"0/{n}"),
             claim="dense ids of all pre-existing entities are identical in both versions"),
        Step("drop committed staged", "SELECT warren.drop_version('kg', @@staged_commit@@); TRUNCATE nodes, edges; "
                                      "VACUUM warren.segments, warren.versions"),
        # Determinism: the base solved again.
        Step("base eigen again", f"CREATE TABLE eigen_again AS SELECT * FROM warren.eigen('kg', {K}, "
                                 f"{EIGEN.format(prefix='again')})", target=600),
        Step("eigenvalues identical", "SELECT count(*) FROM eigen_base b JOIN eigen_again a USING (i) "
                                      "WHERE a.eigenvalue IS DISTINCT FROM b.eigenvalue", kind="check",
             expect=equals("0"), claim="a second solve gives bitwise the same eigenvalues"),
        Step("eigenvectors identical", f"SELECT max(warren.vector_norm_diff('kg', 'lap_' || i, 'again_' || i, 'linf')) "
                                       f"FROM generate_series(1, {K}) i", kind="check", expect=equals("0"),
             claim="… and bitwise the same eigenvectors"),
        Step("drop second solve", drop_eigenvectors("NULL", "again")),
        # The shadow-schema pattern: the bridge corpus as a second graph, compared by external id.
        Step("shadow graph", f"SELECT warren.create_graph('kg_shadow', '{shadow_nodes}', '{shadow_edges}', "
                             f"directed := false)"),
        Step("shadow refresh", "SELECT warren.refresh('kg_shadow')", key="shadow"),
        Step("shadow eigen", f"CREATE TABLE eigen_shadow AS SELECT * FROM warren.eigen('kg_shadow', {K}, "
                             f"{EIGEN.format(prefix='lap')})", target=600),
        Step("shadow subspace", f"SELECT warren.distance('kg', @@base@@, @@shadow@@, 'subspace', vectors_prefix := "
                                f"'lap', k := {K}, graph2 := 'kg_shadow') ->> 'projection'", key="d_shadow"),
        Step("shadow agrees", "SELECT abs(@@d_shadow@@ - @@d_bridge@@)", kind="check", expect=at_most(1e-9),
             claim="the shadow-schema comparison gives the same subspace distance within 1e-9"),
        Step("shadow only adds", "SELECT warren.distance('kg', @@base@@, @@shadow@@, 'edit', graph2 := 'kg_shadow') "
                                 "#>> '{overall,removed}'", kind="check", expect=equals("0"),
             claim="by external id, the shadow graph only adds edges"),
    )
    return UseCase("a2", "A.2", s, steps, settings=("SET warren.max_cache_memory = '8GB'",))
