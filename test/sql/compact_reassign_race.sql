-- DESIGN.md §5.2: compact moves each vector under the creator it has when
-- compact locks it. Here reassign_owned gives reader r_old's vector to
-- r_new and has not committed when compact lists the version's vectors;
-- compact waits for it, then moves the vector under r_new, and leaves
-- nothing on the source version (PR #76 review: it was skipped, and
-- retention then kept the source version for it).
-- fixture: random --nodes 50 --edges 100 --seed 1
CREATE EXTENSION dblink;
\ir include/blocked_by_me.sql
CREATE ROLE cr_owner;
CREATE ROLE r_old;
CREATE ROLE r_new;
GRANT SELECT ON nodes, edges TO cr_owner;
SET ROLE cr_owner;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
RESET ROLE;
SELECT warren.refresh('g') AS source \gset
SELECT warren.grant('g', 'r_old');
SELECT warren.grant('g', 'r_new');
SET ROLE r_old;
SELECT warren.vector_const('g', 'x', 1) IS NOT NULL AS written;
RESET ROLE;

SELECT format('dbname=%s user=%s host=%s port=%s', current_database(), session_user,
              split_part(current_setting('unix_socket_directories'), ',', 1),
              current_setting('port')) AS conninfo \gset
SELECT dblink_connect('compactor', :'conninfo');
SELECT pid AS compactor FROM dblink('compactor', 'SELECT pg_backend_pid()') AS t(pid int) \gset

BEGIN;
SELECT warren.reassign_owned('r_old', 'r_new');
SELECT dblink_send_query('compactor', $$SELECT warren.compact('g')$$);
SELECT assert_blocked_by_me(:compactor, 'compact');
COMMIT;
SELECT version AS compacted FROM dblink_get_result('compactor') AS t(version bigint) \gset
SELECT count(*) FROM dblink_get_result('compactor') AS t(x text);  -- the empty result ending the query
SELECT dblink_disconnect('compactor');

SELECT assert.equal(
    (SELECT string_agg(format('%s:%s:%s', name, version_id = :compacted, created_by), ',')
     FROM warren.vectors),
    'x:t:r_new', 'the vector is moved, under the role it was reassigned to');
SELECT assert.equal((SELECT count(*) FROM warren.vectors WHERE version_id = :source), 0::bigint,
                    'nothing is left on the source version');
SELECT assert.equal(warren.vector_reduce('g', 'x', 'sum'), 50::float8, 'the vector reads as written');
