-- warren.louvain and warren.leiden against NetworkX (DESIGN.md §6.7, §12)
-- on a weighted stochastic block model of 12 planted communities: their
-- modularity is at least NetworkX's louvain_communities' less 0.01, each
-- finds the planted partition (NMI at least 0.9), the modularity each
-- returns is the modularity of its partition computed from the edges in
-- SQL, warren.modularity scores NetworkX's partition as NetworkX does, the
-- same graph stored directed (an edge and its reverse add up) gives the
-- same partition, and Leiden's communities are connected.
-- fixture: sbm --communities 12 --community-size 50 --p-in 0.25 --p-out 0.01 --seed 3
-- oracle: louvain --edges-sql 'SELECT src, dst, (1 + (src + dst) % 3)::float8 FROM edges'
CREATE TABLE w_edges AS SELECT src, dst, (1 + (src + dst) % 3)::float8 AS w FROM edges;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst, w FROM w_edges',
                           directed := false, weighted := true);
SELECT warren.create_graph('d', 'SELECT id FROM nodes', 'SELECT src, dst, w FROM w_edges', weighted := true);
SELECT warren.refresh('g'), warren.refresh('d');

-- Modularity of the partition `labels` (node_id, label) from the edges:
-- NetworkX's formula, Σ_c L_c / m − (d_c / 2m)².
CREATE FUNCTION edge_modularity(labels regclass) RETURNS float8 LANGUAGE plpgsql AS $$
DECLARE q float8;
BEGIN
  EXECUTE format(
    'WITH m AS (SELECT sum(w) AS m FROM w_edges),
          inside AS (SELECT a.label, sum(e.w) AS l FROM w_edges e JOIN %1$s a ON a.node_id = e.src
                     JOIN %1$s b ON b.node_id = e.dst WHERE a.label = b.label GROUP BY a.label),
          degree AS (SELECT l.label, sum(e.w) AS d FROM %1$s l
                     JOIN (SELECT src AS n, w FROM w_edges UNION ALL SELECT dst, w FROM w_edges) e ON e.n = l.node_id
                     GROUP BY l.label)
     SELECT sum(coalesce(i.l, 0) / m.m - (d.d / (2 * m.m)) ^ 2) FROM degree d LEFT JOIN inside i USING (label), m',
    labels) INTO q;
  RETURN q;
END $$;

-- Normalised mutual information of `labels` against the planted partition.
CREATE FUNCTION planted_nmi(labels regclass) RETURNS float8 LANGUAGE plpgsql AS $$
DECLARE nmi float8;
BEGIN
  EXECUTE format(
    'WITH j AS (SELECT l.label AS x, n.community AS y, count(*)::float8 AS c
                FROM %s l JOIN nodes n ON n.id = l.node_id GROUP BY 1, 2),
          t AS (SELECT sum(c) AS n FROM j),
          px AS (SELECT x, sum(c) AS c FROM j GROUP BY x),
          py AS (SELECT y, sum(c) AS c FROM j GROUP BY y),
          i AS (SELECT sum(j.c / t.n * ln(j.c * t.n / (px.c * py.c))) AS i FROM j JOIN px USING (x) JOIN py USING (y), t),
          hx AS (SELECT -sum(c / t.n * ln(c / t.n)) AS h FROM px, t),
          hy AS (SELECT -sum(c / t.n * ln(c / t.n)) AS h FROM py, t)
     SELECT 2 * i.i / (hx.h + hy.h) FROM i, hx, hy', labels) INTO nmi;
  RETURN nmi;
END $$;

SELECT warren.louvain('g', 'louvain') AS louvain_q \gset
SELECT warren.leiden('g', 'leiden') AS leiden_q \gset
SELECT max(modularity) AS nx_q FROM oracle_louvain \gset
CREATE TABLE louvain AS SELECT node_id, value::bigint AS label FROM warren.vector_values('g', 'louvain', NULL::bigint);
CREATE TABLE leiden AS SELECT node_id, value::bigint AS label FROM warren.vector_values('g', 'leiden', NULL::bigint);
CREATE TABLE networkx AS SELECT node_id, community AS label FROM oracle_louvain;

SELECT assert.that(:louvain_q >= :nx_q - 0.01, format('louvain %s against NetworkX %s', :louvain_q, :nx_q));
SELECT assert.that(:leiden_q >= :nx_q - 0.01, format('leiden %s against NetworkX %s', :leiden_q, :nx_q));
SELECT assert.that(abs(:louvain_q - edge_modularity('louvain')) < 1e-12, 'louvain returns its modularity');
SELECT assert.that(abs(:leiden_q - edge_modularity('leiden')) < 1e-12, 'leiden returns its modularity');
SELECT warren.vector_from_query('g', 'nx', 'SELECT node_id, label FROM networkx');
SELECT assert.that(abs(warren.modularity('g', 'nx') - :nx_q) < 1e-12, 'warren.modularity scores as NetworkX does');
SELECT assert.that(planted_nmi('louvain') >= 0.9, format('louvain NMI %s', planted_nmi('louvain')));
SELECT assert.that(planted_nmi('leiden') >= 0.9, format('leiden NMI %s', planted_nmi('leiden')));
SELECT assert.that(planted_nmi('networkx') >= 0.9, 'the fixture''s communities are findable');

-- Directed, the two directions add up to the undirected graph's weights.
SELECT assert.equal(warren.louvain('d', 'louvain'), :louvain_q, 'directed: the same modularity');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('d', 'louvain', NULL::bigint) v
                     JOIN louvain l USING (node_id) WHERE v.value <> l.label), 0::bigint,
                    'directed: the same partition');

-- Leiden's communities are connected: the edges inside them have one
-- component per community.
CREATE TABLE inside AS SELECT e.src, e.dst FROM w_edges e JOIN leiden a ON a.node_id = e.src
                       JOIN leiden b ON b.node_id = e.dst WHERE a.label = b.label;
SELECT warren.create_graph('inside', 'SELECT id FROM nodes', 'SELECT src, dst FROM inside', directed := false);
SELECT warren.refresh('inside');
SELECT assert.equal((SELECT count(DISTINCT component_id) FROM warren.components('inside', NULL::bigint)),
                    (SELECT count(DISTINCT label) FROM leiden), 'every Leiden community is connected');
