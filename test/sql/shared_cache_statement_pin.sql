-- DESIGN.md §5.5, §5.6: the statement pin over shared versions. A version
-- a top-level statement reads stays in the shared cache until the
-- statement ends, normally or with an error, or its transaction does (a
-- COMMIT inside a CALL): no backend evicts it meanwhile, not even its
-- graph's owner through warren.evict, which here (in session x) probes
-- whether this session pins it. A library run reads its version from the
-- shared cache once. test/sql/cache_statement_pin.sql tests the pin over
-- the private cache. Where the shared cache is off or unavailable, nothing
-- is shared and x evicts nothing.
\ir include/shared_cache.sql
CREATE TABLE a_nodes AS SELECT generate_series(1, 2000)::bigint AS id;
SELECT warren.create_graph('a', 'SELECT id FROM a_nodes', 'SELECT id, id % 2000 + 1 FROM a_nodes');
SELECT warren.refresh('a') AS v \gset
SELECT CASE WHEN :'shared' THEN 1 ELSE 0 END::bigint AS one \gset
SELECT dblink_connect('x', :'conninfo');
-- The rows of warren.segments this transaction has read (an index scan
-- counts its heap fetches on the index), and those of one load.
CREATE FUNCTION segment_reads() RETURNS bigint LANGUAGE sql AS $$
    SELECT pg_stat_get_xact_tuples_returned('warren.segments'::regclass)
           + pg_stat_get_xact_tuples_fetched('warren.segments'::regclass)
           + sum(pg_stat_get_xact_tuples_fetched(indexrelid))::bigint
    FROM pg_index WHERE indrelid = 'warren.segments'::regclass
$$;
BEGIN;
SELECT segment_reads() AS before \gset
SELECT warren.prewarm('a');
SELECT segment_reads() - :before AS one_load \gset
COMMIT;
-- What x's warren.evict('a') evicts: the shared version, unless pinned.
CREATE FUNCTION evicted_by_x() RETURNS bigint LANGUAGE sql AS $$
    SELECT evicted FROM dblink('x', 'SELECT warren.evict(''a'')') AS t(evicted bigint)
$$;

-- Within a statement, x cannot evict what it read; after it, x can.
DO $$ BEGIN
    PERFORM warren.prewarm('a');
    PERFORM assert.equal(evicted_by_x(), 0::bigint, 'a statement pins what it read');
END $$;
SELECT assert.equal(evicted_by_x(), :one, 'the pin ends with the statement');

-- Inside a transaction block too: the pin is the statement's, not the
-- transaction's. (The read revives the version, which this session still
-- maps.)
BEGIN;
SELECT warren.prewarm('a');
SELECT assert.equal(evicted_by_x(), :one, 'the pin ends with the statement, not the transaction');
COMMIT;

-- A statement that fails releases its pin as well.
\set ON_ERROR_STOP off
DO $$ BEGIN
    PERFORM warren.prewarm('a');
    RAISE EXCEPTION 'the statement fails';
END $$;
\set ON_ERROR_STOP on
SELECT assert.equal(evicted_by_x(), :one, 'an error releases the pin');

-- A COMMIT inside a CALL ends the pin too: the CALL is one statement, but
-- no pin spans its transactions.
CREATE PROCEDURE read_then_commit(one bigint) LANGUAGE plpgsql AS $$ BEGIN
    PERFORM warren.prewarm('a');
    PERFORM assert.equal(evicted_by_x(), 0::bigint, 'pinned within the transaction');
    COMMIT;
    PERFORM assert.equal(evicted_by_x(), one, 'a COMMIT in a CALL ends the pin');
END $$;
CALL read_then_commit(:one);

-- A library run (several warren.* calls per iteration) reads the shared
-- version without loading it, and loads a private one once. This
-- session's own copy (where nothing is shared) or mapping is freed first.
SET warren.max_cache_memory = '200kB';
SELECT warren.prewarm('a');
BEGIN;
SELECT own_evictions('a');
SELECT segment_reads() AS before \gset
SELECT warren.pagerank('a', 'pr');
SELECT assert.equal(segment_reads() - :before, (1 - :one) * :one_load,
                    'pagerank loaded the version once, or not at all where it is shared');
COMMIT;

SELECT dblink_disconnect('x');
