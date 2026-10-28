SELECT warren.vector_from_query('example_social', 'seen',
    'SELECT 1::bigint, true', dtype := 'bool');

DO $$
DECLARE reached double precision;
BEGIN
    FOR iteration IN 1..10 LOOP
        PERFORM warren.spmv('example_social', 'seen', 'spread', 'lor_land',
            transpose := true, persist := false);
        PERFORM warren.vector_map('example_social', 'seen', 'seen OR spread');
        reached := warren.vector_reduce('example_social', 'seen', 'nnz');
        RAISE NOTICE 'After % hops: % people reached', iteration, reached;
        IF reached = 10000 THEN RETURN; END IF;
    END LOOP;
END $$;

SELECT p.id, p.name, v.value <> 0 AS reached
FROM warren.vector_values('example_social', 'seen', NULL::bigint) v
JOIN warren_examples.people p ON p.id = v.node_id
ORDER BY p.id LIMIT 20;

SELECT warren.vector_reduce('example_social', 'seen', 'nnz') AS people_reached,
       (SELECT count(*) FROM warren.bfs('example_social', ARRAY[1::bigint], 10)) AS bfs_crosscheck;
