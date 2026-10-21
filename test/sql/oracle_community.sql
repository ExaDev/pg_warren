-- The library's local and community structure against references
-- (DESIGN.md §6.7, §12) on one undirected random graph: clustering against
-- NetworkX's, and label_propagation_majority against a direct Python
-- reimplementation of its rule (NetworkX's label propagation is random or
-- semi-synchronous), from each node's own id as its label.
-- fixture: random --nodes 400 --edges 1200 --seed 5
-- oracle: clustering
-- oracle: label_propagation_majority
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

SELECT warren.vector_from_query('g', 'ids', 'SELECT id, id FROM nodes');
SELECT assert.that(warren.label_propagation_majority('g', 'lp', seed := 'ids') < 100,
                   'label_propagation_majority converges');
SELECT assert.that((SELECT count(DISTINCT label) FROM oracle_label_propagation_majority) > 1,
                   'the reference has several labels');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'lp', NULL::bigint) l
                     JOIN oracle_label_propagation_majority o USING (node_id) WHERE l.value = o.label),
                    400::bigint, 'label_propagation_majority agrees with the reference');
