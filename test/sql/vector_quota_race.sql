-- DESIGN.md §5.7, §14 item 47: a quota on readers' vectors under
-- concurrency. One role's counted writes to one graph take a lock on
-- (graph, role), so a second session of the role waits for the first and
-- then counts what it committed: refused if both do not fit, admitted if
-- they do. And usage is read under the latest committed state, so a
-- REPEATABLE READ writer whose snapshot predates another session's write
-- still counts it. A writer under no quota takes the lock too, and so does
-- reassign_owned for the role it moves vectors to (PR #59 review).
-- fixture: random --nodes 50 --edges 100 --seed 1
CREATE EXTENSION dblink;
\ir include/blocked_by_me.sql
CREATE ROLE q_owner;
CREATE ROLE q_reader;
CREATE ROLE q_late;
GRANT SELECT ON nodes, edges TO q_owner;
SET ROLE q_owner;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
RESET ROLE;
SELECT warren.refresh('g');
SELECT warren.grant('g', 'q_reader');
SELECT warren.grant('g', 'q_late');
-- An f64 vector's bytes: room for one and a half of them.
SELECT (warren.info('g') #>> '{versions,0,id_space}')::bigint * 8 AS f64 \gset
SELECT warren.set_vector_quota('g', :f64 * 3 / 2);

SELECT format('dbname=%s user=%s host=%s port=%s', current_database(), session_user,
              split_part(current_setting('unix_socket_directories'), ',', 1),
              current_setting('port')) AS conninfo \gset
SELECT dblink_connect('b', :'conninfo');
SELECT dblink_exec('b', 'SET ROLE q_reader');
SELECT pid AS b_pid FROM dblink('b', 'SELECT pg_backend_pid()') AS t(pid int) \gset

-- 1. The second writer waits on the first's lock, then is refused.
BEGIN;
SET ROLE q_reader;
SELECT warren.vector_const('g', 'a', 1) IS NOT NULL AS written;
RESET ROLE;
SELECT dblink_send_query('b', $$SELECT warren.vector_const('g', 'b', 1)$$);
SELECT assert_blocked_by_me(:b_pid, 'the second writer of q_reader');
SELECT assert.that(EXISTS (SELECT FROM pg_locks WHERE pid = :b_pid AND NOT granted
                           AND locktype = 'advisory' AND objsubid = 22353),
                   'it waits on the (graph, role) lock');
COMMIT;
SELECT count(*) FROM dblink_get_result('b', false) AS t(x bigint);
SELECT dblink_error_message('b') AS b_error \gset
SELECT count(*) FROM dblink_get_result('b', false) AS t(x bigint);  -- the end of its results
SELECT assert.that(:'b_error' LIKE '%vector "b" (% bytes) would take the persisted vectors of role "q_reader" on graph "g" to % bytes, past its quota of % bytes%',
                   'the second writer is refused: ' || :'b_error');
SELECT assert.equal((SELECT string_agg(name, ',' ORDER BY name) FROM warren.vectors), 'a',
                    'only the first writer''s vector is stored');

-- 2. With room for both, the second writer waits, then is admitted.
SELECT warren.set_vector_quota('g', :f64 * 3);
BEGIN;
SET ROLE q_reader;
SELECT warren.vector_const('g', 'b', 1) IS NOT NULL AS written;
RESET ROLE;
SELECT dblink_send_query('b', $$SELECT warren.vector_const('g', 'c', 1)$$);
SELECT assert_blocked_by_me(:b_pid, 'the second writer of q_reader');
COMMIT;
SELECT count(*) FROM dblink_get_result('b', false) AS t(x bigint);
SELECT count(*) FROM dblink_get_result('b', false) AS t(x bigint);  -- the end of its results
SELECT assert.equal((SELECT string_agg(name, ',' ORDER BY name) FROM warren.vectors), 'a,b,c',
                    'both writers'' vectors are stored');
SELECT assert.equal((SELECT bytes FROM warren.vector_usage('g') WHERE role = 'q_reader'::regrole),
                    :f64 * 3, 'q_reader is at its quota');

-- 3. A REPEATABLE READ writer counts a write committed after its snapshot.
SELECT warren.set_vector_quota('g', :f64 * 3 / 2, 'q_late');
SELECT dblink_exec('b', 'RESET ROLE');
SELECT dblink_exec('b', 'SET ROLE q_late');
SELECT dblink_exec('b', 'BEGIN ISOLATION LEVEL REPEATABLE READ');
SELECT * FROM dblink('b', $$SELECT count(*) FROM warren.vector_usage('g')$$) AS t(n bigint);
SET ROLE q_late;
SELECT warren.vector_const('g', 'e', 1) IS NOT NULL AS written;
RESET ROLE;
SELECT assert.equal(
    (SELECT n FROM dblink('b', $$SELECT vectors FROM warren.vector_usage('g')$$) AS t(n bigint)),
    0::bigint, 'the snapshot of b predates q_late''s vector e');
SELECT dblink_send_query('b', $$SELECT warren.vector_const('g', 'f', 1)$$);
SELECT count(*) FROM dblink_get_result('b', false) AS t(x bigint);
SELECT dblink_error_message('b') AS b_error \gset
SELECT count(*) FROM dblink_get_result('b', false) AS t(x bigint);  -- the end of its results
SELECT assert.that(:'b_error' LIKE '%vector "f" (% bytes) would take the persisted vectors of role "q_late" on graph "g" to % bytes, past its quota of % bytes%',
                   'the REPEATABLE READ writer is refused: ' || :'b_error');
SELECT dblink_exec('b', 'ROLLBACK');
SELECT assert.equal((SELECT string_agg(name, ',' ORDER BY name) FROM warren.vectors
                     WHERE created_by = 'q_late'::regrole), 'e', 'only e is q_late''s');

-- 4. A writer under no quota still takes the lock: a quota committed while
-- it is uncommitted holds the next writer to what both write. (The quota
-- is written directly: set_vector_quota would wait for the first writer,
-- whose vector's foreign key holds the graph's row.)
SELECT warren.set_vector_quota('g', NULL);
SELECT dblink_exec('b', 'RESET ROLE');
SELECT dblink_exec('b', 'SET ROLE q_reader');
BEGIN;
SET ROLE q_reader;
SELECT warren.vector_const('g', 'h', 1) IS NOT NULL AS written;
RESET ROLE;
SELECT dblink_connect('c', :'conninfo');
SELECT dblink_exec('c', format(
    'INSERT INTO warren.vector_quotas SELECT graph_id, %L, %s FROM warren.graphs WHERE name = %L',
    'q_reader', :f64 * 4, 'g'));
SELECT dblink_send_query('b', $$SELECT warren.vector_const('g', 'i', 1)$$);
SELECT assert_blocked_by_me(:b_pid, 'a writer of q_reader behind one under no quota');
COMMIT;
SELECT count(*) FROM dblink_get_result('b', false) AS t(x bigint);
SELECT dblink_error_message('b') AS b_error \gset
SELECT count(*) FROM dblink_get_result('b', false) AS t(x bigint);  -- the end of its results
SELECT assert.that(:'b_error' LIKE '%vector "i" (% bytes) would take the persisted vectors of role "q_reader" on graph "g" to % bytes, past its quota of % bytes%',
                   'the writer counts the uncommitted one and the new quota: ' || :'b_error');
SELECT assert.equal((SELECT bytes FROM warren.vector_usage('g') WHERE role = 'q_reader'::regrole),
                    :f64 * 4, 'q_reader is at its new quota');

-- 5. reassign_owned waits on the lock of the role it moves vectors to, then
-- counts that role's committed writes with the move.
CREATE ROLE q_mallory;
CREATE ROLE q_group;
GRANT q_group TO q_mallory;
SELECT warren.grant('g', 'q_mallory');
SELECT warren.grant('g', 'q_group');
SELECT warren.set_vector_quota('g', :f64 * 2, 'q_group');
SET ROLE q_mallory;
SELECT warren.vector_const('g', 'm', 1) IS NOT NULL AS written;
RESET ROLE;
SELECT dblink_exec('b', 'RESET ROLE');
SELECT dblink_exec('b', 'SET ROLE q_mallory');
BEGIN;
SET ROLE q_group;
SELECT warren.vector_const('g', 'g1', 1) IS NOT NULL AS written;
SELECT warren.vector_const('g', 'g2', 1) IS NOT NULL AS written;
RESET ROLE;
SELECT dblink_send_query('b', $$SELECT warren.reassign_owned('q_mallory', 'q_group')$$);
SELECT assert_blocked_by_me(:b_pid, 'reassign_owned to q_group');
COMMIT;
SELECT count(*) FROM dblink_get_result('b', false) AS t(x bigint);
SELECT dblink_error_message('b') AS b_error \gset
SELECT count(*) FROM dblink_get_result('b', false) AS t(x bigint);  -- the end of its results
SELECT assert.that(:'b_error' LIKE '%reassigning the persisted vectors of role "q_mallory" on graph "g" (% bytes) would take those of role "q_group" to % bytes, past its quota%',
                   'the move is refused: ' || :'b_error');
SELECT assert.equal((SELECT created_by::text FROM warren.vectors WHERE name = 'm'), 'q_mallory',
                    'm stays q_mallory''s');
SELECT dblink_disconnect('c');
SELECT dblink_disconnect('b');
