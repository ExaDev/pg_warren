-- Betweenness against NetworkX (DESIGN.md §6.7, §12) from a sample,
-- normalised, without endpoints, on oracle_paths.sql's graph: NetworkX
-- 3.7 scales a sampled source by one source fewer than the others, so both
-- factors are checked, given NetworkX the same sources (id % 9 = 4).
-- fixture: random --nodes 300 --edges 900 --directed --seed 13
-- oracle: betweenness --weighted --directed --sources-sql 'SELECT id FROM nodes WHERE id % 9 = 4' --nodes-sql 'SELECT id FROM nodes WHERE id <> 7' --edges-sql 'SELECT src, dst, w FROM (SELECT src, dst, (1 + (src * 3 + dst) % 4)::float8 AS w FROM edges UNION ALL SELECT src, dst, 2::float8 FROM edges WHERE (src + dst) % 5 = 0 UNION ALL SELECT id, id, 1::float8 FROM nodes WHERE id % 13 = 0) e WHERE 7 NOT IN (src, dst)'
\ir include/paths_graph.sql

SELECT warren.vector_from_query('g', 'from', 'SELECT id, 1.0 FROM g_nodes WHERE id % 9 = 4');
SELECT assert.equal(warren.betweenness_centrality('g', 'b', sources := 'from'), 33::bigint,
                    'a float vector selects its non-zero rows');
SELECT assert.that((SELECT count(*) FROM oracle_betweenness o WHERE o.node_id % 9 = 4 AND o.score > 0) > 5,
                   'sampled sources lie on paths too');
SELECT assert.that((SELECT max(abs(b.value - o.score) / greatest(1, abs(o.score)))
                    FROM warren.vector_values('g', 'b', NULL::bigint) b JOIN oracle_betweenness o USING (node_id)) < 1e-9,
                   'normalised sampled betweenness agrees with NetworkX given the same sample');
