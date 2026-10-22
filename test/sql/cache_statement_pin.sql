-- DESIGN.md §5.5: a version a top-level statement reads stays cached until
-- the statement ends, past warren.max_cache_memory if need be; when the
-- statement ends, normally or with an error, the cap holds again. Two
-- rings of 2,000 nodes, about 94 kB loaded each: one fits a 150kB cap,
-- both do not. Where versions are shared (test/run.py --shared-cache), the
-- shared cache holds them and pins them per statement instead (its
-- #[pg_test]s and test/sql/shared_cache_*), so this test has nothing to see.
SELECT coalesce(current_setting('warren.shared_cache_memory', true), '0') <> '0'
       AND (current_setting('server_version_num')::int >= 170000
            OR current_setting('shared_preload_libraries') ~ 'pg_warren') AS shared \gset
\if :shared
\quit
\endif
CREATE TABLE a_nodes AS SELECT generate_series(1, 2000)::bigint AS id;
SELECT warren.create_graph('a', 'SELECT id FROM a_nodes', 'SELECT id, id % 2000 + 1 FROM a_nodes');
SELECT warren.create_graph('b', 'SELECT id FROM a_nodes', 'SELECT id, id % 2000 + 1 FROM a_nodes');
SELECT warren.refresh('a');
SELECT warren.refresh('b');
SET warren.max_cache_memory = '150kB';

-- Within a statement, loading b evicts nothing the statement has read.
DO $$ BEGIN
    PERFORM warren.prewarm('a');
    PERFORM warren.prewarm('b');
    PERFORM assert.equal(warren.evict('a'), 1::bigint, 'a statement keeps the versions it read');
END $$;
SELECT assert.equal(warren.evict('b'), 1::bigint, 'b fits the cap alone');

-- When the statement ends, the cap holds: a, the least recently used, goes.
DO $$ BEGIN PERFORM warren.prewarm('a'); PERFORM warren.prewarm('b'); END $$;
SELECT assert.equal(warren.evict('a'), 0::bigint, 'the cap holds between statements');
SELECT assert.equal(warren.evict('b'), 1::bigint, 'the most recently used stays');

-- Inside a transaction block too: the pin is the statement's, not the
-- transaction's.
BEGIN;
DO $$ BEGIN PERFORM warren.prewarm('a'); PERFORM warren.prewarm('b'); END $$;
SELECT assert.equal(warren.evict('a'), 0::bigint, 'the pin ends with the statement, not the transaction');
COMMIT;
SELECT warren.evict('b');

-- A statement that fails releases its pin as well.
\set ON_ERROR_STOP off
DO $$ BEGIN
    PERFORM warren.prewarm('a');
    PERFORM warren.prewarm('b');
    RAISE EXCEPTION 'the statement fails';
END $$;
\set ON_ERROR_STOP on
SELECT assert.equal(warren.evict('a'), 0::bigint, 'an error releases the pin');
SELECT assert.equal(warren.evict('b'), 1::bigint, 'and the cap holds after it');

-- A COMMIT inside a CALL ends the pin too: the CALL is one statement, but
-- no pin spans its transactions.
CREATE PROCEDURE a_then_b() LANGUAGE plpgsql AS $$ BEGIN
    PERFORM warren.prewarm('a');
    COMMIT;
    PERFORM warren.prewarm('b');
    PERFORM assert.equal(warren.evict('a'), 0::bigint, 'a COMMIT in a CALL ends the pin');
END $$;
SET warren.max_cache_memory = '100kB';
CALL a_then_b();
SELECT warren.evict('b');

-- One message of several statements reading more graphs than both limits
-- hold together: what no call still uses gives way, and nothing is
-- refused.
SET warren.max_cache_memory = '150kB';
SET warren.max_compute_memory = '64kB';
CREATE TABLE c_nodes AS SELECT generate_series(1, 2000)::bigint AS id;
SELECT warren.create_graph('c', 'SELECT id FROM c_nodes', 'SELECT id, id % 2000 + 1 FROM c_nodes');
SELECT warren.refresh('c');
SELECT warren.prewarm('a') \; SELECT warren.prewarm('b') \; SELECT warren.prewarm('c') \; SELECT warren.prewarm('a');
RESET warren.max_compute_memory;
SELECT warren.evict(g) FROM unnest(ARRAY['a', 'b', 'c']) g;

-- A library run (several warren.* calls per iteration) whose version fits
-- the cap alone, but not beside its transient scratch (pagerank's eleven
-- vectors of 16 kB), loads the version once.
SET warren.max_cache_memory = '200kB';
-- The rows of warren.segments this transaction has read (an index scan
-- counts its heap fetches on the index).
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
SELECT warren.evict('a');
SELECT warren.pagerank('a', 'pr');
SELECT assert.equal(segment_reads() - :before, 2 * :one_load, 'pagerank loaded the version once');
COMMIT;
