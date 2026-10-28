SELECT warren.louvain('example_social', 'louvain', seed := 42) AS louvain_modularity;
SELECT warren.leiden('example_social', 'leiden', seed := 42) AS leiden_modularity;

WITH labels AS (
    SELECT 'Louvain' AS algorithm, node_id, value::bigint AS community
    FROM warren.vector_values('example_social', 'louvain', NULL::bigint)
    UNION ALL
    SELECT 'Leiden', node_id, value::bigint
    FROM warren.vector_values('example_social', 'leiden', NULL::bigint)
), sizes AS (
    SELECT algorithm, community, count(*) AS members
    FROM labels GROUP BY algorithm, community
)
SELECT algorithm, count(*) AS communities, min(members) AS smallest,
       max(members) AS largest, round(avg(members), 1) AS average_size
FROM sizes GROUP BY algorithm ORDER BY algorithm;

SELECT c.value::bigint AS community, count(*) AS members,
       (array_agg(p.name ORDER BY p.id))[1:5] AS sample_members
FROM warren.vector_values('example_social', 'louvain', NULL::bigint) c
JOIN warren_examples.people p ON p.id = c.node_id
GROUP BY c.value ORDER BY members DESC, community LIMIT 10;
