-- DESIGN.md §5.1: the current version cannot be deleted, also when another
-- session promotes it after the deleting transaction took its snapshot.
-- warren.versions has no foreign key from graphs.current_version (a dump
-- could not restore it), so the BEFORE DELETE trigger must lock the graph's
-- row, as a key's check would: under REPEATABLE READ the delete then fails
-- with serialization_failure, under READ COMMITTED with 23503. The
-- promotions are direct updates of graphs.current_version, which leave the
-- versions rows untouched (promote also updates the version's row, which
-- would make the delete conflict on its own).
CREATE EXTENSION dblink;
CREATE TABLE nodes AS SELECT generate_series(1, 4)::bigint AS id;
CREATE TABLE edges AS SELECT id AS src, id % 4 + 1 AS dst FROM nodes;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('g') AS promoted \gset
SELECT warren.refresh('g', promote := false) AS staged \gset

SELECT format('dbname=%s user=%s host=%s port=%s', current_database(), session_user,
              split_part(current_setting('unix_socket_directories'), ',', 1),
              current_setting('port')) AS conninfo \gset
SELECT dblink_connect('a', :'conninfo');

-- REPEATABLE READ: a's snapshot predates the promotion of the staged version.
SELECT dblink_exec('a', 'BEGIN ISOLATION LEVEL REPEATABLE READ');
SELECT * FROM dblink('a', 'SELECT current_version FROM warren.graphs') AS t(v bigint);
UPDATE warren.graphs SET current_version = :staged WHERE name = 'g';
SELECT dblink_exec('a', format('DELETE FROM warren.versions WHERE version_id = %s', :staged), false)
    AS deleted \gset
SELECT dblink_error_message('a') AS a_error \gset
SELECT dblink_exec('a', 'ROLLBACK');
SELECT assert.equal(:'deleted', 'ERROR', 'a repeatable-read delete of the new current version fails');
SELECT assert.that(:'a_error' LIKE '%could not serialize access%',
                   'and must be retried: ' || :'a_error');

-- READ COMMITTED: the delete's trigger reads the graph after the promotion.
SELECT dblink_exec('a', 'BEGIN');
SELECT * FROM dblink('a', 'SELECT current_version FROM warren.graphs') AS t(v bigint);
UPDATE warren.graphs SET current_version = :promoted WHERE name = 'g';
SELECT dblink_exec('a', format('DELETE FROM warren.versions WHERE version_id = %s', :promoted), false)
    AS deleted \gset
SELECT dblink_error_message('a') AS a_error \gset
SELECT dblink_exec('a', 'ROLLBACK');
SELECT assert.equal(:'deleted', 'ERROR', 'a read-committed delete of the new current version fails');
SELECT assert.that(:'a_error' LIKE '%is the current version of its graph%',
                   'with the trigger''s error: ' || :'a_error');
SELECT dblink_disconnect('a');

SELECT assert.equal((SELECT current_version FROM warren.graphs WHERE name = 'g'), :promoted::bigint,
                    'the current version is the last one promoted');
SELECT assert.equal((SELECT count(*) FROM warren.versions), 2::bigint, 'no version was deleted');
SELECT assert.equal((SELECT count(*) FROM warren.bfs('g', ARRAY[1::bigint], 4)), 4::bigint,
                    'the graph still answers');
