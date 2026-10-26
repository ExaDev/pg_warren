-- DESIGN.md §6.2, §5.5: DISCARD ALL, which a session pooler such as
-- PgBouncer runs between clients (server_reset_query), frees this
-- backend's transient vectors, of every role, and forgets last_stats(),
-- so the next client never sees the last one's. Other DISCARDs keep
-- them, and a DISCARD ALL that is refused changes nothing.
CREATE TABLE discard_nodes AS SELECT generate_series(1, 10)::bigint AS id;
SELECT warren.create_graph('d', 'SELECT id FROM discard_nodes', 'SELECT id, id % 10 + 1 FROM discard_nodes');
SELECT warren.refresh('d');
CREATE ROLE discard_reader;
SELECT warren.grant('d', 'discard_reader');
GRANT USAGE ON SCHEMA assert TO discard_reader;

SET ROLE discard_reader;
SELECT warren.vector_const('d', 'theirs', 1, persist := false);
RESET ROLE;
SELECT warren.vector_const('d', 'mine', 1, persist := false);
SELECT count(*) FROM warren.bfs('d', ARRAY[1::bigint], 2);
SELECT assert.equal(warren.last_stats() ->> 'function', 'bfs', 'a traversal records its stats');

DISCARD PLANS;
DISCARD TEMP;
SELECT assert.equal(warren.vector_reduce('d', 'mine', 'sum'), 10::float8, 'other DISCARDs keep transient vectors');
SELECT assert.equal(warren.last_stats() ->> 'function', 'bfs', 'other DISCARDs keep last_stats()');
SELECT assert.raises($$DISCARD ALL$$, 'DISCARD ALL cannot run in a function', '25001');
-- A utility statement's ERROR passes through pg_warren's hook (and,
-- preloaded, pg_stat_statements' before it) as it would without them.
SELECT assert.raises($$CREATE TABLE discard_nodes ()$$, 'a failing utility statement', '42P07');
SELECT assert.equal(warren.vector_reduce('d', 'mine', 'sum'), 10::float8, 'a refused DISCARD ALL keeps them');

DISCARD ALL;
SELECT assert.equal((SELECT transient_bytes FROM warren.backends() WHERE pid = pg_backend_pid()), 0::bigint,
                    'DISCARD ALL frees the transient vectors of every role');
SELECT assert.raises($$SELECT warren.vector_reduce('d', 'mine', 'sum')$$, 'the transient vector is gone', '42704');
SELECT assert.that(warren.last_stats() IS NULL, 'DISCARD ALL forgets last_stats()');
SET ROLE discard_reader;
SELECT assert.raises($$SELECT warren.vector_reduce('d', 'theirs', 'sum')$$, 'another role''s is gone', '42704');
RESET ROLE;
SELECT warren.drop_graph('d');
REVOKE USAGE ON SCHEMA assert FROM discard_reader;
DROP ROLE discard_reader;
