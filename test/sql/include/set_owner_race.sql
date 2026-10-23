-- One case of set_owner_race.sql: this session gives graph :graph to bob
-- while session b (as alice) runs :call, which waits on the graph's row and
-- must then be refused, naming :action; then :graph is given back to alice.
SELECT (SELECT count(*) FROM warren.versions) AS versions_before \gset
BEGIN;
SELECT warren.set_owner(:'graph', 'bob');
SELECT dblink_send_query('b', :'call');
SELECT pg_sleep(0.5);
SELECT assert.equal(dblink_is_busy('b'), 1, :'action' || ' waits on the graph''s row');
COMMIT;
SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);
SELECT dblink_error_message('b') AS b_error \gset
SELECT assert.that(
    :'b_error' LIKE format('%%must be a member of role "bob" to %s graph "%s"%%', :'action', :'graph'),
    :'action' || ' after the transfer is refused: ' || :'b_error');
SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);  -- the empty result ending the query
SELECT assert.equal((SELECT count(*) FROM warren.versions), :versions_before::bigint,
                    :'action' || ' changed no version');
SELECT warren.set_owner(:'graph', 'alice');
