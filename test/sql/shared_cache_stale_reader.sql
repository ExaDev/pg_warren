-- DESIGN.md §5.6, §14 item 34: whether a version supersedes the graph's
-- others is judged by the latest committed current version, not by the
-- caller's snapshot: a REPEATABLE READ reader that keeps reading the
-- version its snapshot sees as current, after a promotion, never evicts
-- the version promoted since. Where the shared cache is off or
-- unavailable, nothing is shared.
\ir include/shared_cache.sql
CREATE TABLE nodes AS SELECT generate_series(1, 100003)::bigint AS id;
CREATE TABLE edges AS SELECT id AS src, id % 100003 + 1 AS dst FROM nodes;
SELECT warren.create_graph('k', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('k') AS k1 \gset
INSERT INTO edges VALUES (1, 3);
SELECT warren.refresh('k', promote := false) AS k2 \gset
SELECT CASE WHEN :'shared' THEN 1 ELSE 0 END::bigint AS one \gset
SELECT dblink_connect(session, :'conninfo') FROM unnest(ARRAY['r', 's']) AS session;

-- v2 is staged (so r's snapshot sees it, as not current). r reads k's v1
-- in a REPEATABLE READ transaction; then v2 is promoted, and s loads it
-- (evicting v1, which r maps) and exits, so nothing maps v2.
SELECT dblink_exec('r', 'BEGIN ISOLATION LEVEL REPEATABLE READ');
SELECT assert.equal(degree, 1::bigint, 'r reads v1')
FROM dblink('r', $$SELECT warren.degree('k', 1::bigint)$$) AS t(degree bigint);
SELECT warren.promote('k', :k2);
SELECT assert.equal(degree, 2::bigint, 's reads the promoted v2')
FROM dblink('s', $$SELECT warren.degree('k', 1::bigint)$$) AS t(degree bigint);
SELECT dblink_disconnect('s');
SELECT pg_sleep(0.3);
SELECT assert.equal(shared_segments(:k2), :one, 'v2 is shared');

-- r keeps reading v1, which its snapshot sees as current: that never
-- evicts v2, the current version.
SELECT assert.equal(degree, 1::bigint, 'r still reads v1')
FROM unnest(ARRAY[1, 2]) AS round,
     LATERAL dblink('r', $$SELECT warren.degree('k', 1::bigint)$$) AS t(degree bigint);
SELECT assert.equal(shared_segments(:k2), :one, 'a stale reader does not evict the current version');
SELECT dblink_exec('r', 'COMMIT');

SELECT dblink_disconnect('r');
