-- Management functions check ownership again after GraphDefinition::lock
-- (DESIGN.md §5.7, §14 item 43). This session transfers graph g from alice
-- to bob in an open transaction; session b, as alice, calls a management
-- function that waits on the graph's row, and runs on when this session
-- commits. It must then be refused: alice no longer owns g. Each sent query
-- is read to its end (dblink_get_result until the empty result) before the
-- next one.
CREATE EXTENSION dblink;
\ir include/blocked_by_me.sql
CREATE ROLE alice;
CREATE ROLE bob;
CREATE ROLE eve;
CREATE TABLE nodes AS SELECT generate_series(1, 4)::bigint AS id;
CREATE TABLE edges AS SELECT id AS src, id % 4 + 1 AS dst FROM nodes;
GRANT SELECT ON nodes, edges TO alice, bob;
SET ROLE alice;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges', retain_versions := 10);
SELECT warren.refresh('g');
SELECT warren.vector_const('g', 'c', 1);
SELECT warren.contract('g', 'c', 'q');
RESET ROLE;

SELECT format('dbname=%s user=%s host=%s port=%s', current_database(), session_user,
              split_part(current_setting('unix_socket_directories'), ',', 1),
              current_setting('port')) AS conninfo \gset
SELECT dblink_connect('b', :'conninfo');
SELECT pid AS b_pid FROM dblink('b', 'SELECT pg_backend_pid()') AS t(pid int) \gset
SELECT dblink_exec('b', 'SET ROLE alice');

-- Each case: this session gives :graph to bob while b's :call, as alice,
-- waits on its row; b is refused as no longer an owner of it, and the
-- graph is given back to alice.
SELECT 'g' AS graph, $$SELECT warren.refresh('g')$$ AS call, 'refresh' AS action \gset
\ir include/set_owner_race.sql

SELECT 'g' AS graph, $$SELECT warren.compact('g')$$ AS call, 'compact' AS action \gset
\ir include/set_owner_race.sql

SELECT warren.refresh('g', promote := false) AS staged \gset
SELECT 'g' AS graph, format($$SELECT warren.promote('g', %s)$$, :staged) AS call,
       'promote' AS action \gset
\ir include/set_owner_race.sql

SELECT 'g' AS graph, format($$SELECT warren.drop_version('g', %s)$$, :staged) AS call,
       'drop versions of' AS action \gset
\ir include/set_owner_race.sql

SELECT 'q' AS graph, $$SELECT warren.contract('g', 'c', 'q')$$ AS call,
       'contract into' AS action \gset
\ir include/set_owner_race.sql

SELECT assert.equal(
    (SELECT count(*) FROM warren.versions JOIN warren.graphs USING (graph_id) WHERE name = 'q'),
    1::bigint, 'the refused contraction added no version');

-- grant and revoke change only readers, and drop_graph deletes the graph:
-- each would otherwise be applied to bob's graph once the row is free.
SELECT 'g' AS graph, $$SELECT warren.grant('g', 'eve')$$ AS call, 'grant' AS action \gset
\ir include/set_owner_race.sql
SELECT assert.equal((SELECT readers::text FROM warren.graphs WHERE name = 'g'), '{}',
                    'the refused grant added no reader');

SELECT warren.grant('g', 'eve');
SELECT 'g' AS graph, $$SELECT warren.revoke('g', 'eve')$$ AS call, 'revoke' AS action \gset
\ir include/set_owner_race.sql
SELECT assert.equal((SELECT readers::text FROM warren.graphs WHERE name = 'g'), '{eve}',
                    'the refused revoke removed no reader');

SELECT 'g' AS graph, $$SELECT warren.drop_graph('g')$$ AS call, 'drop' AS action \gset
\ir include/set_owner_race.sql
SELECT assert.that(EXISTS (SELECT FROM warren.graphs WHERE name = 'g'),
                   'the refused drop_graph kept the graph');

SELECT dblink_disconnect('b');
