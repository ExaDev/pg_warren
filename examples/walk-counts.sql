SELECT warren.vector_from_query('example_social', 'walk0',
    'SELECT 1::bigint, 1::float8');

-- Each product extends every walk by one outgoing edge.
DO $$
BEGIN
    FOR hop IN 1..6 LOOP
        PERFORM warren.spmv('example_social', 'walk' || (hop - 1),
            'walk' || hop, 'plus_times', transpose := true);
    END LOOP;
END $$;

SELECT p.name, r.value::bigint AS six_hop_walks
FROM warren.vector_topk('example_social', 'walk6', 15, NULL::bigint) r
JOIN warren_examples.people p ON p.id = r.node_id
ORDER BY r.value DESC, p.id;

SELECT warren.vector_reduce('example_social', 'walk6', 'sum') AS total_walks,
       warren.vector_reduce('example_social', 'walk6', 'nnz') AS distinct_endpoints;
