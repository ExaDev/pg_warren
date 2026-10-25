-- DESIGN.md §7: retention never deletes a version that has vectors bound
-- to it, even one a concurrent transaction writes while the promotion
-- runs; and a vector written to a version that retention deletes meanwhile
-- fails as for a version that does not exist (42704), or as a
-- serialization failure (40001) under REPEATABLE READ or SERIALIZABLE,
-- not with a raw foreign-key violation. Found by test/concurrent_refresh.py
-- (M11).
CREATE EXTENSION dblink;
CREATE TABLE nodes AS SELECT generate_series(1, 4)::bigint AS id;
CREATE TABLE edges AS SELECT id AS src, id % 4 + 1 AS dst FROM nodes;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges', retain_versions := 1);
SELECT warren.refresh('g') AS v1 \gset

SELECT format('dbname=%s user=%s host=%s port=%s', current_database(), session_user,
              split_part(current_setting('unix_socket_directories'), ',', 1),
              current_setting('port')) AS conninfo \gset
SELECT dblink_connect('writer', :'conninfo');
SELECT dblink_connect('refresher', :'conninfo');
SELECT pid AS refresher FROM dblink('refresher', 'SELECT pg_backend_pid()') AS t(pid int) \gset
SELECT pid AS writer FROM dblink('writer', 'SELECT pg_backend_pid()') AS t(pid int) \gset
-- until_waiting(pid): waits, up to 10 s, until backend pid waits on a lock.
CREATE FUNCTION until_waiting(pid int) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    FOR attempt IN 1..1000 LOOP
        IF EXISTS (SELECT FROM pg_stat_activity a WHERE a.pid = until_waiting.pid AND wait_event_type = 'Lock') THEN
            RETURN;
        END IF;
        PERFORM pg_sleep(0.01);
    END LOOP;
    RAISE EXCEPTION 'backend % never waited on a lock', pid;
END
$$;

-- 1. The writer binds a vector to v1 and has not committed when a
-- refresh's retention, which keeps one version, comes to delete v1.
SELECT dblink_exec('writer', 'BEGIN');
SELECT * FROM dblink('writer', format($$SELECT warren.vector_const('g', 'x', 1, version := %s)$$, :v1))
    AS t(id bigint);
SELECT dblink_send_query('refresher', $$SELECT warren.refresh('g')$$);
SELECT until_waiting(:refresher);
SELECT dblink_exec('writer', 'COMMIT');
SELECT * FROM dblink_get_result('refresher') AS t(version bigint);
SELECT count(*) FROM dblink_get_result('refresher') AS t(x text);  -- the empty result ending the query
SELECT assert.equal((SELECT count(*) FROM warren.vectors WHERE name = 'x' AND version_id = :v1), 1::bigint,
                    'the vector written meanwhile is kept');
SELECT assert.equal((SELECT count(*) FROM warren.versions WHERE version_id = :v1), 1::bigint,
                    'and so is the version it is bound to');
SELECT assert.equal(warren.vector_reduce('g', 'x', 'sum', :v1), 4::float8, 'the vector reads as written');
SELECT warren.drop_vector('g', 'x', :v1);

-- 2. Retention has deleted v2, uncommitted, when the writer binds a
-- vector to it: the write waits, then fails as for a missing version.
SELECT warren.refresh('g') AS v2 \gset
SELECT dblink_exec('refresher', 'BEGIN');
SELECT * FROM dblink('refresher', $$SELECT warren.refresh('g')$$) AS t(version bigint);
SELECT dblink_send_query('writer', format($$SELECT warren.vector_const('g', 'y', 1, version := %s)$$, :v2));
SELECT until_waiting(:writer);
SELECT dblink_exec('refresher', 'COMMIT');
SELECT assert.raises($$SELECT * FROM dblink_get_result('writer') AS t(id bigint)$$,
                     'the write fails as for a missing version', '42704');
SELECT dblink_error_message('writer') AS writer_error \gset
SELECT assert.that(:'writer_error' LIKE format('%%version %s no longer exists%%', :v2),
                   'and says so: ' || :'writer_error');
SELECT count(*) FROM dblink_get_result('writer') AS t(x text);  -- the empty result ending the query
SELECT assert.equal((SELECT count(*) FROM warren.vectors), 0::bigint, 'no vector is left');

-- 3. As case 2 under REPEATABLE READ, whose snapshot still sees v3: the
-- write waits, then fails with serialization_failure (retry).
SELECT warren.refresh('g') AS v3 \gset
SELECT dblink_exec('writer', 'BEGIN ISOLATION LEVEL REPEATABLE READ');
SELECT * FROM dblink('writer', format('SELECT count(*) FROM warren.versions WHERE version_id = %s', :v3))
    AS t(n bigint);
SELECT dblink_exec('refresher', 'BEGIN');
SELECT * FROM dblink('refresher', $$SELECT warren.refresh('g')$$) AS t(version bigint);
SELECT dblink_send_query('writer', format($$SELECT warren.vector_const('g', 'z', 1, version := %s)$$, :v3));
SELECT until_waiting(:writer);
SELECT dblink_exec('refresher', 'COMMIT');
SELECT assert.raises($$SELECT * FROM dblink_get_result('writer') AS t(id bigint)$$,
                     'the write fails as a serialization failure', '40001');
SELECT count(*) FROM dblink_get_result('writer') AS t(x text);  -- the empty result ending the query
SELECT dblink_exec('writer', 'ROLLBACK');
SELECT assert.equal((SELECT count(*) FROM warren.vectors), 0::bigint, 'no vector is left');

SELECT dblink_disconnect('writer');
SELECT dblink_disconnect('refresher');
