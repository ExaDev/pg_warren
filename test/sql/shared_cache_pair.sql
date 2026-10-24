-- DESIGN.md §5.5, §5.6: a call reading two versions (warren.diff) takes
-- the one the shared cache holds first, pinned, so that loading its
-- partner cannot evict it: the partner's load evicts another version
-- instead, and the one held is not loaded again. The cap here fits two of
-- three equal versions. Where the shared cache is off or unavailable,
-- every load is private and no segment is created.
\ir include/shared_cache.sql
CREATE TABLE nodes AS SELECT generate_series(1, 100003)::bigint AS id;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT id, id % 100003 + 1 FROM nodes',
                           retain_versions => 3);
SELECT warren.refresh('g') AS v1 \gset
SELECT warren.refresh('g') AS v2 \gset
SELECT warren.refresh('g') AS v3 \gset
SELECT CASE WHEN :'shared' THEN 1 ELSE 0 END::bigint AS one \gset
SELECT dblink_connect(session, :'conninfo') FROM unnest(ARRAY['x', 'y', 'z']) AS session;

-- x sizes a segment; then the cap is lowered to two and a half of them.
SELECT * FROM dblink('x', $$SELECT warren.prewarm('g')$$) AS t(version bigint);
\if :shared
SELECT format('%skB', shared_segment_size(:v3) * 5 / 2 / 1024) AS cap \gset
ALTER SYSTEM SET warren.shared_cache_memory = :'cap';
SELECT pg_reload_conf();
SELECT pg_sleep(0.5);
\endif
SELECT dblink_disconnect('x');

-- v3, then v1, are shared and mapped by no one: their backends exited.
SELECT * FROM dblink('y', $$SELECT warren.prewarm('g', $$ || :v1 || $$)$$) AS t(version bigint);
SELECT dblink_disconnect('y');
SELECT pg_sleep(0.5);
SELECT assert.equal(shared_segments(:v3), :one, 'v3 is shared');
SELECT assert.equal(shared_segments(:v1), :one, 'v1 is shared');

-- diff(v2, v3): v3, the least recently used, is taken first, so v2's load
-- evicts v1 rather than v3.
SELECT * FROM dblink('z', format($$SELECT warren.diff('g', %s, %s) IS NOT NULL$$, :v2, :v3))
    AS t(diffed boolean);
SELECT assert.equal(shared_segments(:v2), :one, 'v2 is loaded into the shared cache');
SELECT assert.equal(loads, :one, 'v3 is the copy that was shared, not loaded again: z loaded only v2')
FROM dblink('z', $$SELECT shared_loads FROM warren.backends() WHERE pid = pg_backend_pid()$$) AS t(loads bigint);
SELECT assert.equal(shared_segments(:v1), 0::bigint, 'v1 made room for v2');

SELECT dblink_disconnect('z');
\if :shared
ALTER SYSTEM RESET warren.shared_cache_memory;
SELECT pg_reload_conf();
SELECT pg_sleep(0.2);
\endif
