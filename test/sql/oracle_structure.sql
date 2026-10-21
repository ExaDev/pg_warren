-- The structural kernels against NetworkX (DESIGN.md §6.7, §12), on one
-- directed random graph: core_number (undirected), strong_components_vector,
-- topological_order and dag_longest_path (on its edges from a smaller id to
-- a larger, a DAG), and minimum_spanning_forest (per component total weight,
-- with weights that differ by direction).
-- fixture: random --nodes 400 --edges 900 --directed --seed 11
-- oracle: core_number
-- oracle: strong_components --directed
-- oracle: topological_order --directed --edges-sql 'SELECT src, dst FROM edges WHERE src < dst'
-- oracle: dag_longest_path --directed --weighted --edges-sql 'SELECT src, dst, (1 + (src * dst) % 5)::float8 FROM edges WHERE src < dst'
-- oracle: spanning_forest --directed --weighted --edges-sql 'SELECT src, dst, (1 + (src * 3 + dst) % 11)::float8 FROM edges'
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('g');

-- Core numbers ignore direction.
SELECT assert.equal(warren.core_number('g', 'core'), (SELECT max(core) FROM oracle_core_number),
                    'the degeneracy is the largest core number');
SELECT assert.that((SELECT count(DISTINCT core) FROM oracle_core_number) > 2, 'the fixture has several cores');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'core', NULL::bigint) c
                     JOIN oracle_core_number o USING (node_id) WHERE c.value = o.core), 400::bigint,
                    'core_number agrees with NetworkX');

-- Strong components, labelled by the smallest dense id (here the smallest
-- id less one: a first build numbers nodes in id order).
SELECT assert.equal(warren.strong_components_vector('g', 'scc'),
                    (SELECT count(DISTINCT component) FROM oracle_strong_components),
                    'strong_components_vector counts the components');
SELECT assert.that((SELECT count(*) FROM oracle_strong_components WHERE node_id <> component) > 10,
                   'the fixture has a large strong component');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'scc', NULL::bigint) s
                     JOIN oracle_strong_components o USING (node_id) WHERE s.value = o.component - 1),
                    400::bigint, 'strong_components_vector agrees with NetworkX');

-- The whole graph has cycles; its edges up the ids do not.
SELECT assert.raises($$SELECT warren.topological_order('g', 'o')$$, 'a cycle has no order', '22023');
SELECT warren.create_graph('dag', 'SELECT id FROM nodes',
                           'SELECT src, dst, (1 + (src * dst) % 5)::float8 FROM edges WHERE src < dst',
                           weighted := true);
SELECT warren.refresh('dag');
SELECT warren.topological_order('dag', 'o');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('dag', 'o', NULL::bigint) t
                     JOIN oracle_topological_order o USING (node_id) WHERE t.value = o.position), 400::bigint,
                    'topological_order is NetworkX''s lexicographical order');
SELECT assert.equal(warren.dag_longest_path('dag', 'l'), (SELECT max(length) FROM oracle_dag_longest_path),
                    'dag_longest_path returns the longest path''s length');
SELECT assert.that((SELECT max(length) FROM oracle_dag_longest_path) > 10, 'the fixture has long paths');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('dag', 'l', NULL::bigint) l
                     JOIN oracle_dag_longest_path o USING (node_id) WHERE l.value = o.length), 400::bigint,
                    'dag_longest_path agrees with NetworkX');

-- The forest's weight per component (ties may choose other edges).
SELECT warren.create_graph('w', 'SELECT id FROM nodes',
                           'SELECT src, dst, (1 + (src * 3 + dst) % 11)::float8 FROM edges', weighted := true);
SELECT warren.refresh('w');
CREATE TABLE forest AS SELECT * FROM warren.minimum_spanning_forest('w', NULL::bigint);
CREATE TABLE forest_weights AS
SELECT c.component, coalesce(sum(f.weight), 0) AS weight
FROM (SELECT DISTINCT min(node_id) OVER (PARTITION BY component_id) AS component, component_id
      FROM warren.components('w', NULL::bigint)) c
LEFT JOIN warren.components('w', NULL::bigint) n USING (component_id)
LEFT JOIN forest f ON f.source = n.node_id
GROUP BY c.component;
SELECT assert.that((SELECT count(*) FROM oracle_spanning_forest) > 1, 'the fixture has several components');
SELECT assert.equal((SELECT count(*) FROM forest), 400 - (SELECT count(*) FROM oracle_spanning_forest),
                    'a spanning forest has an edge per node less one per component');
SELECT assert.equal((SELECT count(*) FROM ((TABLE forest_weights EXCEPT TABLE oracle_spanning_forest)
                                           UNION ALL (TABLE oracle_spanning_forest EXCEPT TABLE forest_weights)) d),
                    0::bigint, 'each component''s forest weighs what NetworkX''s does');
