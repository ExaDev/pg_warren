-- The exact scripts embedded in the website's example library.
\ir ../../examples/setup.sql
\ir ../../examples/bfs.sql
\ir ../../examples/communities.sql
\ir ../../examples/walk-counts.sql
\ir ../../examples/min-plus.sql
\ir ../../examples/boolean-spread.sql
\ir ../../examples/taint-seeds.sql
\ir ../../examples/taint-distance.sql
\ir ../../examples/taint-routes.sql
\ir ../../examples/versions.sql

SELECT assert.equal((SELECT count(*) FROM warren_examples.people), 10000::bigint, 'example nodes');
SELECT assert.equal((SELECT count(*) FROM warren_examples.follows), 49985::bigint, 'example edges');
SELECT assert.equal((SELECT count(*) FROM warren.bfs('example_social', ARRAY[1::bigint], NULL::integer)),
                    10000::bigint, 'example ring is fully reachable');

-- Independent SQL oracle for BFS and six-hop walk counts.
WITH RECURSIVE walk(node_id, depth) AS (
    SELECT 1::bigint, 0
    UNION ALL
    SELECT f.dst, w.depth + 1 FROM walk w
    JOIN warren_examples.follows f ON f.src = w.node_id WHERE w.depth < 6
), expected AS (
    SELECT node_id, count(*)::float8 AS value FROM walk WHERE depth = 6 GROUP BY node_id
), actual AS (
    SELECT * FROM warren.vector_values('example_social', 'walk6', NULL::bigint) WHERE value <> 0
)
SELECT assert.that(NOT EXISTS (
    SELECT 1 FROM expected e FULL JOIN actual a USING (node_id)
    WHERE e.value IS DISTINCT FROM a.value
), 'plus_times matches recursive SQL walk multiplicities');

SELECT assert.that(NOT EXISTS (
    SELECT 1 FROM warren.vector_values('example_social', 'distance', NULL::bigint) d
    FULL JOIN warren.bfs('example_social', ARRAY[1::bigint], NULL::integer) b USING (node_id)
    WHERE d.value IS DISTINCT FROM b.depth::float8
), 'min_plus matches native BFS');
SELECT assert.equal(warren.vector_reduce('example_social', 'seen', 'nnz'), 10000::float8, 'boolean propagation reaches all nodes');

SELECT assert.equal((SELECT count(*) FROM warren_examples.tainted), 100::bigint, '100 tainted seeds');
SELECT assert.equal((SELECT count(*) FROM warren_examples.taint_routes WHERE distance = 0), 100::bigint, 'all tainted seeds have zero distance');
SELECT assert.equal((SELECT count(*) FROM warren_examples.taint_routes), 10000::bigint, 'all people have routes');
SELECT assert.that(NOT EXISTS (
    SELECT 1 FROM warren_examples.taint_routes r
    LEFT JOIN warren_examples.taint_routes p ON p.node_id = r.toward_taint
    WHERE (r.distance = 0 AND (r.toward_taint IS NOT NULL OR NOT EXISTS (
        SELECT 1 FROM warren_examples.tainted t WHERE t.id = r.node_id)))
       OR (r.distance > 0 AND (p.node_id IS NULL OR p.distance <> r.distance - 1 OR NOT EXISTS (
        SELECT 1 FROM warren_examples.follows f WHERE f.src = p.node_id AND f.dst = r.node_id)))
), 'every route follows real edges back to a seed with decreasing distances');
SELECT assert.that(NOT EXISTS (
    SELECT 1 FROM warren_examples.taint_routes r
    FULL JOIN warren.vector_values('example_social', 'taint_distance', NULL::bigint) d
        ON d.node_id = r.node_id
    WHERE r.distance::float8 IS DISTINCT FROM d.value
), 'route distances match the saved vector');
SELECT assert.that(NOT EXISTS (
    SELECT 1 FROM warren_examples.follows e
    JOIN warren_examples.taint_routes s ON s.node_id = e.src
    JOIN warren_examples.taint_routes t ON t.node_id = e.dst
    WHERE t.distance > s.distance + 1
), 'taint distances satisfy shortest-path relaxation on every edge');

SELECT assert.equal((SELECT count(*) FROM warren.vector_values('example_social', 'louvain', NULL::bigint)),
                    10000::bigint, 'Louvain labels every person');
SELECT assert.equal((SELECT count(*) FROM warren.vector_values('example_social', 'leiden', NULL::bigint)),
                    10000::bigint, 'Leiden labels every person');
SELECT assert.that(warren.modularity('example_social', 'louvain') > 0, 'Louvain finds positive modularity');
SELECT assert.that(warren.modularity('example_social', 'leiden') > 0, 'Leiden finds positive modularity');
SELECT assert.equal((SELECT count(*) FROM warren_examples.people WHERE id = 10001), 0::bigint, 'version example rolls source writes back');
SELECT assert.equal((warren.info('example_social') ->> 'current_version')::bigint,
                    1::bigint, 'version example leaves the current version unchanged');
