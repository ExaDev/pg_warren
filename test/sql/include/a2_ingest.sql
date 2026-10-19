-- One A.2 ingest variant, :'variant', through the §8 workflow up to diff:
-- ingest inside a transaction, build a staged version, diff it against the
-- current version :base, then roll back. Included by a2_ingest_diff.sql.
BEGIN;
INSERT INTO nodes SELECT id, community FROM ingest_nodes WHERE variant = :'variant';
INSERT INTO edges SELECT src, dst FROM ingest_edges WHERE variant = :'variant';
SELECT warren.refresh('kg', promote := false, label := 'corpus-' || :'variant') AS staged \gset
SELECT value -> :'variant' AS expected FROM truth WHERE key = 'variants' \gset
SELECT warren.diff('kg', :base, :staged) AS delta \gset

SELECT assert.equal((:'delta'::jsonb #>> '{nodes,added}')::bigint, (:'expected'::jsonb ->> 'nodes')::bigint,
                    :'variant' || ': diff reports the ingested entities');
SELECT assert.equal((:'delta'::jsonb #>> '{nodes,removed}')::bigint, 0::bigint, :'variant' || ': no entities removed');
-- A.2: edge counts within 5% of the generator's. The graph is undirected, so
-- diff counts each edge once per direction.
SELECT assert.that(abs((:'delta'::jsonb #>> '{all_relations,added}')::bigint - 2 * (:'expected'::jsonb ->> 'edges')::bigint)
                   <= 0.05 * 2 * (:'expected'::jsonb ->> 'edges')::bigint,
                   :'variant' || ': diff reports the ingested edges within 5%');
SELECT assert.equal((:'delta'::jsonb #>> '{all_relations,removed}')::bigint, 0::bigint, :'variant' || ': no edges removed');
ROLLBACK;

SELECT assert.equal((SELECT array_agg(version_id) FROM warren.versions), ARRAY[:base]::bigint[],
                    :'variant' || ': after ROLLBACK only the base version remains');
SELECT assert.equal((SELECT count(*) FROM warren.segments WHERE version_id <> :base), 0::bigint,
                    :'variant' || ': after ROLLBACK no segments of the staged version remain');
SELECT assert.equal((SELECT count(*) FROM warren.vectors), 0::bigint, :'variant' || ': no vectors remain');
SELECT assert.equal((SELECT count(*) FROM nodes), (SELECT (value)::bigint FROM truth WHERE key = 'nodes'),
                    :'variant' || ': the ingest rolled back');
