-- warren.katz_centrality (DESIGN.md §6.7): Katz centrality by power
-- iteration, as NetworkX computes it. Centrality flows along edges,
-- weighted by edge weight (spmv's rule: weights if the graph has them,
-- unless weighted := false), attenuated by alpha, plus each node's beta:
-- the constant `beta`, or beta × vector `seed` (NetworkX's per-node beta).
-- From 0, each iteration takes alpha · Aᵀx + β; it stops when the L1 change
-- falls below (live nodes) × tol, or after max_iter iterations with a
-- WARNING (NetworkX raises; alpha at or above 1 / the largest eigenvalue
-- diverges). With normalized, the result is scaled to unit L2 norm. Writes
-- vector `out` (f64) and returns the iterations run. Constants enter
-- expressions as constant vectors.
CREATE FUNCTION warren.katz_centrality(graph text, "out" text, alpha float8 DEFAULT 0.1,
                                       beta float8 DEFAULT 1.0, seed text DEFAULT NULL,
                                       normalized boolean DEFAULT true, tol float8 DEFAULT 1e-6,
                                       max_iter int DEFAULT 1000, rels text[] DEFAULT NULL,
                                       weighted boolean DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS int LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['one', 'b', 'alpha', 'x', 'y', 'next', 'scale'];
  nodes float8;
  norm float8;
  delta float8;
  i int := 0;
BEGIN
  IF starts_with("out", '_warren_') OR starts_with(seed, '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  IF tol IS NULL OR tol < 0 THEN
    RAISE EXCEPTION 'tol must be at least 0, not %', coalesce(tol::text, 'NULL')
      USING ERRCODE = 'invalid_parameter_value';
  END IF;
  IF max_iter IS NULL OR max_iter < 1 THEN
    RAISE EXCEPTION 'max_iter must be at least 1, not %', coalesce(max_iter::text, 'NULL')
      USING ERRCODE = 'invalid_parameter_value';
  END IF;
  BEGIN
    PERFORM warren.vector_const(graph, '_warren_katz_centrality_one', 1, persist := false, version := v);
    nodes := warren.vector_reduce(graph, '_warren_katz_centrality_one', 'sum', v);
    IF nodes = 0 THEN
      RAISE EXCEPTION 'graph "%" has no live nodes', graph USING ERRCODE = 'invalid_parameter_value';
    END IF;
    PERFORM warren.vector_const(graph, '_warren_katz_centrality_b', beta, persist := false, version := v);
    IF seed IS NOT NULL THEN
      PERFORM warren.vector_map(graph, '_warren_katz_centrality_b',
                                format('_warren_katz_centrality_b * %I::float8', seed), false, v);
    END IF;
    PERFORM warren.vector_const(graph, '_warren_katz_centrality_alpha', alpha, persist := false, version := v);
    PERFORM warren.vector_const(graph, '_warren_katz_centrality_x', 0, persist := false, version := v);
    WHILE i < max_iter LOOP
      i := i + 1;
      PERFORM warren.spmv(graph, '_warren_katz_centrality_x', '_warren_katz_centrality_y',
                          transpose := true, rels := rels, weighted := weighted, persist := false, version := v);
      PERFORM warren.vector_map(graph, '_warren_katz_centrality_next',
        '_warren_katz_centrality_alpha * _warren_katz_centrality_y + _warren_katz_centrality_b', false, v);
      delta := warren.vector_norm_diff(graph, '_warren_katz_centrality_next', '_warren_katz_centrality_x',
                                       'l1', v, v);
      PERFORM warren.vector_map(graph, '_warren_katz_centrality_x', '_warren_katz_centrality_next', false, v);
      EXIT WHEN delta < nodes * tol;
    END LOOP;
    IF NOT delta < nodes * tol THEN
      RAISE WARNING 'katz_centrality did not converge in % iterations', i;
    END IF;
    IF normalized THEN
      norm := warren.vector_reduce(graph, '_warren_katz_centrality_x', 'l2', v);
      PERFORM warren.vector_const(graph, '_warren_katz_centrality_scale',
                                  CASE WHEN norm = 0 THEN 1 ELSE 1.0 / norm END, persist := false, version := v);
      PERFORM warren.vector_map(graph, "out", '_warren_katz_centrality_x * _warren_katz_centrality_scale', true, v);
    ELSE
      PERFORM warren.vector_map(graph, "out", '_warren_katz_centrality_x', true, v);
    END IF;
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_katz_centrality_' || part, v, true) FROM unnest(parts) part;
    RAISE;
  END;
  PERFORM warren.drop_vector(graph, '_warren_katz_centrality_' || part, v, true) FROM unnest(parts) part;
  RETURN i;
END $$;
