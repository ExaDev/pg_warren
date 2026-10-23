-- warren.components_vector (DESIGN.md §6.7): weakly connected components by
-- min-label propagation (min_second SpMV both ways, accumulating min) until
-- no label changes. Each component is labelled by its smallest dense id,
-- as warren.components labels it. Writes vector `out` (i64) and returns
-- the iterations run.
CREATE FUNCTION warren.components_vector(graph text, "out" text, rels text[] DEFAULT NULL,
                                         version bigint DEFAULT NULL)
RETURNS int LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['x', 'next'];
  delta float8;
  i int := 0;
BEGIN
  IF starts_with("out", '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  BEGIN
    PERFORM warren.vector_dense_id(graph, '_warren_components_vector_x', 'i64', false, v);
    LOOP
      i := i + 1;
      PERFORM warren.report_progress('components_vector', graph, 'iterating', i - 1, NULL);
      PERFORM warren.vector_map(graph, '_warren_components_vector_next', '_warren_components_vector_x', false, v);
      PERFORM warren.spmv(graph, '_warren_components_vector_x', '_warren_components_vector_next', 'min_second',
                          rels := rels, accumulate := 'min', persist := false, version := v);
      PERFORM warren.spmv(graph, '_warren_components_vector_x', '_warren_components_vector_next', 'min_second',
                          transpose := true, rels := rels, accumulate := 'min', persist := false, version := v);
      delta := warren.vector_norm_diff(graph, '_warren_components_vector_next', '_warren_components_vector_x',
                                       'l1', v, v);
      PERFORM warren.vector_map(graph, '_warren_components_vector_x', '_warren_components_vector_next', false, v);
      EXIT WHEN delta = 0;
    END LOOP;
    PERFORM warren.vector_map(graph, "out", '_warren_components_vector_x', true, v);
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_components_vector_' || part, v, true) FROM unnest(parts) part;
    RAISE;
  END;
  PERFORM warren.drop_vector(graph, '_warren_components_vector_' || part, v, true) FROM unnest(parts) part;
  RETURN i;
END $$;
