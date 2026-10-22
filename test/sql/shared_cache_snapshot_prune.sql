-- DESIGN.md §5.6, §14 item 34: the prune judges which shared versions
-- still exist by the latest committed state, not by the caller's
-- snapshot: a REPEATABLE READ transaction whose snapshot predates a
-- version does not prune it, whoever calls warren.evict and whatever the
-- graph. Where the shared cache is off or unavailable, nothing is shared.
\ir include/shared_cache.sql
CREATE TABLE nodes AS SELECT generate_series(1, 100003)::bigint AS id;
CREATE TABLE edges AS SELECT id AS src, id % 100003 + 1 AS dst FROM nodes;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.create_graph('h', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('h') AS h1 \gset
SELECT CASE WHEN :'shared' THEN 1 ELSE 0 END::bigint AS one \gset
SELECT dblink_connect(session, :'conninfo') FROM unnest(ARRAY['a', 'b']) AS session;

-- a's snapshot predates g's version, which b loads and then releases (its
-- backend exits), so nothing maps it.
SELECT dblink_exec('a', 'BEGIN ISOLATION LEVEL REPEATABLE READ');
SELECT * FROM dblink('a', $$SELECT warren.degree('h', 1::bigint)$$) AS t(degree bigint);
SELECT warren.refresh('g') AS g1 \gset
SELECT * FROM dblink('b', $$SELECT warren.degree('g', 1::bigint)$$) AS t(degree bigint);
SELECT dblink_disconnect('b');
SELECT pg_sleep(0.3);
SELECT assert.equal(shared_segments(:g1), :one, 'g''s version is shared');

-- warren.evict, by a role that owns nothing, prunes: g's version exists,
-- though a's snapshot does not see it, so it stays.
-- (Where nothing is shared, it frees a's own copy of h.)
SELECT assert.equal(evicted, 1 - :one, 'the stranger evicts no shared version of h''s')
FROM dblink('a', $$SELECT own_evictions('h')$$) AS t(evicted bigint);
SELECT assert.equal(shared_segments(:g1), :one, 'an old snapshot does not prune a newer version');
SELECT dblink_exec('a', 'COMMIT');

SELECT dblink_disconnect('a');
