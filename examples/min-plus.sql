SELECT warren.vector_from_query('example_social', 'distance',
    'SELECT 1::bigint, 0::float8', fill := 'Infinity'::float8);

DO $$
DECLARE changed double precision;
BEGIN
    FOR iteration IN 1..100 LOOP
        PERFORM warren.vector_map('example_social', 'next_distance', 'distance', persist := false);
        PERFORM warren.spmv('example_social', 'distance', 'next_distance', 'min_plus',
            transpose := true, accumulate := 'min', persist := false);
        PERFORM warren.vector_map('example_social', 'changed',
            'CASE WHEN distance <> next_distance THEN 1 ELSE 0 END', persist := false);
        changed := warren.vector_reduce('example_social', 'changed', 'sum');
        PERFORM warren.vector_map('example_social', 'distance', 'next_distance');
        RAISE NOTICE 'Round %: % distances improved', iteration, changed;
        IF changed = 0 THEN RETURN; END IF;
    END LOOP;
    RAISE EXCEPTION 'Distances did not converge within 100 rounds';
END $$;

SELECT value AS shortest_distance, count(*) AS people
FROM warren.vector_values('example_social', 'distance', NULL::bigint)
GROUP BY value ORDER BY value;

SELECT count(*) AS mismatches
FROM warren.vector_values('example_social', 'distance', NULL::bigint) d
FULL JOIN warren.bfs('example_social', ARRAY[1::bigint], NULL::integer) b USING (node_id)
WHERE d.value IS DISTINCT FROM b.depth::float8;
