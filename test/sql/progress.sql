-- DESIGN.md §5.7: warren.progress() shows each backend's command and the
-- operation inside it, from the activity table, as other sessions see them
-- mid-run: a refresh waiting in its edge query, a PL/pgSQL program that
-- reports its own progress, a library function and the native calls
-- inside it, and eigen's Lanczos steps. Another role sees what runs, not
-- how far; the frames are gone once the statement ends. Without the table
-- (PG 15 and 16 without the preload) only the caller's own row shows.
\ir include/shared_cache.sql
SELECT current_setting('server_version_num')::int >= 170000
       OR current_setting('shared_preload_libraries') ~ 'pg_warren' AS activity \gset
CREATE TABLE nodes AS SELECT generate_series(1, 20000)::bigint AS id;
CREATE TABLE edges AS
    SELECT id AS src, id % 20000 + 1 AS dst FROM nodes
    UNION ALL SELECT id, (id * 7919) % 20000 + 1 FROM nodes;
SELECT warren.create_graph('g', 'SELECT id FROM nodes',
                           'SELECT src, dst FROM edges, (SELECT pg_advisory_xact_lock_shared(7)) AS waited',
                           directed := false);
SELECT warren.refresh('g');
CREATE ROLE watcher;
CREATE ROLE monitor IN ROLE pg_read_all_stats;
GRANT USAGE ON SCHEMA assert TO watcher, monitor;  -- test plumbing

SELECT dblink_connect('a', :'conninfo');
SELECT pid AS a_pid FROM dblink('a', 'SELECT pg_backend_pid()') AS t(pid int) \gset
SELECT set_config('test.a_pid', :'a_pid', false);

-- seen: every row of a's progress while a runs, polled until a is idle.
CREATE TABLE seen AS SELECT * FROM warren.progress() WHERE false;
CREATE FUNCTION watch_a() RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    TRUNCATE seen;
    FOR attempt IN 1..20000 LOOP
        INSERT INTO seen SELECT * FROM warren.progress()
                         WHERE pid = current_setting('test.a_pid')::int;
        EXIT WHEN dblink_is_busy('a') = 0;
        PERFORM pg_sleep(0.001);
    END LOOP;
END
$$;
-- until_a(condition): waits, up to 10 s, until a's row meets condition.
CREATE FUNCTION until_a(condition text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    FOR attempt IN 1..1000 LOOP
        EXECUTE format('SELECT EXISTS (SELECT FROM warren.progress() WHERE pid = %s AND (%s))',
                       current_setting('test.a_pid'), condition) INTO STRICT found;
        IF found THEN
            RETURN;
        END IF;
        PERFORM pg_sleep(0.01);
    END LOOP;
    RAISE EXCEPTION 'assertion failed: a''s progress never met %', condition;
END
$$;

\if :activity
-- 1. A refresh waiting in its edge query: its command is its operation.
SELECT pg_advisory_lock(7);
SELECT dblink_send_query('a', $$SELECT warren.refresh('g')$$);
SELECT until_a($$operation_phase = 'reading edges'$$);
SELECT assert.equal(
    (SELECT format('%s %s %s %s %s', command, graph, phase, done, operation)
     FROM warren.progress() WHERE pid = :a_pid),
    'refresh g reading edges 0 refresh', 'the refresh reads its edges');
SELECT assert.that((SELECT total > 0 AND command_start <= now() AND version IS NULL
                    FROM warren.progress() WHERE pid = :a_pid),
                   'of the planner''s estimate, before the version is recorded');
SELECT pg_advisory_unlock(7);
SELECT * FROM dblink_get_result('a') AS t(version bigint);
SELECT count(*) FROM dblink_get_result('a') AS t(x text);  -- the empty result ending the query
SELECT assert.equal((SELECT count(*) FROM warren.progress() WHERE pid = :a_pid), 0::bigint,
                    'the frame ends with the refresh');

-- 2. A PL/pgSQL program reports its own progress; other roles see what it
-- runs, not how far, unless they have its role's or pg_read_all_stats'
-- privileges; the frame ends with the statement.
SELECT pg_advisory_lock(8);
SELECT dblink_send_query('a', $send$DO $do$ BEGIN
    PERFORM warren.report_progress('mine', 'g', 'step 1', 1, 3);
    PERFORM pg_advisory_xact_lock_shared(8);
END $do$$send$);
SELECT until_a($$command = 'mine'$$);
SELECT assert.equal(
    (SELECT format('%s %s %s %s %s', graph, phase, done, total, coalesce(operation, '-'))
     FROM warren.progress() WHERE pid = :a_pid),
    'g step 1 1 3 -', 'the command a reported');
SET ROLE watcher;
SELECT assert.equal(
    (SELECT format('%s %s %s %s', command, graph IS NULL, phase IS NULL, command_start IS NOT NULL)
     FROM warren.progress() WHERE pid = :a_pid),
    'mine t t t', 'another role sees what a runs and since when');
RESET ROLE;
SET ROLE monitor;
SELECT assert.equal((SELECT phase FROM warren.progress() WHERE pid = :a_pid), 'step 1',
                    'pg_read_all_stats sees how far');
RESET ROLE;
SELECT pg_advisory_unlock(8);
SELECT * FROM dblink_get_result('a') AS t(x text);
SELECT count(*) FROM dblink_get_result('a') AS t(x text);  -- the empty result ending the query
SELECT assert.equal((SELECT count(*) FROM warren.progress() WHERE pid = :a_pid), 0::bigint,
                    'the command ends with the statement');

-- 3. louvain reports its levels, and the native calls inside it their own.
SELECT dblink_send_query('a', $$SELECT warren.louvain('g', 'communities')$$);
SELECT watch_a();
SELECT * FROM dblink_get_result('a') AS t(q float8);
SELECT count(*) FROM dblink_get_result('a') AS t(x text);  -- the empty result ending the query
SELECT assert.that((SELECT count(*) > 0 FROM seen WHERE command = 'louvain' AND graph = 'g'
                    AND phase LIKE 'level %' AND total = 100),
                   'louvain reports its levels of max_levels');
SELECT assert.that((SELECT count(*) > 0 FROM seen WHERE command = 'louvain'
                    AND operation IN ('local_moving', 'contract')),
                   'and the native call running inside it');

-- 4. eigen reports its Lanczos steps of max_iter, and their counters.
SELECT dblink_send_query('a', $$SELECT * FROM warren.eigen('g', 6, 'laplacian', 'smallest',
                                                          tol := 1e-10, persist := false)$$);
SELECT watch_a();
SELECT count(*) FROM dblink_get_result('a') AS t(i int, eigenvalue float8, residual float8);
SELECT count(*) FROM dblink_get_result('a') AS t(x text);  -- the empty result ending the query
SELECT assert.that((SELECT count(*) > 0 FROM seen WHERE command = 'eigen' AND operation_phase = 'lanczos'
                    AND operation_done <= operation_total AND detail ? 'converged'),
                   'eigen reports its steps');
SELECT dblink_disconnect('a');
\else
DO $$ BEGIN
    PERFORM warren.report_progress('mine', 'g', 'step 1');
    PERFORM assert.equal(
        (SELECT string_agg(format('%s %s', command, pid = pg_backend_pid()), ', ') FROM warren.progress()),
        'mine t', 'without the table, only this backend''s own row');
END $$;
SELECT assert.equal((SELECT count(*) FROM warren.progress()), 0::bigint,
                    'and its command ended with its statement');
SELECT dblink_disconnect('a');
\endif
REVOKE USAGE ON SCHEMA assert FROM watcher, monitor;
DROP ROLE watcher;
DROP ROLE monitor;
