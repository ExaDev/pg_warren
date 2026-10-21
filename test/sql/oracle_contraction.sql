-- warren.contract against NetworkX's quotient graph (DESIGN.md §6.6, §12),
-- and project and lift against SQL GROUP BY and joins, on an undirected
-- weighted random graph with self-loops and parallel edges (deduplicated,
-- weights summed), labelled into 23 communities by a hash of the id, with
-- node 7 deleted after the first build (a tombstone: in no community). An
-- undirected edge counts once, inside a community or between two.
-- fixture: random --nodes 300 --edges 1500 --seed 7
-- oracle: quotient --labels-sql 'SELECT id, (id * 7919) % 23 FROM nodes WHERE id <> 7' --nodes-sql 'SELECT id FROM nodes WHERE id <> 7' --edges-sql 'SELECT src, dst, w FROM (SELECT src, dst, (1 + (src * 3 + dst) % 11)::float8 AS w FROM edges UNION ALL SELECT src, dst, 0.5 FROM edges WHERE (src + dst) % 5 = 0 UNION ALL SELECT id, id, 1.5 FROM nodes WHERE id % 17 = 0) e WHERE 7 NOT IN (src, dst)'
CREATE TABLE g_nodes AS SELECT id FROM nodes;
CREATE TABLE g_edges AS
SELECT src, dst, (1 + (src * 3 + dst) % 11)::float8 AS w FROM edges
UNION ALL SELECT src, dst, 0.5 FROM edges WHERE (src + dst) % 5 = 0
UNION ALL SELECT id, id, 1.5 FROM nodes WHERE id % 17 = 0;
SELECT warren.create_graph('g', 'SELECT id FROM g_nodes', 'SELECT src, dst, w FROM g_edges',
                           directed := false, weighted := true);
SELECT warren.refresh('g');
DELETE FROM g_nodes WHERE id = 7;
SELECT warren.refresh('g');
SELECT warren.vector_from_query('g', 'label', 'SELECT id, (id * 7919) % 23 FROM g_nodes');

SELECT assert.that(warren.contract('g', 'label', 'q') IS NOT NULL, 'contract returns the version');
SELECT assert.equal((SELECT count(*) FROM warren.id_map('q', NULL::bigint) WHERE live), 23::bigint,
                    'one node per label');
SET test.quotient = 'q';
\ir include/quotient_edges.sql
SELECT assert.that((SELECT count(*) FROM oracle_quotient WHERE source = target) > 10,
                   'the fixture has edges inside communities');
SELECT assert.equal((SELECT count(*) FROM quotient_edges), (SELECT count(*) FROM oracle_quotient),
                    'the quotient has NetworkX''s edges');
SELECT assert.that((SELECT max(abs(f.weight - o.weight) / o.weight)
                    FROM quotient_edges f JOIN oracle_quotient o USING (source, target)) < 1e-12,
                   'each edge has NetworkX''s weight');
SELECT assert.equal((SELECT count(*) FROM quotient_edges f JOIN oracle_quotient o USING (source, target)),
                    (SELECT count(*) FROM oracle_quotient), 'every edge is compared');

-- Projection against GROUP BY, lifting against a join.
CREATE TABLE label AS SELECT id, (id * 7919) % 23 AS label FROM g_nodes;
SELECT warren.vector_from_query('g', 'x', 'SELECT id, sin(id) FROM g_nodes');
SELECT warren.vector_from_query('g', 'm', 'SELECT id, ARRAY[sin(id), id::float8] FROM g_nodes');
SELECT warren.project('g', 'x', 'q', agg, agg) FROM unnest(ARRAY['sum', 'mean', 'max', 'min', 'count']) agg;
SELECT warren.project('g', 'm', 'q', 'centroid', 'mean');
CREATE TABLE expected AS
SELECT label, sum(sin(id)) AS sum, avg(sin(id)) AS mean, max(sin(id)) AS max, min(sin(id)) AS min,
       count(*) AS count, avg(id::float8) AS mean_id
FROM label GROUP BY label;
SELECT assert.that((SELECT max(greatest(abs(s.value - e.sum), abs(a.value - e.mean), abs(x.value - e.max),
                                        abs(n.value - e.min), abs(c.value - e.count)))
                    FROM expected e
                    JOIN warren.vector_values('q', 'sum', NULL::bigint) s ON s.node_id = e.label
                    JOIN warren.vector_values('q', 'mean', NULL::bigint) a ON a.node_id = e.label
                    JOIN warren.vector_values('q', 'max', NULL::bigint) x ON x.node_id = e.label
                    JOIN warren.vector_values('q', 'min', NULL::bigint) n ON n.node_id = e.label
                    JOIN warren.vector_values('q', 'count', NULL::bigint) c ON c.node_id = e.label) < 1e-12,
                   'project agrees with GROUP BY');
SELECT assert.that((SELECT max(greatest(abs(r.row[1] - e.mean), abs(r.row[2] - e.mean_id)))
                    FROM expected e JOIN warren.matrix_rows('q', 'centroid', NULL::bigint) r
                    ON r.node_id = e.label) < 1e-12, 'the mean of a matrix is each column''s');
SELECT warren.lift('q', 'sum', 'g', 'community_sum');
SELECT assert.that((SELECT max(abs(v.value - e.sum)) FROM warren.vector_values('g', 'community_sum', NULL::bigint) v
                    JOIN label l ON l.id = v.node_id JOIN expected e USING (label)) < 1e-12,
                   'lift agrees with the join');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'community_sum', NULL::bigint)), 299::bigint,
                    'every live node is lifted');
