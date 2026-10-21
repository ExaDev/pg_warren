-- The library's local and community structure against references
-- (DESIGN.md §6.7, §12) on one undirected random graph: clustering against
-- NetworkX's.
-- fixture: random --nodes 400 --edges 1200 --seed 5
-- oracle: clustering
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges', directed := false);
SELECT warren.refresh('g');

SELECT warren.clustering('g', 'cc');
SELECT assert.that((SELECT count(*) FROM oracle_clustering WHERE coefficient > 0) > 10,
                   'the fixture has nodes in triangles');
SELECT assert.that((SELECT max(abs(c.value - o.coefficient))
                    FROM warren.vector_values('g', 'cc', NULL::bigint) c JOIN oracle_clustering o USING (node_id))
                   < 1e-12, 'clustering agrees with NetworkX');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'cc', NULL::bigint) c
                     JOIN oracle_clustering o USING (node_id)), 400::bigint, 'every node is compared');
