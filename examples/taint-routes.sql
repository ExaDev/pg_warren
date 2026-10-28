CREATE TABLE IF NOT EXISTS warren_examples.taint_routes (
    node_id bigint PRIMARY KEY,
    distance integer NOT NULL,
    toward_taint bigint
);
-- Replace the saved pointers atomically, even when pgAdmin autocommits.
DO $$
BEGIN
    DELETE FROM warren_examples.taint_routes;
    INSERT INTO warren_examples.taint_routes (node_id, distance, toward_taint)
    SELECT node_id, depth, via
    FROM warren.bfs('example_social',
        ARRAY(SELECT id FROM warren_examples.tainted), NULL::integer);
END $$;

SELECT p.name, r.distance, neighbour.name AS next_person_toward_taint
FROM warren_examples.taint_routes r
JOIN warren_examples.people p ON p.id = r.node_id
LEFT JOIN warren_examples.people neighbour ON neighbour.id = r.toward_taint
ORDER BY r.distance DESC, p.id LIMIT 10;

WITH RECURSIVE route AS (
    SELECT node_id, distance, toward_taint, ARRAY[node_id] AS path
    FROM warren_examples.taint_routes
    WHERE node_id = (SELECT id FROM warren_examples.people WHERE name = 'person-1316')
    UNION ALL
    SELECT next.node_id, next.distance, next.toward_taint, route.path || next.node_id
    FROM route
    JOIN warren_examples.taint_routes next ON next.node_id = route.toward_taint
    WHERE next.distance = route.distance - 1
)
SELECT node_id AS nearest_tainted_person, path AS path_to_taint
FROM route WHERE distance = 0;
