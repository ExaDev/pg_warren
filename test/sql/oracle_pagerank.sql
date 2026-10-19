-- warren.pagerank against NetworkX (DESIGN.md §6.7, §12) on a sparse
-- directed random graph, which has dangling nodes (no out-edges): every
-- score within 1e-9 of NetworkX's.
-- fixture: random --nodes 300 --edges 900 --directed --seed 4
-- oracle: pagerank --damping 0.85 --directed
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('g');
SELECT assert.that((SELECT count(*) FROM nodes n WHERE NOT EXISTS (SELECT 1 FROM edges e WHERE e.src = n.id)) > 0,
                   'the fixture has dangling nodes');

SELECT assert.that(warren.pagerank('g', 'pr', tol := 1e-12, max_iter := 1000) < 1000, 'pagerank converges');
SELECT assert.that((SELECT max(abs(p.value - o.score))
                    FROM warren.vector_values('g', 'pr', NULL::bigint) p JOIN oracle_pagerank o USING (node_id)) < 1e-9,
                   'pagerank agrees with NetworkX');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'pr', NULL::bigint) p
                     JOIN oracle_pagerank o USING (node_id)), 300::bigint, 'every node is ranked');
