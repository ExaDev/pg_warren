-- The structural kernels against NetworkX (DESIGN.md §6.7, §12), on one
-- directed random graph widened with self-loops, parallel edges in the same
-- relation (deduplicated, weights summed) and in a second relation (a
-- length: the forest takes a pair's lightest relation, a longest path its
-- heaviest, §5.3), and a node deleted after the first build (a
-- tombstone): core_number (undirected), strong_components_vector and
-- minimum_spanning_forest (per component total weight; weights differ by
-- direction) on graph g; topological_order and dag_longest_path on its
-- edges from a smaller id to a larger (a DAG), with negative weights.
-- The oracles read the same rows as the graphs, node 7 left out.
-- fixture: random --nodes 400 --edges 900 --directed --seed 11
-- oracle: core_number --nodes-sql 'SELECT id FROM nodes WHERE id <> 7' --edges-sql 'SELECT src, dst FROM edges WHERE 7 NOT IN (src, dst) UNION ALL SELECT id, id FROM nodes WHERE id % 13 = 0'
-- oracle: strong_components --directed --nodes-sql 'SELECT id FROM nodes WHERE id <> 7' --edges-sql 'SELECT src, dst FROM edges WHERE 7 NOT IN (src, dst) UNION ALL SELECT id, id FROM nodes WHERE id % 13 = 0'
-- oracle: spanning_forest --directed --nodes-sql 'SELECT id FROM nodes WHERE id <> 7' --edges-sql 'SELECT src, dst, min(w) FROM (SELECT src, dst, rel, sum(w) AS w FROM (SELECT src, dst, 0 AS rel, (1 + (src * 3 + dst) % 11)::float8 AS w FROM edges UNION ALL SELECT src, dst, 1, (1 + src % 4)::float8 FROM edges WHERE (src + dst) % 5 = 0 UNION ALL SELECT src, dst, 0, 2::float8 FROM edges WHERE (src + dst) % 7 = 0 UNION ALL SELECT id, id, 0, 1::float8 FROM nodes WHERE id % 13 = 0) e WHERE 7 NOT IN (src, dst) GROUP BY 1, 2, 3) r GROUP BY 1, 2'
-- oracle: topological_order --directed --nodes-sql 'SELECT id FROM nodes WHERE id <> 7' --edges-sql 'SELECT src, dst FROM edges WHERE src < dst AND 7 NOT IN (src, dst)'
-- oracle: dag_longest_path --directed --nodes-sql 'SELECT id FROM nodes WHERE id <> 7' --edges-sql 'SELECT src, dst, max(w) FROM (SELECT src, dst, rel, sum(w) AS w FROM (SELECT src, dst, 0 AS rel, ((src * dst) % 7 - 2)::float8 AS w FROM edges WHERE src < dst UNION ALL SELECT src, dst, 1, 1::float8 FROM edges WHERE src < dst AND (src + dst) % 5 = 0) e WHERE 7 NOT IN (src, dst) GROUP BY 1, 2, 3) r GROUP BY 1, 2'
CREATE TABLE g_nodes AS SELECT id FROM nodes;
CREATE TABLE g_edges AS
SELECT src, dst, 'a'::text AS rel, (1 + (src * 3 + dst) % 11)::float8 AS w FROM edges
UNION ALL SELECT src, dst, 'b', (1 + src % 4)::float8 FROM edges WHERE (src + dst) % 5 = 0
UNION ALL SELECT src, dst, 'a', 2 FROM edges WHERE (src + dst) % 7 = 0
UNION ALL SELECT id, id, 'a', 1 FROM nodes WHERE id % 13 = 0;
CREATE TABLE dag_edges AS
SELECT src, dst, 'a'::text AS rel, ((src * dst) % 7 - 2)::float8 AS w FROM edges WHERE src < dst
UNION ALL SELECT src, dst, 'b', 1 FROM edges WHERE src < dst AND (src + dst) % 5 = 0;
SELECT warren.create_graph('g', 'SELECT id FROM g_nodes', 'SELECT src, dst, rel, w FROM g_edges',
                           weighted := true, has_relations := true);
SELECT warren.create_graph('dag', 'SELECT id FROM g_nodes', 'SELECT src, dst, rel, w FROM dag_edges',
                           weighted := true, has_relations := true, build_reverse := false);
SELECT warren.refresh('g'), warren.refresh('dag');
DELETE FROM g_nodes WHERE id = 7;
SELECT warren.refresh('g'), warren.refresh('dag');
SELECT assert.equal((SELECT id_space - node_count FROM warren.versions v JOIN warren.graphs g USING (graph_id)
                     WHERE g.name = 'g' AND v.version_id = g.current_version), 1::bigint, 'node 7 is a tombstone');
SELECT assert.that((SELECT bool_and(deduplicated_edges > 0) FROM warren.versions v JOIN warren.graphs g USING (graph_id)
                    WHERE g.name = 'g'), 'parallel edges in one relation were folded');

-- Core numbers ignore direction and self-loops.
SELECT assert.equal(warren.core_number('g', 'core'), (SELECT max(core) FROM oracle_core_number),
                    'the degeneracy is the largest core number');
SELECT assert.that((SELECT count(DISTINCT core) FROM oracle_core_number) > 2, 'the fixture has several cores');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'core', NULL::bigint) c
                     JOIN oracle_core_number o USING (node_id) WHERE c.value = o.core), 399::bigint,
                    'core_number agrees with NetworkX');

-- Strong components, labelled by the smallest dense id (here the smallest
-- id less one: a first build numbers nodes in id order, and the tombstone
-- keeps its id).
SELECT assert.equal(warren.strong_components_vector('g', 'scc'),
                    (SELECT count(DISTINCT component) FROM oracle_strong_components),
                    'strong_components_vector counts the live components');
SELECT assert.that((SELECT count(*) FROM oracle_strong_components WHERE node_id <> component) > 10,
                   'the fixture has a large strong component');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'scc', NULL::bigint) s
                     JOIN oracle_strong_components o USING (node_id) WHERE s.value = o.component - 1),
                    399::bigint, 'strong_components_vector agrees with NetworkX');

-- The whole graph has cycles; its edges up the ids do not.
SELECT assert.raises($$SELECT warren.topological_order('g', 'o')$$, 'a cycle has no order', '22023');
SELECT warren.topological_order('dag', 'o');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('dag', 'o', NULL::bigint) t
                     JOIN oracle_topological_order o USING (node_id) WHERE t.value = o.position), 399::bigint,
                    'topological_order is NetworkX''s lexicographical order');
SELECT assert.equal(warren.dag_longest_path('dag', 'l'), (SELECT max(length) FROM oracle_dag_longest_path),
                    'dag_longest_path returns the longest path''s length');
SELECT assert.that((SELECT max(length) FROM oracle_dag_longest_path) > 5, 'the fixture has long paths');
SELECT assert.that((SELECT count(*) FROM oracle_dag_longest_path WHERE length = 0) > 50,
                   'negative weights make paths restart');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('dag', 'l', NULL::bigint) l
                     JOIN oracle_dag_longest_path o USING (node_id) WHERE l.value = o.length), 399::bigint,
                    'dag_longest_path agrees with NetworkX');

-- The forest's weight per component (ties may choose other edges).
CREATE TABLE forest AS SELECT * FROM warren.minimum_spanning_forest('g', NULL::bigint);
CREATE TABLE forest_weights AS
SELECT c.component, coalesce(sum(f.weight), 0) AS weight
FROM (SELECT DISTINCT min(node_id) OVER (PARTITION BY component_id) AS component, component_id
      FROM warren.components('g', NULL::bigint)) c
LEFT JOIN warren.components('g', NULL::bigint) n USING (component_id)
LEFT JOIN forest f ON f.source = n.node_id
GROUP BY c.component;
SELECT assert.that((SELECT count(*) FROM oracle_spanning_forest) > 1, 'the fixture has several components');
SELECT assert.equal((SELECT count(*) FROM forest), 399 - (SELECT count(*) FROM oracle_spanning_forest),
                    'a spanning forest has an edge per node less one per component');
SELECT assert.equal((SELECT count(*) FROM ((TABLE forest_weights EXCEPT TABLE oracle_spanning_forest)
                                           UNION ALL (TABLE oracle_spanning_forest EXCEPT TABLE forest_weights)) d),
                    0::bigint, 'each component''s forest weighs what NetworkX''s does');
