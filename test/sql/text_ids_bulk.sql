-- One call's text ids past 65,536 lookups (DESIGN.md §5.7): the call then
-- looks them up through a hash index charged to warren.max_compute_memory,
-- or, when that has no room for the index, by the binary search it used
-- before; either way it answers as it did below the threshold. 50,000
-- nodes: the index takes 8 bytes a node (400 kB), a vector from a query 9.

CREATE TABLE iris AS
SELECT n, 'https://example.org/node/' || n AS id FROM generate_series(1, 50000) n;
SELECT warren.create_graph('bulk', 'SELECT id FROM iris',
                           $q$SELECT a.id, b.id FROM iris a JOIN iris b ON b.n = a.n % 50000 + 1$q$);
SELECT warren.refresh('bulk');

-- 70,000 lookups: every node once, and 20,000 ids of no node.
CREATE VIEW bulk_rows AS
SELECT id, n::float8 AS value FROM iris
UNION ALL SELECT 'https://example.org/none/' || n, 1 FROM generate_series(1, 20000) n;

SELECT warren.vector_from_query('bulk', 'indexed', 'SELECT id, value FROM bulk_rows');
SELECT assert.equal(warren.vector_reduce('bulk', 'indexed', 'sum'), 50000::float8 * 50001 / 2,
                    'past the threshold, through the index');

SET warren.max_compute_memory = '600kB';
SELECT warren.vector_from_query('bulk', 'searched', 'SELECT id, value FROM bulk_rows');
SELECT assert.equal(warren.vector_reduce('bulk', 'searched', 'sum'), 50000::float8 * 50001 / 2,
                    'without room for the index, by the binary search');
RESET warren.max_compute_memory;

-- similarity's pairs: 20,000 of a node and its successor, then 25,000
-- of unknown ids, which are not kept, so that the call passes 65,536
-- lookups with every pair it keeps already held (512 kB, once grown).
CREATE VIEW bulk_pairs AS
SELECT a.id AS u, b.id AS v FROM iris a JOIN iris b ON b.n = a.n + 1 WHERE a.n <= 20000
UNION ALL SELECT 'https://example.org/none/' || n, 'https://example.org/none/' || n
FROM generate_series(1, 25000) n;
SELECT assert.equal((SELECT count(*) FROM warren.similarity('bulk', 'SELECT u, v FROM bulk_pairs', NULL::text)),
                    20000::bigint, 'similarity past the threshold scores every pair of nodes');
SET warren.max_compute_memory = '800kB';
SELECT assert.equal((SELECT count(*) FROM warren.similarity('bulk', 'SELECT u, v FROM bulk_pairs', NULL::text)),
                    20000::bigint, 'and without room for the index');
RESET warren.max_compute_memory;
