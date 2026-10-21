-- warren.contract holds its parent graph's row FOR SHARE while it reads
-- the parent version, and checks the version still exists once it holds
-- it (DESIGN.md §6.6; PR #28 review): a drop_version in another session
-- either waits for the contraction or, if it went first, makes the
-- contraction fail cleanly as an unknown version, never with a raw
-- foreign-key error or a quotient unlinked from birth.
CREATE EXTENSION dblink;
CREATE TABLE nodes AS SELECT generate_series(1, 6)::bigint AS id;
CREATE TABLE edges AS SELECT id AS src, id % 6 + 1 AS dst FROM nodes;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges', retain_versions := 10);
SELECT warren.refresh('g');
SELECT warren.refresh('g', promote := false) AS staged \gset
SELECT warren.vector_from_query('g', 'l', 'SELECT id, id % 2 FROM nodes', version := :staged);

SELECT format('dbname=%s user=%s host=%s port=%s', current_database(), session_user,
              split_part(current_setting('unix_socket_directories'), ',', 1),
              current_setting('port')) AS conninfo \gset
SELECT dblink_connect('b', :'conninfo');

-- 1. b's contraction of the staged version waits on this session's drop
-- of it, then fails as an unknown version.
BEGIN;
SELECT warren.drop_version('g', :staged);
SELECT dblink_send_query('b', format($$SELECT warren.contract('g', 'l', 'q', version := %s)$$, :staged));
SELECT pg_sleep(0.5);
SELECT assert.equal(dblink_is_busy('b'), 1, 'the contraction waits on the parent graph''s row');
COMMIT;
SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);
SELECT dblink_error_message('b') AS b_error \gset
SELECT assert.that(:'b_error' LIKE format('%%graph "g" has no version %s%%', :staged),
                   'the contraction finds the version gone: ' || :'b_error');
SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);  -- the empty result ending the query
SELECT assert.equal((SELECT count(*) FROM warren.graphs WHERE name = 'q'), 0::bigint, 'no quotient is made');

-- 2. b's drop of the version this session is contracting waits for it.
SELECT warren.refresh('g', promote := false) AS staged \gset
SELECT warren.vector_from_query('g', 'l', 'SELECT id, id % 2 FROM nodes', version := :staged);
BEGIN;
SELECT warren.contract('g', 'l', 'q', version := :staged) AS quotient \gset
SELECT dblink_send_query('b', format($$SELECT warren.drop_version('g', %s)$$, :staged));
SELECT pg_sleep(0.5);
SELECT assert.equal(dblink_is_busy('b'), 1, 'the drop waits on the contraction');
COMMIT;
SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);
SELECT count(*) FROM dblink_get_result('b', false) AS t(x text);  -- the empty result ending the query
SELECT assert.equal((SELECT parent_version FROM warren.versions WHERE version_id = :quotient), NULL::bigint,
                    'the drop, made after, unlinks the quotient');

SELECT dblink_disconnect('b');
