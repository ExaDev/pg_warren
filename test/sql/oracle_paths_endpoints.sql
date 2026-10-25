-- Betweenness against NetworkX (DESIGN.md §6.7, §12) with endpoints, on
-- oracle_paths.sql's graph: exact and weighted, normalised (endpoints
-- counted on every path, the source on each it starts), within 1e-9.
-- fixture: random --nodes 300 --edges 900 --directed --seed 13
-- oracle: betweenness --endpoints --weighted --directed --nodes-sql 'SELECT id FROM nodes WHERE id <> 7' --edges-sql 'SELECT src, dst, min(w) FROM (SELECT src, dst, rel, sum(w) AS w FROM (SELECT src, dst, 0 AS rel, (1 + (src * 3 + dst) % 4)::float8 AS w FROM edges UNION ALL SELECT src, dst, 1, 2::float8 FROM edges WHERE (src + dst) % 5 = 0 UNION ALL SELECT id, id, 0, 1::float8 FROM nodes WHERE id % 13 = 0) e WHERE 7 NOT IN (src, dst) GROUP BY 1, 2, 3) r GROUP BY 1, 2'
\ir include/paths_graph.sql

SELECT assert.equal(warren.betweenness_centrality('g', 'b', endpoints := true), 299::bigint, 'exact');
SELECT assert.that((SELECT count(*) FROM oracle_betweenness WHERE score > 0) > 250, 'endpoints count on their own paths');
SELECT assert.that((SELECT max(abs(b.value - o.score) / greatest(1, abs(o.score)))
                    FROM warren.vector_values('g', 'b', NULL::bigint) b JOIN oracle_betweenness o USING (node_id)) < 1e-9,
                   'betweenness with endpoints agrees with NetworkX');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'b', NULL::bigint) b
                     JOIN oracle_betweenness o USING (node_id)), 299::bigint, 'every live node is scored');
