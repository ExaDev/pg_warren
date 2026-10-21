-- The library's centralities against NetworkX (DESIGN.md §6.7, §12) on a
-- sparse directed random graph with edge weights 1–5, which has dangling
-- nodes and several components: every score within 1e-9 of NetworkX's,
-- run with the same defaults (tol, max_iter, normalisation), except that
-- HITS may take 1000 iterations on both sides: it needs 100 here.
-- fixture: random --nodes 400 --edges 1200 --directed --seed 5
-- oracle: eigenvector --directed --edges-sql 'SELECT src, dst, (1 + (src * dst) % 5)::float8 FROM edges'
-- oracle: katz --directed --edges-sql 'SELECT src, dst, (1 + (src * dst) % 5)::float8 FROM edges'
-- oracle: hits --directed --edges-sql 'SELECT src, dst, (1 + (src * dst) % 5)::float8 FROM edges'
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst, (1 + (src * dst) % 5)::float8 FROM edges',
                           weighted := true);
SELECT warren.refresh('g');

SELECT assert.that(warren.eigenvector_centrality('g', 'ev') < 100, 'eigenvector_centrality converges');
SELECT assert.that((SELECT max(abs(e.value - o.score))
                    FROM warren.vector_values('g', 'ev', NULL::bigint) e JOIN oracle_eigenvector o USING (node_id)) < 1e-9,
                   'eigenvector_centrality agrees with NetworkX');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'ev', NULL::bigint) e
                     JOIN oracle_eigenvector o USING (node_id)), 400::bigint, 'every node is scored');

SELECT assert.that(warren.katz_centrality('g', 'katz') < 1000, 'katz_centrality converges');
SELECT assert.that((SELECT max(abs(k.value - o.score))
                    FROM warren.vector_values('g', 'katz', NULL::bigint) k JOIN oracle_katz o USING (node_id)) < 1e-9,
                   'katz_centrality agrees with NetworkX');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'katz', NULL::bigint) k
                     JOIN oracle_katz o USING (node_id)), 400::bigint, 'every node is scored');

SELECT assert.that(warren.hits('g', 'hub', 'authority', max_iter := 1000) < 1000, 'hits converges');
SELECT assert.that((SELECT max(greatest(abs(h.value - o.hub), abs(a.value - o.authority)))
                    FROM warren.vector_values('g', 'hub', NULL::bigint) h
                    JOIN warren.vector_values('g', 'authority', NULL::bigint) a USING (node_id)
                    JOIN oracle_hits o USING (node_id)) < 1e-9,
                   'hits agrees with NetworkX');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'hub', NULL::bigint) h
                     JOIN oracle_hits o USING (node_id)), 400::bigint, 'every node is scored');
