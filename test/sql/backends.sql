-- DESIGN.md §5.7: warren.backends() has a row per backend that has used
-- pg_warren, read from the activity table in shared memory, then a cluster
-- row that also counts backends that have exited. Counters count once per
-- statement and version. Other roles see a backend's pid, database, role
-- and start, and its sizes and counters only with the privileges of its
-- role or of pg_read_all_stats; the cluster row is everyone's. A backend's
-- role is its authenticated user; a parallel worker has no row. Without the
-- table (PG 15 and 16 without the preload) only the caller's own row shows,
-- with a NOTICE.
\ir include/shared_cache.sql
SELECT current_setting('server_version_num')::int >= 170000
       OR current_setting('shared_preload_libraries') ~ 'pg_warren' AS activity \gset
CREATE TABLE nodes AS SELECT generate_series(1, 1000)::bigint AS id;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT id, id % 1000 + 1 FROM nodes');
SELECT warren.refresh('g') AS version \gset
CREATE ROLE watcher;
CREATE ROLE monitor IN ROLE pg_read_all_stats;
GRANT USAGE ON SCHEMA assert TO watcher, monitor;  -- test plumbing

SELECT dblink_connect('a', :'conninfo');
SELECT pid AS a_pid FROM dblink('a', 'SELECT pg_backend_pid()') AS t(pid int) \gset
-- Three reads in one statement, then one in the next.
SELECT * FROM dblink('a', $$SELECT count(warren.degree('g', i::bigint)) FROM generate_series(1, 3) i$$)
    AS t(n bigint);
SELECT * FROM dblink('a', $$SELECT warren.degree('g', 1::bigint)$$) AS t(n bigint);

\if :activity
SELECT assert.equal(
    (SELECT format('%s %s %s', lookups, private_loads + shared_loads, private_hits + shared_hits)
     FROM warren.backends() WHERE pid = :a_pid),
    '2 1 1', 'a counted one load and one hit: once per statement, not per call');
SELECT assert.equal(
    (SELECT format('%s %s', shared_loads, refusals ->> 'unavailable')
     FROM warren.backends() WHERE pid = :a_pid),
    CASE WHEN :'shared' THEN '1 0' ELSE '0 1' END,
    'shared, or refused as unavailable and loaded privately');
SELECT assert.that((SELECT loaded_bytes > 0 AND load_seconds >= 0 AND since <= now()
                    FROM warren.backends() WHERE pid = :a_pid), 'a load counts its bytes');
SELECT assert.that((SELECT (mapped_bytes > 0) = :'shared' AND (private_bytes > 0) = NOT :'shared'
                    FROM warren.backends() WHERE pid = :a_pid),
                   'a maps the shared version, or holds its own copy');
SELECT assert.equal((SELECT count(*) FROM warren.backends() WHERE kind = 'cluster'), 1::bigint,
                    'one cluster row');

-- Privacy across roles.
SET ROLE watcher;
SELECT assert.equal(
    (SELECT format('%s %s %s %s', database IS NOT NULL, role, since IS NOT NULL, lookups)
     FROM warren.backends() WHERE pid = :a_pid),
    't postgres t ', 'another role sees who a is, not what it holds or counts');
SELECT assert.that((SELECT lookups >= 2 AND refusals IS NOT NULL FROM warren.backends()
                    WHERE kind = 'cluster'), 'the totals are everyone''s');
RESET ROLE;
SET ROLE monitor;
SELECT assert.equal((SELECT lookups FROM warren.backends() WHERE pid = :a_pid), 2::bigint,
                    'pg_read_all_stats sees everything');
RESET ROLE;

-- A backend's role is its authenticated user, as pg_stat_activity's
-- usesysid, even after SET SESSION AUTHORIZATION switches before its first
-- use of pg_warren.
SELECT dblink_connect('b', :'conninfo');
SELECT pid AS b_pid FROM dblink('b', 'SELECT pg_backend_pid()') AS t(pid int) \gset
SELECT dblink_exec('b', 'SET SESSION AUTHORIZATION watcher');
SELECT * FROM dblink('b', $$SELECT count(*) FROM warren.backends()$$) AS t(n bigint);
SELECT dblink_exec('b', 'RESET SESSION AUTHORIZATION');
SELECT dblink_exec('b', 'SET SESSION AUTHORIZATION monitor');
SELECT * FROM dblink('b', $$SELECT count(*) FROM warren.backends()$$) AS t(n bigint);
SELECT assert.equal(
    (SELECT role::oid FROM warren.backends() WHERE pid = :b_pid),
    (SELECT usesysid FROM pg_stat_activity WHERE pid = :b_pid),
    'the role pg_stat_activity shows');
SELECT dblink_disconnect('b');

-- A parallel worker reports nothing: it claims no slot.
-- The setting that runs a query's plan in a worker; PG 16 renamed it.
SELECT CASE WHEN current_setting('server_version_num')::int >= 160000
            THEN 'debug_parallel_query' ELSE 'force_parallel_mode' END AS force \gset
SELECT dblink_connect('c', :'conninfo');
SELECT dblink_exec('c', format('SET %s = on', :'force'));
SELECT dblink_send_query('c', $$SELECT sum(warren.degree('g', 1::bigint)) FROM generate_series(1, 30) i
                                WHERE pg_sleep(0.1) IS NOT NULL$$);
CREATE TABLE worker_rows (n bigint);
DO $$
BEGIN
    FOR attempt IN 1..300 LOOP
        PERFORM pg_stat_clear_snapshot();  -- pg_stat_activity as of now
        INSERT INTO worker_rows
        SELECT count(*) FROM warren.backends() b JOIN pg_stat_activity a USING (pid)
        WHERE a.backend_type = 'parallel worker';
        EXIT WHEN dblink_is_busy('c') = 0;
        PERFORM pg_sleep(0.01);
    END LOOP;
END
$$;
SELECT * FROM dblink_get_result('c') AS t(n numeric);
SELECT count(*) FROM dblink_get_result('c') AS t(x text);  -- the empty result ending the query
SELECT assert.equal((SELECT max(n) FROM worker_rows), 0::bigint, 'no worker has a row');
SELECT dblink_disconnect('c');

-- An exited backend's counters stay in the totals.
SELECT lookups AS before FROM warren.backends() WHERE kind = 'cluster' \gset
SELECT set_config('test.a_pid', :'a_pid', false);
SELECT dblink_disconnect('a');
DO $$
BEGIN
    FOR attempt IN 1..500 LOOP
        EXIT WHEN NOT EXISTS (SELECT FROM warren.backends()
                              WHERE pid = current_setting('test.a_pid', true)::int);
        PERFORM pg_sleep(0.01);
    END LOOP;
END
$$;
SELECT assert.equal((SELECT count(*) FROM warren.backends() WHERE pid = :a_pid), 0::bigint,
                    'a gave its slot back');
SELECT assert.that((SELECT lookups >= :before FROM warren.backends() WHERE kind = 'cluster'),
                   'and its counters joined the totals');
\else
SELECT assert.equal(
    (SELECT string_agg(format('%s %s', kind, pid = pg_backend_pid()), ', ') FROM warren.backends()),
    'backend t', 'without the table, only this backend''s own row');
SELECT dblink_disconnect('a');
\endif
REVOKE USAGE ON SCHEMA assert FROM watcher, monitor;
DROP ROLE watcher;
DROP ROLE monitor;
