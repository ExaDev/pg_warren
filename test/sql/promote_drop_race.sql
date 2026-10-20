-- A promote that finds its graph, then waits on the graph's row while
-- another session drops the graph, must fail cleanly: graph does not
-- exist (42704), not an internal error. Session b drops the graph and
-- holds the transaction open; session a's promote finds the graph in its
-- snapshot and blocks on the row lock; when b commits, the lock finds no
-- row.
CREATE EXTENSION dblink;
CREATE TABLE nodes AS SELECT generate_series(1, 4)::bigint AS id;
CREATE TABLE edges AS SELECT id AS src, id % 4 + 1 AS dst FROM nodes;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('g');
SELECT warren.refresh('g', promote := false) AS staged \gset

SELECT format('dbname=%s user=%s host=%s port=%s', current_database(), session_user,
              split_part(current_setting('unix_socket_directories'), ',', 1),
              current_setting('port')) AS conninfo \gset
SELECT dblink_connect('a', :'conninfo');
SELECT dblink_connect('b', :'conninfo');

SELECT dblink_exec('b', 'BEGIN');
SELECT * FROM dblink('b', $$SELECT warren.drop_graph('g')$$) AS t(x text);
SELECT dblink_send_query('a', format($$SELECT warren.promote('g', %s)$$, :staged));
SELECT pg_sleep(0.5);
SELECT assert.equal(dblink_is_busy('a'), 1, 'the promote waits on the graph''s row');
SELECT dblink_exec('b', 'COMMIT');

SELECT count(*) FROM dblink_get_result('a', false) AS t(x text);
SELECT dblink_error_message('a') AS a_error \gset
SELECT assert.that(:'a_error' LIKE '%graph "g" does not exist%',
                   'the promote fails as for a missing graph: ' || :'a_error');
SELECT dblink_disconnect('a');
SELECT dblink_disconnect('b');
SELECT assert.equal((SELECT count(*) FROM warren.graphs), 0::bigint, 'the graph is gone');
