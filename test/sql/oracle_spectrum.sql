-- warren.eigen against a dense eigensolver (numpy eigvalsh, harness/oracle.py)
-- on a small random graph, and the same result for every run and thread
-- count (DESIGN.md §6.5, §12; Appendix A.2's determinism).
-- fixture: random --nodes 150 --edges 600 --seed 3
-- oracle: spectrum --matrix adjacency --which largest --k 6
SELECT warren.create_graph('r', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges', directed := false);
SELECT warren.refresh('r');

CREATE TEMP TABLE solved AS
SELECT * FROM warren.eigen('r', 6, dtype := 'f64', tol := 1e-12, symmetrize := false);
SELECT assert.equal((SELECT count(*) FROM solved), 6::bigint, 'six eigenpairs');
SELECT assert.that((SELECT max(abs(s.eigenvalue - o.eigenvalue)) FROM solved s JOIN oracle_spectrum o USING (i)) < 1e-9,
                   'the eigenvalues are the dense solver''s');
SELECT assert.that((SELECT max(residual) FROM solved) < 1e-9, 'every residual is small');

-- f32 vectors (the default) agree to their precision.
SELECT assert.that((SELECT max(abs(s.eigenvalue - o.eigenvalue))
                    FROM warren.eigen('r', 6) s JOIN oracle_spectrum o USING (i)) < 1e-5,
                   'f32 eigenvalues agree to f32 precision');

-- Identical results across runs and thread counts, eigenvectors included.
SET warren.threads = 1;
CREATE TEMP TABLE one AS SELECT * FROM warren.eigen('r', 4, 'normalized_laplacian', 'largest', vectors_prefix := 'v');
CREATE TEMP TABLE one_vector AS SELECT * FROM warren.vector_values('r', 'v_1', NULL::bigint);
SET warren.threads = 4;
CREATE TEMP TABLE four AS SELECT * FROM warren.eigen('r', 4, 'normalized_laplacian', 'largest', vectors_prefix := 'v');
SELECT assert.equal((SELECT count(*) FROM one FULL JOIN four USING (i)
                     WHERE one.eigenvalue IS DISTINCT FROM four.eigenvalue
                        OR one.residual IS DISTINCT FROM four.residual), 0::bigint,
                    'eigenpairs are identical on 1 and 4 threads');
SELECT assert.equal((SELECT count(*) FROM one_vector o FULL JOIN warren.vector_values('r', 'v_1', NULL::bigint) f
                     USING (node_id) WHERE o.value IS DISTINCT FROM f.value), 0::bigint,
                    'eigenvectors are identical on 1 and 4 threads');
