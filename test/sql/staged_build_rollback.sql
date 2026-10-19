-- DESIGN.md §12 / A.2: rolling back a transaction containing a staged build
-- leaves no rows; after COMMIT without promote, the staged version is
-- queryable by explicit version and invisible by default (warren.info's
-- current version until traversal SQL exists).
-- fixture: random --nodes 200 --edges 600 --seed 1
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges', directed := false);

-- The graph's first build, staged and rolled back.
BEGIN;
SELECT warren.refresh('g', promote := false) AS staged \gset
SELECT assert.that((SELECT count(*) FROM warren.segments WHERE version_id = :staged) > 0, 'the staged build wrote segments');
ROLLBACK;
SELECT assert.equal((SELECT count(*) FROM warren.versions), 0::bigint, 'no versions after rollback');
SELECT assert.equal((SELECT count(*) FROM warren.segments), 0::bigint, 'no segments after rollback');
SELECT assert.equal(warren.info('g') -> 'versions', '[]'::jsonb, 'info lists no versions after rollback');

-- With a promoted version in place: ingest and a staged build, rolled back,
-- leave exactly the promoted version's rows.
SELECT warren.refresh('g') AS current \gset
SELECT count(*) AS current_segments FROM warren.segments \gset
BEGIN;
INSERT INTO nodes SELECT generate_series(1001, 1010);
INSERT INTO edges SELECT n, n + 1 FROM generate_series(1001, 1009) n;
SELECT warren.refresh('g', promote := false) AS staged \gset
ROLLBACK;
SELECT assert.equal((SELECT array_agg(version_id) FROM warren.versions), ARRAY[:current]::bigint[], 'only the promoted version remains');
SELECT assert.equal((SELECT count(*) FROM warren.segments WHERE version_id = :current), :current_segments::bigint, 'the promoted version keeps its segments');
SELECT assert.equal((SELECT count(*) FROM warren.segments), :current_segments::bigint, 'no other segments remain');
SELECT assert.equal((SELECT count(*) FROM warren.vectors), 0::bigint, 'no vectors remain');
SELECT assert.equal((SELECT count(*) FROM nodes), 200::bigint, 'the ingest rolled back too');

-- The same ingest and staged build, committed without promote.
BEGIN;
INSERT INTO nodes SELECT generate_series(1001, 1010);
INSERT INTO edges SELECT n, n + 1 FROM generate_series(1001, 1009) n;
SELECT warren.refresh('g', promote := false) AS staged \gset
COMMIT;
SELECT assert.equal((warren.info('g') ->> 'current_version')::bigint, :current::bigint, 'the default version is still the promoted one');
SELECT jsonb_path_query_first(warren.info('g'), '$.versions[*] ? (@.version == $v)', jsonb_build_object('v', :staged)) AS staged_info \gset
SELECT assert.equal((:'staged_info'::jsonb ->> 'staged')::boolean, true, 'the committed build is staged');
SELECT assert.equal((:'staged_info'::jsonb ->> 'current')::boolean, false, 'the committed build is not current');
SELECT assert.equal((:'staged_info'::jsonb ->> 'node_count')::bigint, 210::bigint, 'the staged build saw the ingest');
SELECT warren.diff('g', :current, :staged) AS delta \gset
SELECT assert.equal((:'delta'::jsonb #>> '{nodes,added}')::bigint, 10::bigint, 'the staged version is readable by explicit version');
SELECT assert.equal((:'delta'::jsonb #>> '{nodes,removed}')::bigint, 0::bigint, 'nothing was removed');
-- An undirected graph stores each edge in both directions, and diff counts both.
SELECT assert.equal((:'delta'::jsonb #>> '{all_relations,added}')::bigint, 2 * 9::bigint, 'the staged version has the ingested edges');
