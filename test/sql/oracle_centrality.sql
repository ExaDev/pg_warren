-- The library's centralities against NetworkX (DESIGN.md §6.7, §12) on a
-- sparse directed random graph with edge weights 1–5, which has dangling
-- nodes and several components: every score within 1e-9 of NetworkX's,
-- run with the same defaults (tol, max_iter, normalisation).
-- fixture: random --nodes 400 --edges 1200 --directed --seed 5
-- oracle: eigenvector --directed --edges-sql 'SELECT src, dst, (1 + (src * dst) % 5)::float8 FROM edges'
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst, (1 + (src * dst) % 5)::float8 FROM edges',
                           weighted := true);
SELECT warren.refresh('g');

SELECT assert.that(warren.eigenvector_centrality('g', 'ev') < 100, 'eigenvector_centrality converges');
SELECT assert.that((SELECT max(abs(e.value - o.score))
                    FROM warren.vector_values('g', 'ev', NULL::bigint) e JOIN oracle_eigenvector o USING (node_id)) < 1e-9,
                   'eigenvector_centrality agrees with NetworkX');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'ev', NULL::bigint) e
                     JOIN oracle_eigenvector o USING (node_id)), 400::bigint, 'every node is scored');
