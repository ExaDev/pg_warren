-- DESIGN.md §7 ownership under concurrency: a writer checks the vector it
-- replaces when it opens the version, so a same-named vector another role
-- commits while the writer computes must not be deleted unchecked. Here
-- reader_b's slow vector_map starts with no vector "score"; reader_a's
-- vector_const commits one mid-way; reader_b must fail with
-- serialization_failure (retry), and reader_a's vector must survive.
-- fixture: random --nodes 50 --edges 100 --seed 1
CREATE EXTENSION dblink;
CREATE ROLE g_owner;
CREATE ROLE reader_a;
CREATE ROLE reader_b;
GRANT SELECT ON nodes, edges TO g_owner;
SET ROLE g_owner;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
RESET ROLE;
SELECT warren.refresh('g');
SELECT warren.grant('g', 'reader_a');
SELECT warren.grant('g', 'reader_b');
SELECT warren.vector_dense_id('g', 'i');
-- Sleeps once, at dense id 0, so the map takes about two seconds.
CREATE FUNCTION public.slow(x float8) RETURNS float8 LANGUAGE plpgsql VOLATILE AS $$
BEGIN
    IF x = 0 THEN PERFORM pg_sleep(2); END IF;
    RETURN x;
END $$;

SELECT format('dbname=%s user=%s host=%s port=%s', current_database(), session_user,
              split_part(current_setting('unix_socket_directories'), ',', 1),
              current_setting('port')) AS conninfo \gset
SELECT dblink_connect('a', :'conninfo');
SELECT dblink_connect('b', :'conninfo');
SELECT dblink_exec('a', 'SET ROLE reader_a');
SELECT dblink_exec('b', 'SET ROLE reader_b');
SELECT dblink_exec('b', 'SET search_path = public');

SELECT dblink_send_query('b', $$SELECT warren.vector_map('g', 'score', 'slow(i::float8)')$$);
SELECT pg_sleep(0.5);
SELECT assert.equal(dblink_is_busy('b'), 1, 'reader_b is still computing');
SELECT * FROM dblink('a', $$SELECT warren.vector_const('g', 'score', 1)$$) AS t(id bigint);

SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);
SELECT dblink_error_message('b') AS b_error \gset
SELECT assert.that(:'b_error' LIKE '%vector "score" at version % was written by a concurrent transaction; retry%',
                   'reader_b is told to retry: ' || :'b_error');
SELECT assert.equal((SELECT string_agg(created_by::text, ',') FROM warren.vectors WHERE name = 'score'),
                    'reader_a', 'reader_a''s vector survives');
SELECT assert.equal(
    (SELECT array_agg(DISTINCT value) FROM warren.vector_values('g', 'score', NULL::bigint)),
    ARRAY[1]::float8[], 'with reader_a''s values');
SELECT dblink_disconnect('a');
SELECT dblink_disconnect('b');
