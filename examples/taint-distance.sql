SELECT warren.vector_from_query('example_social', 'is_tainted',
    'SELECT id, true FROM warren_examples.tainted', dtype := 'bool');

SELECT warren.vector_from_query('example_social', 'taint_distance', $query$
    SELECT node_id, depth::float8
    FROM warren.bfs('example_social',
        ARRAY(SELECT id FROM warren_examples.tainted), NULL::integer)
$query$, fill := 'Infinity'::float8);

SELECT value AS distance, count(*) AS people
FROM warren.vector_values('example_social', 'taint_distance', NULL::bigint)
GROUP BY value ORDER BY value;

SELECT p.id, p.name, d.value AS distance_to_nearest_taint
FROM warren.vector_values('example_social', 'taint_distance', NULL::bigint) d
JOIN warren_examples.people p ON p.id = d.node_id
WHERE NOT EXISTS (SELECT 1 FROM warren_examples.tainted t WHERE t.id = p.id)
ORDER BY d.value DESC, p.id LIMIT 20;
