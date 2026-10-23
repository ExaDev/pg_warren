-- DESIGN.md §5.7: reassign_owned, drop_owned and drop_orphans, queued in
-- session b behind this session's uncommitted set_owner, refresh or grant
-- of a graph, act on what that transaction committed: a graph moved away
-- is untouched, a graph moved to the new owner keeps it out of its
-- readers, and a graph adopted, or a reader granted, by a role created in
-- that transaction is not swept as an orphan. A drop_vector queued behind
-- a reassign_owned of the vector's creator is refused, not applied to the
-- new creator's vector.
CREATE EXTENSION dblink;
\ir include/blocked_by_me.sql
CREATE ROLE rr_old;
CREATE ROLE rr_new;
CREATE ROLE rr_other;
CREATE TABLE nodes AS SELECT generate_series(1, 4)::bigint AS id;
CREATE TABLE edges AS SELECT id AS src, id % 4 + 1 AS dst FROM nodes;
GRANT SELECT ON nodes, edges TO rr_old, rr_new, rr_other;
SET ROLE rr_old;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('g');
RESET ROLE;
SET ROLE rr_other;
SELECT warren.create_graph('v', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('v');
SELECT warren.grant('v', 'rr_old');
RESET ROLE;

SELECT format('dbname=%s user=%s host=%s port=%s', current_database(), session_user,
              split_part(current_setting('unix_socket_directories'), ',', 1),
              current_setting('port')) AS conninfo \gset
SELECT dblink_connect('b', :'conninfo');
SELECT pid AS b_pid FROM dblink('b', 'SELECT pg_backend_pid()') AS t(pid int) \gset

-- reassign_owned behind a transfer of g away from rr_old moves nothing.
SELECT $$SELECT warren.set_owner('g', 'rr_other')$$ AS first,
       $$SELECT warren.reassign_owned('rr_old', 'rr_new')$$ AS call \gset
\ir include/dropped_roles_race.sql
SELECT assert.equal(:'b_result', '0', 'reassign_owned moved nothing: ' || :'b_error');
SELECT assert.equal((SELECT owner::text FROM warren.graphs WHERE name = 'g'), 'rr_other',
                    'g stays with the role it was moved to');
SELECT warren.set_owner('g', 'rr_old');

-- reassign_owned behind a grant of g to rr_new keeps rr_new out of the
-- readers of the graph it now owns.
SELECT $$SELECT warren.grant('g', 'rr_new')$$ AS first,
       $$SELECT warren.reassign_owned('rr_old', 'rr_new')$$ AS call \gset
\ir include/dropped_roles_race.sql
SELECT assert.equal(:'b_result', '1', 'reassign_owned moved g: ' || :'b_error');
SELECT assert.equal((SELECT owner::text || ':' || readers::text FROM warren.graphs WHERE name = 'g'),
                    'rr_new:{}', 'the new owner is not a reader');
SELECT warren.set_owner('g', 'rr_old');

-- reassign_owned behind a refresh of g waits, then moves it.
SELECT $$SELECT warren.refresh('g')$$ AS first,
       $$SELECT warren.reassign_owned('rr_old', 'rr_new')$$ AS call \gset
\ir include/dropped_roles_race.sql
SELECT assert.equal(:'b_result', '1', 'reassign_owned moved g after the refresh: ' || :'b_error');
SELECT warren.set_owner('g', 'rr_old');

-- drop_owned behind a transfer of g away from rr_old drops nothing.
SELECT $$SELECT warren.set_owner('g', 'rr_other')$$ AS first,
       $$SELECT warren.drop_owned('rr_old')$$ AS call \gset
\ir include/dropped_roles_race.sql
SELECT assert.equal(:'b_result', '0', 'drop_owned dropped nothing: ' || :'b_error');
SELECT assert.equal((SELECT owner::text FROM warren.graphs WHERE name = 'g'), 'rr_other',
                    'g is kept by the role it was moved to');
SELECT warren.set_owner('g', 'rr_old');

-- A drop_vector by rr_old, queued behind a reassign_owned of rr_old's
-- vectors, would delete rr_new's vector: it is refused (40001). (The
-- drop_owned above removed rr_old from v's readers.)
SELECT warren.grant('v', 'rr_old');
SET ROLE rr_old;
SELECT warren.vector_const('v', 'mine', 1);
RESET ROLE;
SELECT dblink_exec('b', 'SET ROLE rr_old');
SELECT $$SELECT warren.reassign_owned('rr_old', 'rr_new')$$ AS first,
       $$SELECT warren.drop_vector('v', 'mine')$$ AS call \gset
\ir include/dropped_roles_race.sql
SELECT assert.that(:'b_error' LIKE '%written by a concurrent transaction%',
                   'the queued drop_vector is refused: ' || :'b_error');
SELECT assert.equal((SELECT created_by::text FROM warren.vectors WHERE name = 'mine'), 'rr_new',
                    'rr_new keeps the vector');
SELECT dblink_exec('b', 'RESET ROLE');
SELECT warren.set_owner('g', 'rr_old');

-- drop_owned behind a refresh of g waits, then drops it.
SELECT $$SELECT warren.refresh('g')$$ AS first, $$SELECT warren.drop_owned('rr_old')$$ AS call \gset
\ir include/dropped_roles_race.sql
SELECT assert.equal(:'b_result', '1', 'drop_owned dropped g after the refresh: ' || :'b_error');
SELECT warren.grant('v', 'rr_old');

-- drop_orphans behind a superuser adopting an orphan for a role created in
-- the same transaction keeps the graph.
CREATE ROLE rr_gone;
GRANT SELECT ON nodes, edges TO rr_gone;
SET ROLE rr_gone;
SELECT warren.create_graph('o', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
RESET ROLE;
SELECT warren.grant('v', 'rr_gone');
REVOKE ALL ON nodes, edges FROM rr_gone;
DROP ROLE rr_gone;
SELECT $$CREATE ROLE rr_heir; GRANT SELECT ON nodes, edges TO rr_heir;
         SELECT warren.set_owner('o', 'rr_heir')$$ AS first,
       $$SELECT warren.drop_orphans()$$ AS call \gset
\ir include/dropped_roles_race.sql
SELECT assert.equal(:'b_result', '0', 'drop_orphans dropped no graph: ' || :'b_error');
SELECT assert.equal((SELECT owner::text FROM warren.graphs WHERE name = 'o'), 'rr_heir',
                    'the adopted graph is kept');
SELECT assert.equal((SELECT readers::text FROM warren.graphs WHERE name = 'v'), '{rr_old}',
                    'drop_orphans pruned the dropped reader');

-- drop_orphans behind a grant to a role created in the same transaction
-- keeps that reader.
CREATE ROLE rr_gone2;
SELECT warren.grant('v', 'rr_gone2');
DROP ROLE rr_gone2;
SELECT $$CREATE ROLE rr_late; SELECT warren.grant('v', 'rr_late')$$ AS first,
       $$SELECT warren.drop_orphans()$$ AS call \gset
\ir include/dropped_roles_race.sql
SELECT assert.equal(:'b_error', 'OK', 'drop_orphans ran');
SELECT assert.equal((SELECT readers::text FROM warren.graphs WHERE name = 'v'), '{rr_old,rr_late}',
                    'the new reader is kept');

SELECT dblink_disconnect('b');
SELECT warren.drop_graph(name) FROM warren.graphs;
REVOKE ALL ON nodes, edges FROM rr_old, rr_new, rr_other, rr_heir;
DROP ROLE rr_old, rr_new, rr_other, rr_heir, rr_late;
