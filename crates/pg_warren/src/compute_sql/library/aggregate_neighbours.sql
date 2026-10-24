-- warren.aggregate_neighbours (DESIGN.md §6.7, A.5): each node's row of
-- matrix `x` replaced by the mean, sum or max of its neighbours' rows
-- (out-neighbours, or in-neighbours with direction := 'in'; distinct
-- neighbours over `rels`, or with `per_relation` a neighbour once in each
-- relation linking it (DESIGN.md §5.3); weights are not read), `hops`
-- times over. A node with no neighbours gets NaN for mean (SQL's AVG of
-- nothing), 0 for sum and -Infinity (the least bigint) for max. mean
-- needs an f32 or f64 matrix; the result keeps x's dtype. Writes matrix
-- `out`.
CREATE FUNCTION warren.aggregate_neighbours(graph text, x text, "out" text, agg text DEFAULT 'mean',
                                            hops int DEFAULT 1, rels text[] DEFAULT NULL,
                                            direction text DEFAULT 'out', version bigint DEFAULT NULL,
                                            per_relation boolean DEFAULT false)
RETURNS void LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['x', 'degree'];
  semiring text := CASE agg WHEN 'mean' THEN 'plus_second' WHEN 'sum' THEN 'plus_second'
                            WHEN 'max' THEN 'max_second' END;
  dtype text;
BEGIN
  IF starts_with("out", '_warren_') OR starts_with(x, '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  IF semiring IS NULL THEN
    RAISE EXCEPTION 'agg must be mean, sum or max, not %', agg USING ERRCODE = 'invalid_parameter_value';
  END IF;
  IF direction NOT IN ('out', 'in') THEN
    RAISE EXCEPTION 'direction must be ''out'' or ''in'', not %', direction USING ERRCODE = 'invalid_parameter_value';
  END IF;
  dtype := warren.explain_map(graph, format('%I', x), v) ->> 'dtype';
  IF agg = 'mean' AND dtype NOT IN ('f32', 'f64') THEN
    RAISE EXCEPTION 'mean needs an f32 or f64 matrix, not %', dtype USING ERRCODE = 'datatype_mismatch';
  END IF;
  BEGIN
    PERFORM warren.vector_map(graph, '_warren_aggregate_neighbours_x', format('%I', x), false, v);
    IF agg = 'mean' THEN
      PERFORM warren.vector_degree(graph, '_warren_aggregate_neighbours_degree', direction, rels, dtype, false, v,
                                   per_relation);
    END IF;
    FOR hop IN 1..hops LOOP
      PERFORM warren.report_progress('aggregate_neighbours', graph, 'hop', hop - 1, hops);
      PERFORM warren.spmm(graph, '_warren_aggregate_neighbours_x', '_warren_aggregate_neighbours_x', semiring,
                          transpose := direction = 'in', rels := rels, weighted := false, persist := false,
                          version := v, per_relation := per_relation);
      IF agg = 'mean' THEN
        PERFORM warren.vector_map(graph, '_warren_aggregate_neighbours_x',
          'CASE WHEN _warren_aggregate_neighbours_degree > 0
                THEN _warren_aggregate_neighbours_x / _warren_aggregate_neighbours_degree END', false, v);
      END IF;
    END LOOP;
    PERFORM warren.vector_map(graph, "out", '_warren_aggregate_neighbours_x', true, v);
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_aggregate_neighbours_' || part, v, true) FROM unnest(parts) part;
    RAISE;
  END;
  PERFORM warren.drop_vector(graph, '_warren_aggregate_neighbours_' || part, v, true) FROM unnest(parts) part;
END $$;
