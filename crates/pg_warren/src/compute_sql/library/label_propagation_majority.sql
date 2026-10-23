-- warren.label_propagation_majority (DESIGN.md §6.7): majority-vote label
-- propagation, synchronous and deterministic. Labels start as vector `seed`
-- (default: each node's dense id); each iteration every node takes the most
-- frequent label among its own and its neighbours' (direction ignored, each
-- neighbour once, over `rels`; a self-loop counts the node's own label once
-- more; weights are not read), the smallest on a tie, until no label
-- changes, or after max_iter iterations with a WARNING (a synchronous
-- update can oscillate). NetworkX's label propagation is asynchronous
-- (random order and ties) or semi-synchronous (by colouring), so neither
-- matches step for step. Tier 2: each iteration is one `gather` (mode, with
-- include_self), which counts the labels in-engine, N rows through the
-- executor. Writes vector `out` (f64) and returns the iterations run.
CREATE FUNCTION warren.label_propagation_majority(graph text, "out" text, seed text DEFAULT NULL,
                                                  max_iter int DEFAULT 100, rels text[] DEFAULT NULL,
                                                  version bigint DEFAULT NULL)
RETURNS int LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  info jsonb := warren.info(graph);
  v bigint := coalesce(version, (info ->> 'current_version')::bigint);
  parts text[] := ARRAY['x', 'next'];
  -- Each node's own label and its neighbours', counted in-engine; the most
  -- votes win, then the smallest label.
  votes text := format(
    'SELECT node_id, value
     FROM warren.gather(%1$L, %2$L, NULL::%3$s, NULL::float8, ''mode'', ''both'', %4$L::text[],
                        version := %5$s, include_self := true)',
    graph, '_warren_label_propagation_majority_x', info ->> 'id_type', rels, v);
  nodes bigint;
  delta float8;
  i int := 0;
BEGIN
  IF "out" IS NULL OR max_iter IS NULL THEN
    RAISE EXCEPTION '% must not be NULL', CASE WHEN "out" IS NULL THEN 'out' ELSE 'max_iter' END
      USING ERRCODE = 'null_value_not_allowed';
  END IF;
  IF starts_with("out", '_warren_') OR starts_with(seed, '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  IF max_iter < 1 THEN
    RAISE EXCEPTION 'max_iter must be at least 1, not %', max_iter USING ERRCODE = 'invalid_parameter_value';
  END IF;
  BEGIN
    IF seed IS NULL THEN
      PERFORM warren.vector_dense_id(graph, '_warren_label_propagation_majority_x', 'f64', false, v);
    ELSE
      -- vector_reduce names a missing seed vector (42704); vector_map would
      -- call it a missing column.
      PERFORM warren.vector_reduce(graph, seed, 'nnz', v);
      PERFORM warren.vector_map(graph, '_warren_label_propagation_majority_x', format('%I::float8', seed), false, v);
    END IF;
    -- No live nodes, no labels: no iterations (NetworkX finds no communities).
    nodes := (warren.vector_stats(graph, '_warren_label_propagation_majority_x', v) ->> 'count')::bigint;
    WHILE i < max_iter AND nodes > 0 LOOP
      i := i + 1;
      PERFORM warren.report_progress('label_propagation_majority', graph, 'iterating', i - 1, max_iter);
      PERFORM warren.vector_from_query(graph, '_warren_label_propagation_majority_next', votes, 'f64',
                                       persist := false, version := v);
      delta := warren.vector_norm_diff(graph, '_warren_label_propagation_majority_next',
                                       '_warren_label_propagation_majority_x', 'l1', v, v);
      PERFORM warren.vector_map(graph, '_warren_label_propagation_majority_x',
                                '_warren_label_propagation_majority_next', false, v);
      EXIT WHEN delta = 0;
    END LOOP;
    IF delta <> 0 THEN
      RAISE WARNING 'label_propagation_majority did not converge in % iterations', i;
    END IF;
    PERFORM warren.vector_map(graph, "out", '_warren_label_propagation_majority_x', true, v);
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_label_propagation_majority_' || part, v, true)
      FROM unnest(parts) part;
    RAISE;
  END;
  PERFORM warren.drop_vector(graph, '_warren_label_propagation_majority_' || part, v, true) FROM unnest(parts) part;
  RETURN i;
END $$;
