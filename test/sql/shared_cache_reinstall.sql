-- DESIGN.md §5.5, §5.6: version ids restart when pg_warren is dropped and
-- created again, so the registry's key names the installation (its
-- pg_extension oid), and a backend drops its own caches when the
-- installation changes. A backend that read a version before the reinstall
-- reads the new installation's version of the same id after it, never the
-- old one, shared or in its own cache, and the next miss's prune evicts
-- the old installation's shared versions. Where the shared cache is off or
-- unavailable, nothing is shared.
\ir include/shared_cache.sql
CREATE TABLE nodes AS SELECT generate_series(1, 100003)::bigint AS id;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT id, id % 100003 + 1 FROM nodes');
SELECT warren.refresh('g') AS old \gset
SELECT CASE WHEN :'shared' THEN 1 ELSE 0 END::bigint AS one \gset
SELECT dblink_connect(session, :'conninfo') FROM unnest(ARRAY['a', 'b']) AS session;
SELECT assert.equal(degree, 1::bigint, 'a reads the first installation''s version')
FROM dblink('a', $$SELECT warren.degree('g', 1::bigint)$$) AS t(degree bigint);
SELECT assert.equal(shared_segments(:old), :one, 'it is shared');

-- Reinstalled, with a graph of the same name, whose first version has the
-- same id and two edges out of node 1.
DROP EXTENSION pg_warren CASCADE;
CREATE EXTENSION pg_warren;
CREATE TABLE edges AS SELECT id AS src, id % 100003 + 1 AS dst FROM nodes;
INSERT INTO edges VALUES (1, 3);
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('g') AS new \gset
SELECT assert.equal(:new::bigint, :old::bigint, 'version ids restart');
SELECT assert.equal(degree, 2::bigint, 'a reads the new installation''s version')
FROM dblink('a', $$SELECT warren.degree('g', 1::bigint)$$) AS t(degree bigint);
SELECT assert.equal(degree, 2::bigint, 'and so does a backend new to it')
FROM dblink('b', $$SELECT warren.degree('g', 1::bigint)$$) AS t(degree bigint);
SELECT assert.equal(shared_segments(:new), :one, 'the new installation''s version is shared');
SELECT assert.equal(unnamed_shared_bytes(), :unnamed, 'the old installation''s was pruned');

SELECT dblink_disconnect(session) FROM unnest(ARRAY['a', 'b']) AS session;
