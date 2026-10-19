-- warren.aggregate_neighbours (DESIGN.md §6.7, A.5): each node's row of
-- matrix `x` replaced by the mean, sum or max of its neighbours' rows
-- (out-neighbours, or in-neighbours with direction := 'in'; distinct
-- neighbours over `rels`; weights are not read), `hops` times over. A node
-- with no neighbours gets NaN for mean (SQL's AVG of nothing), 0 for sum
-- and -Infinity (the least bigint) for max. mean needs an f32 or f64
-- matrix; the result keeps x's dtype. Writes matrix `out`.
CREATE FUNCTION warren.aggregate_neighbours(graph text, x text, "out" text, agg text DEFAULT 'mean',
                                            hops int DEFAULT 1, rels text[] DEFAULT NULL,
                                            direction text DEFAULT 'out', version bigint DEFAULT NULL)
RETURNS void LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  semiring text := CASE agg WHEN 'mean' THEN 'plus_second' WHEN 'sum' THEN 'plus_second'
                            WHEN 'max' THEN 'max_second' END;
  dtype text := warren.explain_map(graph, format('%I', x), v) ->> 'dtype';
BEGIN
  IF semiring IS NULL THEN
    RAISE EXCEPTION 'agg must be mean, sum or max, not %', agg USING ERRCODE = 'invalid_parameter_value';
  END IF;
  IF direction NOT IN ('out', 'in') THEN
    RAISE EXCEPTION 'direction must be ''out'' or ''in'', not %', direction USING ERRCODE = 'invalid_parameter_value';
  END IF;
  IF agg = 'mean' AND dtype NOT IN ('f32', 'f64') THEN
    RAISE EXCEPTION 'mean needs an f32 or f64 matrix, not %', dtype USING ERRCODE = 'datatype_mismatch';
  END IF;
  PERFORM warren.vector_map(graph, '_warren_aggregate_neighbours_x', format('%I', x), false, v);
  IF agg = 'mean' THEN
    PERFORM warren.vector_degree(graph, '_warren_aggregate_neighbours_degree', direction, rels, dtype, false, v);
  END IF;
  FOR i IN 1..hops LOOP
    PERFORM warren.spmm(graph, '_warren_aggregate_neighbours_x', '_warren_aggregate_neighbours_x', semiring,
                        transpose := direction = 'in', rels := rels, weighted := false, persist := false,
                        version := v);
    IF agg = 'mean' THEN
      PERFORM warren.vector_map(graph, '_warren_aggregate_neighbours_x',
        'CASE WHEN _warren_aggregate_neighbours_degree > 0
              THEN _warren_aggregate_neighbours_x / _warren_aggregate_neighbours_degree END', false, v);
    END IF;
  END LOOP;
  PERFORM warren.vector_map(graph, "out", '_warren_aggregate_neighbours_x', true, v);
  PERFORM warren.drop_vector(graph, '_warren_aggregate_neighbours_' || part, v)
  FROM unnest(ARRAY['x', 'degree']) part
  WHERE part = 'x' OR agg = 'mean';
END $$;
