-- Graph g for oracle_paths*.sql: the fixture's edges in relation a with
-- weights 1–4, again in relation b where (src + dst) % 5 = 0 (weight 2: a
-- pair in both weighs its lighter relation), self-loops on ids divisible
-- by 13, and node 7 deleted after the first build.
CREATE TABLE g_nodes AS SELECT id FROM nodes;
CREATE TABLE g_edges AS
SELECT src, dst, 'a'::text AS rel, (1 + (src * 3 + dst) % 4)::float8 AS w FROM edges
UNION ALL SELECT src, dst, 'b', 2 FROM edges WHERE (src + dst) % 5 = 0
UNION ALL SELECT id, id, 'a', 1 FROM nodes WHERE id % 13 = 0;
SELECT warren.create_graph('g', 'SELECT id FROM g_nodes', 'SELECT src, dst, rel, w FROM g_edges',
                           weighted := true, has_relations := true);
SELECT warren.refresh('g');
DELETE FROM g_nodes WHERE id = 7;
SELECT warren.refresh('g');
SELECT assert.equal((SELECT id_space - node_count FROM warren.versions v JOIN warren.graphs g USING (graph_id)
                     WHERE g.name = 'g' AND v.version_id = g.current_version), 1::bigint, 'node 7 is a tombstone');
