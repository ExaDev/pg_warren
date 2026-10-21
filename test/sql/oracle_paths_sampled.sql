-- The shortest-path centralities against NetworkX (DESIGN.md §6.7, §12),
-- the other combinations of oracle_paths.sql on the same graph:
-- unweighted undirected betweenness from the nodes with id % 7 = 2 (as
-- NetworkX's sample: its sampling rescale, source and non-source),
-- weighted directed closeness from the same sources (the estimator over
-- NetworkX's incoming distances) without Wasserman–Faust, and exact
-- unweighted undirected harmonic centrality. A sample of every live node
-- is exact, and a seed gives the same sample on any thread count.
-- fixture: random --nodes 300 --edges 900 --directed --seed 13
-- oracle: betweenness --undirected --no-normalized --directed --sources-sql 'SELECT id FROM nodes WHERE id % 7 = 2' --nodes-sql 'SELECT id FROM nodes WHERE id <> 7' --edges-sql 'SELECT src, dst FROM (SELECT src, dst FROM edges UNION ALL SELECT id, id FROM nodes WHERE id % 13 = 0) e WHERE 7 NOT IN (src, dst)'
-- oracle: closeness --weighted --no-wf-improved --directed --sources-sql 'SELECT id FROM nodes WHERE id % 7 = 2' --nodes-sql 'SELECT id FROM nodes WHERE id <> 7' --edges-sql 'SELECT src, dst, w FROM (SELECT src, dst, (1 + (src * 3 + dst) % 4)::float8 AS w FROM edges UNION ALL SELECT src, dst, 2::float8 FROM edges WHERE (src + dst) % 5 = 0 UNION ALL SELECT id, id, 1::float8 FROM nodes WHERE id % 13 = 0) e WHERE 7 NOT IN (src, dst)'
-- oracle: harmonic --undirected --directed --nodes-sql 'SELECT id FROM nodes WHERE id <> 7' --edges-sql 'SELECT src, dst FROM (SELECT src, dst FROM edges UNION ALL SELECT id, id FROM nodes WHERE id % 13 = 0) e WHERE 7 NOT IN (src, dst)'
\ir include/paths_graph.sql

SELECT warren.vector_from_query('g', 'from', 'SELECT id, true FROM g_nodes WHERE id % 7 = 2', dtype := 'bool');
SELECT assert.equal(warren.betweenness_centrality('g', 'b', sources := 'from', normalized := false,
                                                  directed := false, weighted := false), 43::bigint,
                    'the sources are the population');
SELECT assert.that((SELECT max(abs(b.value - o.score) / greatest(1, abs(o.score)))
                    FROM warren.vector_values('g', 'b', NULL::bigint) b JOIN oracle_betweenness o USING (node_id)) < 1e-9,
                   'sampled betweenness agrees with NetworkX given the same sample');

SELECT warren.closeness_centrality('g', 'c', sources := 'from', wf_improved := false);
SELECT assert.that((SELECT count(DISTINCT score) FROM oracle_closeness) > 50, 'closeness varies');
SELECT assert.that((SELECT max(abs(c.value - o.score) / greatest(1, abs(o.score)))
                    FROM warren.vector_values('g', 'c', NULL::bigint) c JOIN oracle_closeness o USING (node_id)) < 1e-9,
                   'the closeness estimator agrees with its definition over NetworkX''s distances');

SELECT warren.harmonic_centrality('g', 'h', directed := false, weighted := false);
SELECT assert.that((SELECT max(abs(h.value - o.score) / greatest(1, abs(o.score)))
                    FROM warren.vector_values('g', 'h', NULL::bigint) h JOIN oracle_harmonic o USING (node_id)) < 1e-9,
                   'undirected harmonic centrality agrees with NetworkX');
SELECT warren.harmonic_centrality('g', 'every', k := 1000, directed := false, weighted := false);
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'h', NULL::bigint) h
                     JOIN warren.vector_values('g', 'every', NULL::bigint) e USING (node_id) WHERE h.value = e.value),
                    299::bigint, 'a sample of every live node is exact');

SET warren.threads = 1;
SELECT warren.betweenness_centrality('g', 'one', k := 25, seed := 3);
SET warren.threads = 3;
SELECT warren.betweenness_centrality('g', 'three', k := 25, seed := 3);
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'one', NULL::bigint) a
                     JOIN warren.vector_values('g', 'three', NULL::bigint) b USING (node_id) WHERE a.value = b.value),
                    299::bigint, 'a seed gives the same bits on any thread count');
