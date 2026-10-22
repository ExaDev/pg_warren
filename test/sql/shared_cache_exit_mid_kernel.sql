-- DESIGN.md §5.6, §6.9: a backend terminated while a compute kernel's
-- threads read a shared version exits cleanly. The FATAL exits from the
-- main thread's poll without unwinding; the exit callback cancels the
-- kernel and waits for its threads before Postgres unmaps the version's
-- segment, so no thread reads unmapped memory and the postmaster does not
-- restart the cluster. Where the shared cache is off or unavailable the
-- version is private, and the same holds.
\ir include/shared_cache.sql
CREATE TABLE nodes AS SELECT generate_series(1, 300003)::bigint AS id;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT id, id % 300003 + 1 FROM nodes');
SELECT warren.refresh('g') AS v \gset
SELECT CASE WHEN :'shared' THEN 1 ELSE 0 END::bigint AS one \gset
SELECT pg_postmaster_start_time() AS started \gset
SELECT dblink_connect('a', :'conninfo');
SELECT pid AS kernel FROM dblink('a', 'SELECT pg_backend_pid()') AS t(pid int) \gset

-- a runs betweenness centrality over g on its compute threads.
SELECT dblink_exec('a', 'SET warren.threads = 4');
SELECT dblink_send_query('a',
    $$SELECT warren.betweenness_centrality('g', 'bc', k := 3000, persist := false)$$);
DO $$
BEGIN
    FOR attempt IN 1..3000 LOOP
        EXIT WHEN EXISTS (SELECT FROM pg_stat_activity
                          WHERE query LIKE '%betweenness_centrality%' AND state = 'active'
                            AND pid <> pg_backend_pid());
        PERFORM pg_sleep(0.01);
    END LOOP;
END
$$;
SELECT pg_sleep(0.5);
SELECT assert.equal(shared_segments(:v), :one, 'the kernel reads the shared version');
SELECT assert.equal(dblink_is_busy('a'), 1, 'the kernel is still running');

-- a is terminated mid-kernel; the cluster carries on.
SELECT pg_terminate_backend(:kernel, 10000);
SELECT pg_sleep(0.5);
\c
SELECT assert.equal(pg_postmaster_start_time(), :'started'::timestamptz,
                    'the cluster did not restart');
SELECT assert.equal(warren.degree('g', 1::bigint, direction := 'both'), 2::bigint,
                    'a new backend reads g');
