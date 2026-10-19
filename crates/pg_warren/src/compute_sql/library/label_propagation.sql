-- warren.label_propagation (DESIGN.md §6.7, A.1): min-label propagation.
-- Labels start as vector `seed` (default: each node's dense id); each
-- iteration every node takes the smallest label among itself and its
-- neighbours (both directions, over `rels`), until no label changes or
-- after max_iter iterations. With `mask`, only the nodes it selects (a
-- non-zero value, GraphBLAS semantics) send or take labels; the others
-- keep theirs. It is deterministic min-propagation, not majority-vote
-- label propagation. Writes vector `out` (f64) and returns the iterations
-- run.
CREATE FUNCTION warren.label_propagation(graph text, "out" text, seed text DEFAULT NULL,
                                         max_iter int DEFAULT 100, mask text DEFAULT NULL,
                                         rels text[] DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS int LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['x', 'msg', 'next'];
  selected text;
  delta float8;
  i int := 0;
BEGIN
  IF starts_with("out", '_warren_') OR starts_with(seed, '_warren_') OR starts_with(mask, '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  BEGIN
    IF seed IS NULL THEN
      PERFORM warren.vector_dense_id(graph, '_warren_label_propagation_x', 'f64', false, v);
    ELSE
      PERFORM warren.vector_map(graph, '_warren_label_propagation_x', format('%I::float8', seed), false, v);
    END IF;
    IF mask IS NOT NULL THEN
      selected := CASE warren.explain_map(graph, format('%I', mask), v) ->> 'dtype'
                    WHEN 'bool' THEN format('%I', mask) ELSE format('%I <> 0', mask) END;
    END IF;
    WHILE i < max_iter LOOP
      i := i + 1;
      -- Unselected nodes send NULL (NaN), which min never picks over a label.
      PERFORM warren.vector_map(graph, '_warren_label_propagation_msg',
        CASE WHEN selected IS NULL THEN '_warren_label_propagation_x'
             ELSE format('CASE WHEN %s THEN _warren_label_propagation_x END', selected) END, false, v);
      PERFORM warren.vector_map(graph, '_warren_label_propagation_next', '_warren_label_propagation_x', false, v);
      PERFORM warren.spmv(graph, '_warren_label_propagation_msg', '_warren_label_propagation_next', 'min_second',
                          rels := rels, mask := mask, accumulate := 'min', persist := false, version := v);
      PERFORM warren.spmv(graph, '_warren_label_propagation_msg', '_warren_label_propagation_next', 'min_second',
                          transpose := true, rels := rels, mask := mask, accumulate := 'min', persist := false,
                          version := v);
      delta := warren.vector_norm_diff(graph, '_warren_label_propagation_next', '_warren_label_propagation_x',
                                       'l1', v, v);
      PERFORM warren.vector_map(graph, '_warren_label_propagation_x', '_warren_label_propagation_next', false, v);
      EXIT WHEN delta = 0;
    END LOOP;
    PERFORM warren.vector_map(graph, "out", '_warren_label_propagation_x', true, v);
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_label_propagation_' || part, v, true) FROM unnest(parts) part;
    RAISE;
  END;
  PERFORM warren.drop_vector(graph, '_warren_label_propagation_' || part, v, true) FROM unnest(parts) part;
  RETURN i;
END $$;
