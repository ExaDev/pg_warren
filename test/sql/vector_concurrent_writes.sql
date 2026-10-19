-- DESIGN.md §6.2: two transactions writing one vector name at one version.
-- The second waits for the first, then fails with serialization_failure
-- (retry), not a raw unique_violation; the first's vector is kept.
-- fixture: random --nodes 50 --edges 100 --seed 1
CREATE EXTENSION dblink;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('g');
SELECT warren.vector_const('g', 'x', 0);

SELECT format('dbname=%s host=%s port=%s', current_database(),
              split_part(current_setting('unix_socket_directories'), ',', 1),
              current_setting('port')) AS conninfo \gset
SELECT dblink_connect('first', :'conninfo');
SELECT dblink_connect('second', :'conninfo');

SELECT dblink_exec('first', 'BEGIN');
SELECT * FROM dblink('first', $$SELECT warren.vector_const('g', 'x', 1)$$) AS t(id bigint);
-- The second writer blocks on the first's deletion of the old row.
SELECT dblink_send_query('second', $$SELECT warren.vector_const('g', 'x', 2)$$);
SELECT pg_sleep(0.5);
SELECT assert.equal(dblink_is_busy('second'), 1, 'the second writer waits for the first');
SELECT dblink_exec('first', 'COMMIT');

SELECT count(*) FROM dblink_get_result('second', false) AS t(id bigint);
SELECT dblink_error_message('second') AS second_error \gset
SELECT assert.that(:'second_error' LIKE '%vector "x" at version % was written by a concurrent transaction; retry%',
                   'the second writer is told to retry: ' || :'second_error');
SELECT assert.equal(
    (SELECT array_agg(DISTINCT value) FROM warren.vector_values('g', 'x', NULL::bigint)),
    ARRAY[1]::float8[], 'the first writer''s vector is kept');
SELECT assert.equal((SELECT count(*) FROM warren.vectors WHERE name = 'x'), 1::bigint, 'one row for x');
SELECT dblink_disconnect('first');
SELECT dblink_disconnect('second');
