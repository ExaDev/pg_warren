-- GraphDefinition::lock re-reads a graph's current version after taking
-- its row lock. When the lock has waited for another session's change of
-- current_version, the re-read must see the new current version as the
-- graph's own, not as a stray one (DESIGN.md §5.1). This session changes
-- current_version in an open transaction; session b's call waits on the
-- graph's row, and runs on when this session commits. Each sent query is
-- read to its end (dblink_get_result until the empty result) before the
-- next one.
CREATE EXTENSION dblink;
\ir include/blocked_by_me.sql
CREATE TABLE nodes AS SELECT generate_series(1, 4)::bigint AS id;
CREATE TABLE edges AS SELECT id AS src, id % 4 + 1 AS dst FROM nodes;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges', retain_versions := 10);
SELECT warren.refresh('g');
SELECT warren.refresh('g', promote := false) AS staged \gset

SELECT format('dbname=%s user=%s host=%s port=%s', current_database(), session_user,
              split_part(current_setting('unix_socket_directories'), ',', 1),
              current_setting('port')) AS conninfo \gset
SELECT dblink_connect('b', :'conninfo');
SELECT pid AS b_pid FROM dblink('b', 'SELECT pg_backend_pid()') AS t(pid int) \gset

-- 1. b's refresh waits on this session's promote.
BEGIN;
SELECT warren.promote('g', :staged);
SELECT dblink_send_query('b', $$SELECT warren.refresh('g')$$);
SELECT assert_blocked_by_me(:b_pid, 'the refresh waits on the graph''s row');
COMMIT;
SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);
SELECT assert.equal(dblink_error_message('b'), 'OK', 'a refresh after a promote succeeds');
SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);  -- the empty result ending the query

-- 2. b's compact waits on this session's refresh.
BEGIN;
SELECT warren.refresh('g');
SELECT dblink_send_query('b', $$SELECT warren.compact('g')$$);
SELECT assert_blocked_by_me(:b_pid, 'the compact waits on the graph''s row');
COMMIT;
SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);
SELECT assert.equal(dblink_error_message('b'), 'OK', 'a compact after a refresh succeeds');
SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);  -- the empty result ending the query

-- 3. b's drop_version of a version this session promotes is refused as
-- dropping the current version, by drop_version itself.
SELECT warren.refresh('g', promote := false) AS staged \gset
BEGIN;
SELECT warren.promote('g', :staged);
SELECT dblink_send_query('b', format($$SELECT warren.drop_version('g', %s)$$, :staged));
SELECT assert_blocked_by_me(:b_pid, 'the drop waits on the graph''s row');
COMMIT;
SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);
SELECT dblink_error_message('b') AS b_error \gset
SELECT assert.that(:'b_error' LIKE '%cannot drop version % of graph "g": it is the current version%',
                   'drop_version refuses the new current version: ' || :'b_error');
SELECT assert.equal((SELECT current_version FROM warren.graphs WHERE name = 'g'), :staged::bigint,
                    'the promoted version is current');

-- 4. b's compact without an order waits on this session's compact in
-- locality order, and then follows the order that compact recorded: the
-- graph's node order describes its id space (DESIGN.md §14 item 22).
SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);  -- the empty result ending case 3
BEGIN;
SELECT warren.compact('g', 'locality') AS local \gset
SELECT dblink_send_query('b', $$SELECT warren.compact('g')$$);
SELECT assert_blocked_by_me(:b_pid, 'the compact waits on the graph''s row');
COMMIT;
SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);
SELECT assert.equal(dblink_error_message('b'), 'OK', 'a compact after a compact succeeds');
SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);  -- the empty result ending the query
SELECT assert.equal(
    (SELECT array_agg(node_id || ':' || dense_id ORDER BY node_id)
     FROM warren.id_map('g', NULL::bigint)),
    (SELECT array_agg(node_id || ':' || dense_id ORDER BY node_id)
     FROM warren.id_map('g', NULL::bigint, :local)),
    'the waiting compact kept the locality order');
SELECT assert.that((SELECT current_version FROM warren.graphs WHERE name = 'g') > :local,
                   'the waiting compact made a new version');

SELECT dblink_disconnect('b');
