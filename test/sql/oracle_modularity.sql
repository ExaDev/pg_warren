-- warren.modularity against NetworkX's community.modularity (DESIGN.md
-- §6.7, §12) on an undirected weighted random graph with self-loops,
-- partitioned by a hash of the id. pg_warren counts a self-loop once in
-- its node's degree (as contract's PᵀAP does) and NetworkX twice; the
-- oracle halves each self-loop's weight, which makes NetworkX's count
-- pg_warren's, so the two agree exactly.
-- fixture: random --nodes 200 --edges 800 --seed 11
-- oracle: modularity --labels-sql 'SELECT id, id % 7 FROM nodes' --edges-sql 'SELECT src, dst, w FROM (SELECT src, dst, (1 + (src + 2 * dst) % 5)::float8 AS w FROM edges UNION ALL SELECT id, id, 2.5 FROM nodes WHERE id % 9 = 0) e'
CREATE TABLE g_edges AS
SELECT src, dst, (1 + (src + 2 * dst) % 5)::float8 AS w FROM edges
UNION ALL SELECT id, id, 2.5 FROM nodes WHERE id % 9 = 0;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst, w FROM g_edges',
                           directed := false, weighted := true);
SELECT warren.refresh('g');
SELECT warren.vector_from_query('g', 'label', 'SELECT id, id % 7 FROM nodes');
SELECT assert.that(abs(warren.modularity('g', 'label') - (SELECT modularity FROM oracle_modularity)) < 1e-12,
                   format('modularity %s against NetworkX with halved self-loops %s',
                          warren.modularity('g', 'label'), (SELECT modularity FROM oracle_modularity)));
