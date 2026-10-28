-- Node ids are the original people.id values. Look up a start dynamically.
SELECT depth, count(*) AS people_reached
FROM warren.bfs('example_social', ARRAY(
    SELECT id FROM warren_examples.people WHERE name = 'person-1'
), 10)
GROUP BY depth ORDER BY depth;

-- Return business rows, including a predecessor for each shortest route.
SELECT p.id, p.name, b.depth, previous.name AS reached_via
FROM warren.bfs('example_social', ARRAY(
    SELECT id FROM warren_examples.people WHERE name = 'person-1'
), 3) b
JOIN warren_examples.people p ON p.id = b.node_id
LEFT JOIN warren_examples.people previous ON previous.id = b.via
ORDER BY b.depth, p.id;

SELECT warren.shortest_path('example_social', 1::bigint, 9999::bigint) AS path;
