-- DESIGN.md §5.6, §14 item 28: under warren.shared_cache_memory, a version
-- that does not fit beside the shared versions in use is loaded privately,
-- never waited for, and admission drains the least recently used version
-- that backends map but no statement pins, so that it is freed once they
-- release it, at their next read of the shared cache; a version a
-- statement pins is never evicted. The cap here fits one of two equal
-- versions. Where the shared cache is off or unavailable, every load is
-- private and no segment is created.
\ir include/shared_cache.sql
CREATE TABLE nodes AS SELECT generate_series(1, 100003)::bigint AS id;
SELECT warren.create_graph('g1', 'SELECT id FROM nodes', 'SELECT id, id % 100003 + 1 FROM nodes');
SELECT warren.create_graph('g2', 'SELECT id FROM nodes', 'SELECT id, id % 100003 + 1 FROM nodes');
SELECT warren.refresh('g1') AS v1 \gset
SELECT warren.refresh('g2') AS v2 \gset
SELECT CASE WHEN :'shared' THEN 1 ELSE 0 END::bigint AS one \gset
SELECT dblink_connect(session, :'conninfo') FROM unnest(ARRAY['a', 'b', 'c', 'd']) AS session;

-- a loads g1 into the shared cache and keeps it mapped between statements;
-- then the cap is lowered to one and a half of its segments.
\if :shared
ALTER SYSTEM SET warren.shared_cache_memory = '1GB';
SELECT pg_reload_conf();
SELECT pg_sleep(0.5);
\endif
SELECT * FROM dblink('a', $$SELECT warren.prewarm('g1')$$) AS t(version bigint);
SELECT assert.equal(shared_segments(:v1), :one, 'g1 is shared');
\if :shared
SELECT format('%skB', shared_segment_size(:v1) * 3 / 2 / 1024) AS cap \gset
ALTER SYSTEM SET warren.shared_cache_memory = :'cap';
SELECT pg_reload_conf();
SELECT pg_sleep(0.5);
\endif

-- g2 does not fit beside g1, which a maps: b loads g2 privately, and g1
-- drains while a maps it.
SELECT * FROM dblink('b', $$SELECT warren.prewarm('g2')$$) AS t(version bigint);
SELECT assert.equal(shared_segments(:v2), 0::bigint, 'g2 is loaded privately');
SELECT assert.equal(shared_segments(:v1), :one, 'g1 stays while a maps it');
SELECT assert.equal(evicted, 1::bigint, 'b holds g2 in its own cache')
FROM dblink('b', $$SELECT warren.evict('g2')$$) AS t(evicted bigint);

-- a's next read releases g1, which is freed, and then g2 fits.
SELECT * FROM dblink('a', $$SELECT warren.prewarm('g2')$$) AS t(version bigint);
SELECT assert.equal(shared_segments(:v1), 0::bigint, 'g1 is freed once a releases it');
SELECT assert.equal(shared_segments(:v2), :one, 'g2 is shared');

-- While c's statement pins g2, d's g1 does not fit and nothing is evicted.
SELECT dblink_send_query('c', $$SELECT warren.degree('g2', 1::bigint), pg_sleep(1)$$);
SELECT pg_sleep(0.3);
SELECT * FROM dblink('d', $$SELECT warren.prewarm('g1')$$) AS t(version bigint);
SELECT assert.equal(shared_segments(:v2), :one, 'a pinned version is never evicted');
SELECT assert.equal(shared_segments(:v1), 0::bigint, 'g1 is loaded privately');
SELECT assert.equal(evicted, 1::bigint, 'd holds g1 in its own cache')
FROM dblink('d', $$SELECT warren.evict('g1')$$) AS t(evicted bigint);
SELECT count(*) FROM dblink_get_result('c') AS t(degree bigint, slept text);
SELECT count(*) FROM dblink_get_result('c') AS t(degree bigint, slept text);

SELECT dblink_disconnect(session) FROM unnest(ARRAY['a', 'b', 'c', 'd']) AS session;
\if :shared
ALTER SYSTEM RESET warren.shared_cache_memory;
SELECT pg_reload_conf();
SELECT pg_sleep(0.2);
\endif
