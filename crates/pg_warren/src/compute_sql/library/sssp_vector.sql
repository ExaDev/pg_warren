-- warren.sssp_vector (DESIGN.md §6.7): single-source shortest-path
-- distances by Bellman-Ford relaxation (min_plus SpMV along edges,
-- accumulating min) until no distance falls, or after max_iter iterations.
-- By default the bound is the live node count, and distances still falling
-- then mean a negative cycle reachable from the source, an error (22023).
-- Weights as spmv reads them (1 if unweighted); unreachable nodes are
-- Infinity. Writes vector `out` (f64) and returns the iterations run.
CREATE FUNCTION warren.sssp_vector(graph text, "out" text, source anyelement,
                                   rels text[] DEFAULT NULL, weighted boolean DEFAULT NULL,
                                   max_iter int DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS int LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['x', 'next', 'fell'];
  limit_ int;
  falling float8;
  i int := 0;
BEGIN
  IF starts_with("out", '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  BEGIN
    PERFORM warren.vector_from_query(graph, '_warren_sssp_vector_x',
                                     format('SELECT %L::%s, 0', source, pg_typeof(source)),
                                     fill := 'Infinity', persist := false, version := v);
    IF warren.vector_reduce(graph, '_warren_sssp_vector_x', 'min', v) <> 0 THEN
      RAISE EXCEPTION 'node % is not a node of graph "%"', source, graph USING ERRCODE = 'invalid_parameter_value';
    END IF;
    limit_ := coalesce(max_iter, (warren.vector_stats(graph, '_warren_sssp_vector_x', v) ->> 'count')::int);
    WHILE i < limit_ LOOP
      i := i + 1;
      PERFORM warren.vector_map(graph, '_warren_sssp_vector_next', '_warren_sssp_vector_x', false, v);
      PERFORM warren.spmv(graph, '_warren_sssp_vector_x', '_warren_sssp_vector_next', 'min_plus',
                          transpose := true, rels := rels, weighted := weighted, accumulate := 'min',
                          persist := false, version := v);
      PERFORM warren.vector_map(graph, '_warren_sssp_vector_fell',
        'CASE WHEN _warren_sssp_vector_next < _warren_sssp_vector_x THEN 1 ELSE 0 END', false, v);
      falling := warren.vector_reduce(graph, '_warren_sssp_vector_fell', 'sum', v);
      PERFORM warren.vector_map(graph, '_warren_sssp_vector_x', '_warren_sssp_vector_next', false, v);
      EXIT WHEN falling = 0;
    END LOOP;
    IF max_iter IS NULL AND falling > 0 THEN
      RAISE EXCEPTION 'a negative cycle is reachable from node % of graph "%"', source, graph
        USING ERRCODE = 'invalid_parameter_value';
    END IF;
    PERFORM warren.vector_map(graph, "out", '_warren_sssp_vector_x', true, v);
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_sssp_vector_' || part, v, true) FROM unnest(parts) part;
    RAISE;
  END;
  PERFORM warren.drop_vector(graph, '_warren_sssp_vector_' || part, v, true) FROM unnest(parts) part;
  RETURN i;
END $$;
