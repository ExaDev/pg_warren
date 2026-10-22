-- DESIGN.md §5.6, crash safety: a backend that waits for another's load
-- takes the load over if the loader's process is gone, here terminated
-- while it fills the version's segment; the killed loader's segment, never
-- pinned, is freed with its process, and one segment holds the version
-- after. Where the shared cache is off or unavailable, both load privately.
\ir include/shared_cache.sql
CREATE TABLE nodes AS SELECT generate_series(1, 2000003)::bigint AS id;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT id, id % 2000003 + 1 FROM nodes');
SELECT warren.refresh('g') AS v \gset
SELECT set_config('test.version', :'v', false);
SELECT CASE WHEN :'shared' THEN 1 ELSE 0 END::bigint AS one \gset
SELECT dblink_connect(session, :'conninfo') FROM unnest(ARRAY['a', 'b']) AS session;
SELECT pid AS loader FROM dblink('a', 'SELECT pg_backend_pid()') AS t(pid int) \gset

-- a starts loading g; once its segment exists, b waits for it.
SELECT dblink_send_query('a', $$SELECT warren.prewarm('g')$$);
\if :shared
DO $$
BEGIN
    FOR attempt IN 1..3000 LOOP
        EXIT WHEN shared_segments(current_setting('test.version')::bigint) > 0;
        PERFORM pg_sleep(0.01);
    END LOOP;
END
$$;
\endif
SELECT assert.equal(dblink_is_busy('a'), 1, 'a is still loading g');
SELECT dblink_send_query('b', $$SELECT warren.prewarm('g')$$);
SELECT pg_sleep(0.2);

-- a is terminated mid-load; b takes the load over.
SELECT pg_terminate_backend(:loader, 10000);
SELECT assert.equal(version, :v::bigint, 'b loaded g')
FROM dblink_get_result('b') AS t(version bigint);
SELECT count(*) FROM dblink_get_result('b') AS t(version bigint);
SELECT assert.equal(shared_segments(:v), :one, 'one segment holds g, the loader''s freed');
SELECT assert.equal(warren.degree('g', 1::bigint, direction := 'both'), 2::bigint,
                    'this backend reads g');

SELECT dblink_disconnect('b');
