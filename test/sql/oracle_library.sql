-- The algorithm library against references (DESIGN.md §6.7, §12, A.5), on
-- one undirected random graph: components_vector and triangles against
-- NetworkX, sssp_vector against warren.dijkstra, and aggregate_neighbours
-- against SQL's AVG over neighbour rows (f32, within 1e-5).
-- fixture: random --nodes 400 --edges 1200 --seed 5
-- oracle: components
-- oracle: triangles
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges', directed := false);
SELECT warren.refresh('g');

-- components_vector labels a component by its smallest dense id; relabel
-- by the smallest external id to compare partitions with NetworkX's.
SELECT warren.components_vector('g', 'cc');
CREATE TABLE warren_components AS
SELECT node_id, min(node_id) OVER (PARTITION BY value) AS component FROM warren.vector_values('g', 'cc', NULL::bigint);
SELECT assert.that((SELECT count(DISTINCT component) FROM oracle_components) > 1, 'the fixture has several components');
SELECT assert.equal((SELECT count(*) FROM ((TABLE warren_components EXCEPT TABLE oracle_components)
                                           UNION ALL (TABLE oracle_components EXCEPT TABLE warren_components)) d),
                    0::bigint, 'components_vector agrees with NetworkX');

SELECT warren.triangles('g', 'tri');
SELECT assert.that((SELECT sum(triangles) FROM oracle_triangles) > 0, 'the fixture has triangles');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('g', 'tri', NULL::bigint) t
                     JOIN oracle_triangles o USING (node_id) WHERE t.value = o.triangles), 400::bigint,
                    'triangles agrees with NetworkX');

-- Weighted shortest paths from node 1.
SELECT warren.create_graph('w', 'SELECT id FROM nodes', 'SELECT src, dst, (1 + (src * dst) % 5)::float8 FROM edges',
                           directed := false, weighted := true);
SELECT warren.refresh('w');
SELECT warren.sssp_vector('w', 'd', 1::bigint);
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('w', 'd', NULL::bigint) v
                     LEFT JOIN LATERAL warren.dijkstra('w', 1::bigint, v.node_id) p ON true
                     WHERE v.value IS DISTINCT FROM coalesce(p.cost, 'Infinity')), 0::bigint,
                    'sssp_vector agrees with dijkstra');
SELECT assert.that((SELECT count(*) FROM warren.vector_values('w', 'd', NULL::bigint) WHERE value = 'Infinity') > 0,
                   'some nodes are unreachable');

-- An 8-dimensional f32 embedding per node, averaged over neighbours.
CREATE TABLE embeddings AS
SELECT id, array_agg(sin(id * k)::real ORDER BY k) AS embedding FROM nodes, generate_series(1, 8) k GROUP BY id;
SELECT warren.vector_from_query('g', 'emb', 'SELECT id, embedding FROM embeddings', 'f32');
SELECT warren.aggregate_neighbours('g', 'emb', 'emb_nbr');
CREATE TABLE sql_mean AS
SELECT n.a AS node_id, k, avg(e.embedding[k]) AS value
FROM (SELECT DISTINCT a, b FROM (SELECT src AS a, dst AS b FROM edges UNION ALL SELECT dst, src FROM edges) u
      ) n
JOIN embeddings e ON e.id = n.b, generate_series(1, 8) k
GROUP BY n.a, k;
SELECT assert.that((SELECT max(abs(r.row[s.k] - s.value))
                    FROM warren.matrix_rows('g', 'emb_nbr', NULL::bigint) r JOIN sql_mean s USING (node_id)) <= 1e-5,
                   'aggregate_neighbours equals SQL AVG over neighbour rows');
SELECT assert.that((SELECT bool_and(r.row[1] = 'NaN') FROM warren.matrix_rows('g', 'emb_nbr', NULL::bigint) r
                    WHERE NOT EXISTS (SELECT 1 FROM sql_mean s WHERE s.node_id = r.node_id)),
                   'nodes without neighbours get NaN');
SELECT assert.equal((SELECT count(DISTINCT node_id) FROM sql_mean)
                    + (SELECT count(*) FROM nodes WHERE id NOT IN (SELECT node_id FROM sql_mean)), 400::bigint,
                    'every node is compared');
