-- One A.2 ingest variant, :'variant', through the whole §8 workflow; sets
-- :distance, the subspace distance from :base, and :drift, the mean
-- embedding drift after aggregate_neighbours. Included by a2_spectral.sql.
BEGIN;
INSERT INTO nodes SELECT id, community FROM ingest_nodes WHERE variant = :'variant';
INSERT INTO edges SELECT src, dst FROM ingest_edges WHERE variant = :'variant';
SELECT warren.refresh('kg', promote := false, label := 'corpus-' || :'variant') AS staged \gset
SELECT assert.equal((warren.diff('kg', :base, :staged) #>> '{nodes,added}')::bigint,
                    (SELECT count(*) FROM ingest_nodes WHERE variant = :'variant'), :'variant' || ': diff sees the ingest');
SELECT * FROM warren.eigen('kg', 6, 'normalized_laplacian', 'smallest', dtype := 'f64', tol := 1e-12,
                           vectors_prefix := 'lap', version := :base);
SELECT * FROM warren.eigen('kg', 6, 'normalized_laplacian', 'smallest', dtype := 'f64', tol := 1e-12,
                           vectors_prefix := 'lap', version := :staged);
SELECT warren.distance('kg', :base, :staged, 'subspace', vectors_prefix := 'lap', k := 6) AS measured \gset
SELECT (:'measured'::jsonb ->> 'projection')::float8 AS distance \gset
SELECT assert.equal(warren.distance('kg', :base, :staged, 'subspace', vectors_prefix := 'lap', k := 6),
                    :'measured'::jsonb, :'variant' || ': the same comparison twice gives the same result');
-- Neighbour-aggregated embeddings before and after (A.2), compared on the
-- entities both versions share.
SELECT warren.vector_from_query('kg', 'emb', 'SELECT id, embedding FROM embeddings', 'f32', version := :base);
SELECT warren.vector_from_query('kg', 'emb', format(
         'SELECT id, embedding FROM embeddings UNION ALL SELECT id, embedding FROM ingest_embeddings WHERE variant = %L',
         :'variant'), 'f32', version := :staged);
SELECT warren.aggregate_neighbours('kg', 'emb', 'agg', version := :base);
SELECT warren.aggregate_neighbours('kg', 'emb', 'agg', version := :staged);
SELECT (warren.distance('kg', :base, :staged, 'embedding', x := 'agg') ->> 'mean_l2')::float8 AS drift \gset
ROLLBACK;

SELECT assert.equal((SELECT array_agg(version_id) FROM warren.versions), ARRAY[:base]::bigint[],
                    :'variant' || ': after ROLLBACK only the base version remains');
SELECT assert.equal((SELECT count(*) FROM warren.segments WHERE version_id <> :base), 0::bigint,
                    :'variant' || ': no segments of the staged version remain');
SELECT assert.equal((SELECT count(*) FROM warren.vectors), 0::bigint,
                    :'variant' || ': no eigenvectors remain');
