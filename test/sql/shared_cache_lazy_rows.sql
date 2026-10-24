-- DESIGN.md §12: `components`, `minimum_spanning_forest`, `vector_topk`
-- and `most_similar` make each row's external id when the row is
-- returned, holding the loaded version between rows. A caller that stops
-- early (LIMIT), reads through a cursor across statements while the
-- version is evicted, or is cancelled mid-scan gets the rows it would
-- have, and the version is released when the call ends, however it ends.
-- Where the shared cache is off or unavailable, the version is private.
\ir include/shared_cache.sql
CREATE TABLE nodes AS SELECT n, 'https://example.org/resource/' || n AS id FROM generate_series(1, 20000) n;
SELECT warren.create_graph('g', 'SELECT id FROM nodes',
                           $q$SELECT a.id, b.id FROM nodes a JOIN nodes b ON b.n = a.n + 1$q$);
SELECT warren.refresh('g') AS v1 \gset
SELECT warren.vector_from_query('g', 'v', 'SELECT id, n::float8 FROM nodes');
SELECT CASE WHEN :'shared' THEN 1 ELSE 0 END::bigint AS one \gset

-- What this backend still holds of the version a call read.
CREATE FUNCTION holding() RETURNS bigint LANGUAGE sql AS $$
    SELECT count(*) FROM warren.cache() WHERE kind = 'version' AND state IN ('in use', 'held')
$$;

-- Callers that stop early, in the select list and in FROM.
SELECT assert.equal(count(*), 3::bigint, 'components under LIMIT')
FROM (SELECT warren.components('g', NULL::text) LIMIT 3) s;
SELECT assert.equal(count(*), 3::bigint, 'the forest under LIMIT')
FROM (SELECT warren.minimum_spanning_forest('g', NULL::text) LIMIT 3) s;
SELECT assert.equal(count(*), 3::bigint, 'the top k under LIMIT')
FROM (SELECT warren.vector_topk('g', 'v', 20000, NULL::text) LIMIT 3) s;
SELECT assert.equal(count(*), 1::bigint, 'most_similar under LIMIT')
FROM (SELECT warren.most_similar('g', 'https://example.org/resource/2'::text, k := 100) LIMIT 1) s;
SELECT assert.equal(holding(), 0::bigint, 'a call stopped early holds no version');

-- The rows of an open cursor, after its statement's pin has ended.
CREATE FUNCTION drain(c refcursor) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    r record;
    n bigint := 0;
BEGIN
    LOOP
        FETCH c INTO r;
        EXIT WHEN NOT FOUND;
        IF r.node_id LIKE 'https://example.org/resource/%' AND r.component_id = 0 THEN
            n := n + 1;
        END IF;
    END LOOP;
    RETURN n;
END
$$;
BEGIN;
DECLARE c CURSOR FOR
    SELECT (x).node_id, (x).component_id FROM (SELECT warren.components('g', NULL::text) AS x OFFSET 0) s;
MOVE 2 IN c;
SELECT warren.evict('g');
SELECT assert.equal(holding() + shared_segments(:v1), 1::bigint,
                    'the evicted version is held for the cursor, privately or mapped');
SELECT assert.equal(drain('c'), 19998::bigint, 'the cursor reads the rest of the evicted version');
COMMIT;
SELECT assert.equal(holding(), 0::bigint, 'a closed cursor holds no version');

-- A call that errors mid-scan, at its 5,000th row.
CREATE SEQUENCE rows_read;
SELECT assert.raises($$SELECT count(1 / (5000 - nextval('rows_read'))) FROM (SELECT warren.components('g', NULL::text) OFFSET 0) s$$,
                     'components erroring mid-scan', '22012');
SELECT setval('rows_read', 1, false);
SELECT assert.raises($$SELECT count(1 / (5000 - nextval('rows_read'))) FROM (SELECT warren.minimum_spanning_forest('g', NULL::text) OFFSET 0) s$$,
                     'the forest erroring mid-scan', '22012');
SELECT assert.equal(holding(), 0::bigint, 'a call that errored holds no version');

-- A call cancelled mid-scan, between rows, in another session.
SELECT dblink_connect('b', :'conninfo');
SELECT pid FROM dblink('b', 'SELECT pg_backend_pid()') AS t(pid int) \gset
SELECT dblink_send_query('b', $$SELECT count(pg_sleep(0.001)) FROM (SELECT warren.components('g', NULL::text) OFFSET 0) s$$);
SELECT pg_sleep(1);
SELECT assert.that(pg_cancel_backend(:pid), 'the scan is cancelled');
SELECT assert.equal(count(*), 0::bigint, 'the cancelled scan returns no count')
FROM dblink_get_result('b', false) AS t(n bigint);
SELECT assert.that(dblink_error_message('b') LIKE '%canceling statement due to user request%',
                   'components cancelled mid-scan: ' || dblink_error_message('b'));
SELECT * FROM dblink_get_result('b') AS t(n bigint);
SELECT assert.equal(h, 0::bigint, 'a cancelled call holds no version')
FROM dblink('b', $$SELECT count(*) FROM warren.cache() WHERE kind = 'version' AND state IN ('in use', 'held')$$) AS t(h bigint);
SELECT assert.equal(n, 20000::bigint, 'and its session''s next call reads every row')
FROM dblink('b', $$SELECT count(*) FROM warren.components('g', NULL::text)$$) AS t(n bigint);
SELECT dblink_disconnect('b');

SELECT assert.equal(count(*), 20000::bigint, 'and the next call reads every row')
FROM warren.components('g', NULL::text);
SELECT assert.equal(count(*), 19999::bigint, 'and every forest edge')
FROM warren.minimum_spanning_forest('g', NULL::text);
SELECT assert.equal(shared_segments(:v1), :one, 'the version is shared again where it can be');
