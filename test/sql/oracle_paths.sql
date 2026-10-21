-- The shortest-path centralities against NetworkX (DESIGN.md §6.7, §12),
-- exact and from given sources, on one directed random graph widened with
-- self-loops, parallel edges in one relation (deduplicated, weights summed)
-- and in a second relation (summed across relations), and a node deleted
-- after the first build (a tombstone, never a source and not counted in
-- n): weighted betweenness (exact, directed), weighted closeness (exact,
-- undirected: a pair's lighter direction) and weighted harmonic centrality
-- from the nodes with id % 10 = 3 (directed). The oracles read the same
-- rows, node 7 left out; every score within 1e-9, relatively.
-- fixture: random --nodes 300 --edges 900 --directed --seed 13
-- oracle: betweenness --weighted --directed --nodes-sql 'SELECT id FROM nodes WHERE id <> 7' --edges-sql 'SELECT src, dst, w FROM (SELECT src, dst, (1 + (src * 3 + dst) % 4)::float8 AS w FROM edges UNION ALL SELECT src, dst, 2::float8 FROM edges WHERE (src + dst) % 5 = 0 UNION ALL SELECT id, id, 1::float8 FROM nodes WHERE id % 13 = 0) e WHERE 7 NOT IN (src, dst)'
-- oracle: closeness --weighted --undirected --directed --nodes-sql 'SELECT id FROM nodes WHERE id <> 7' --edges-sql 'SELECT src, dst, w FROM (SELECT src, dst, (1 + (src * 3 + dst) % 4)::float8 AS w FROM edges UNION ALL SELECT src, dst, 2::float8 FROM edges WHERE (src + dst) % 5 = 0 UNION ALL SELECT id, id, 1::float8 FROM nodes WHERE id % 13 = 0) e WHERE 7 NOT IN (src, dst)'
-- oracle: harmonic --weighted --directed --sources-sql 'SELECT id FROM nodes WHERE id % 10 = 3 AND id <> 7' --nodes-sql 'SELECT id FROM nodes WHERE id <> 7' --edges-sql 'SELECT src, dst, w FROM (SELECT src, dst, (1 + (src * 3 + dst) % 4)::float8 AS w FROM edges UNION ALL SELECT src, dst, 2::float8 FROM edges WHERE (src + dst) % 5 = 0 UNION ALL SELECT id, id, 1::float8 FROM nodes WHERE id % 13 = 0) e WHERE 7 NOT IN (src, dst)'
\ir include/paths_graph.sql

SELECT assert.equal(warren.betweenness_centrality('g', 'b'), 299::bigint, 'every live node is a source');
SELECT assert.that((SELECT count(*) FROM oracle_betweenness WHERE score > 0) > 100, 'many nodes lie on paths');
SELECT assert.that((SELECT max(abs(b.value - o.score) / greatest(1, abs(o.score)))
                    FROM warren.vector_values('g', 'b', NULL::bigint) b JOIN oracle_betweenness o USING (node_id)) < 1e-9,
                   'betweenness agrees with NetworkX');

SELECT assert.equal(warren.closeness_centrality('g', 'c', directed := false), 299::bigint, 'closeness is exact');
SELECT assert.that((SELECT count(DISTINCT score) FROM oracle_closeness) > 100, 'closeness varies');
SELECT assert.that((SELECT max(abs(c.value - o.score) / greatest(1, abs(o.score)))
                    FROM warren.vector_values('g', 'c', NULL::bigint) c JOIN oracle_closeness o USING (node_id)) < 1e-9,
                   'undirected closeness agrees with NetworkX');

SELECT warren.vector_from_query('g', 'from', 'SELECT id, true FROM g_nodes WHERE id % 10 = 3', dtype := 'bool');
SELECT assert.equal(warren.harmonic_centrality('g', 'h', sources := 'from'), 30::bigint, 'the sources are the vector''s nodes');
SELECT assert.that((SELECT max(abs(h.value - o.score) / greatest(1, abs(o.score)))
                    FROM warren.vector_values('g', 'h', NULL::bigint) h JOIN oracle_harmonic o USING (node_id)) < 1e-9,
                   'harmonic centrality from given sources agrees with NetworkX scaled by (n - 1) / k_u');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'h', NULL::bigint) h
                     JOIN oracle_harmonic o USING (node_id)), 299::bigint, 'every live node is scored');
