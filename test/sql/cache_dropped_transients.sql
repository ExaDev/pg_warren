-- DESIGN.md §5.5: transient vectors of a version whose drop committed are
-- freed before a transient write is refused, though no cache load has
-- pruned them; a drop that rolls back frees nothing, even if a prune ran
-- inside it.
CREATE TABLE a_nodes AS SELECT generate_series(1, 2000)::bigint AS id;
CREATE TABLE b_nodes AS SELECT generate_series(1, 5)::bigint AS id;
SELECT warren.create_graph('a', 'SELECT id FROM a_nodes', 'SELECT id, id % 2000 + 1 FROM a_nodes');
SELECT warren.create_graph('b', 'SELECT id FROM b_nodes', 'SELECT id, id % 5 + 1 FROM b_nodes');
SELECT warren.refresh('a');
SELECT warren.refresh('b');
SELECT warren.prewarm('b');
-- Five vectors of 16,000 bytes: more than the 64kB set below.
SELECT warren.vector_const('a', n, 1, persist := false) FROM unnest(ARRAY['v', 'w', 'x', 'y', 'z']) n;

BEGIN;
SELECT warren.drop_graph('a');
SELECT warren.evict('b');
ROLLBACK;
SELECT assert.equal((SELECT sum(value) FROM warren.vector_values('a', 'v', NULL::bigint)),
                    2000::float8, 'a drop rolled back frees no transient vectors');

SELECT warren.drop_graph('a');
SET warren.max_cache_memory = '64kB';
-- b's version is a cache hit, so no load has pruned a's vectors yet.
SELECT assert.equal(warren.vector_const('b', 'q', 1, persist := false), NULL::bigint,
                    'a committed drop frees its transient vectors for others');
