-- warren.eigenvector_centrality (DESIGN.md §6.7): eigenvector centrality
-- by power iteration, as NetworkX computes it. Centrality flows along edges
-- (the left eigenvector of A: a node is central when central nodes point at
-- it), weighted by edge weight (spmv's rule: weights if the graph has them,
-- unless weighted := false). From the uniform 1/N, each iteration takes
-- x + Aᵀx (the shift by I keeps a periodic graph from oscillating) and
-- scales it to unit L2 norm; it stops when the L1 change falls below
-- (live nodes) × tol, or after max_iter iterations with a WARNING (NetworkX
-- raises). Values too large for double precision (an L2 norm past about
-- 1e154) are refused (22003) and write nothing. Writes vector `out` (f64,
-- unit L2 norm) and returns the iterations run. Constants enter
-- expressions as constant vectors; a constant vector is 0 at tombstoned
-- rows, where vector_map evaluates too, so a division by one is guarded.
-- A pair linked in several relations is one edge, or with `per_relation`
-- an edge in each (DESIGN.md §5.3).
CREATE FUNCTION warren.eigenvector_centrality(graph text, "out" text, tol float8 DEFAULT 1e-6,
                                              max_iter int DEFAULT 100, rels text[] DEFAULT NULL,
                                              weighted boolean DEFAULT NULL, version bigint DEFAULT NULL, per_relation boolean DEFAULT false)
RETURNS int LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['one', 'x', 'y', 'next', 'scale'];
  nodes float8;
  norm float8;
  delta float8;
  i int := 0;
BEGIN
  IF "out" IS NULL OR tol IS NULL OR max_iter IS NULL THEN
    RAISE EXCEPTION '% must not be NULL', CASE WHEN "out" IS NULL THEN 'out' WHEN tol IS NULL THEN 'tol' ELSE 'max_iter' END
      USING ERRCODE = 'null_value_not_allowed';
  END IF;
  IF starts_with("out", '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  IF tol < 0 THEN
    RAISE EXCEPTION 'tol must be at least 0, not %', tol USING ERRCODE = 'invalid_parameter_value';
  END IF;
  IF max_iter < 1 THEN
    RAISE EXCEPTION 'max_iter must be at least 1, not %', max_iter USING ERRCODE = 'invalid_parameter_value';
  END IF;
  BEGIN
    PERFORM warren.vector_const(graph, '_warren_eigenvector_centrality_one', 1, persist := false, version := v);
    nodes := warren.vector_reduce(graph, '_warren_eigenvector_centrality_one', 'sum', v);
    IF nodes = 0 THEN
      RAISE EXCEPTION 'graph "%" has no live nodes', graph USING ERRCODE = 'invalid_parameter_value';
    END IF;
    PERFORM warren.vector_const(graph, '_warren_eigenvector_centrality_x', 1.0 / nodes, persist := false,
                                version := v);
    WHILE i < max_iter LOOP
      i := i + 1;
      PERFORM warren.report_progress('eigenvector_centrality', graph, 'iterating', i - 1, max_iter);
      PERFORM warren.spmv(graph, '_warren_eigenvector_centrality_x', '_warren_eigenvector_centrality_y',
                          transpose := true, rels := rels, weighted := weighted, per_relation := per_relation, persist := false, version := v);
      PERFORM warren.vector_map(graph, '_warren_eigenvector_centrality_next',
        '_warren_eigenvector_centrality_x + _warren_eigenvector_centrality_y', false, v);
      norm := warren.vector_reduce(graph, '_warren_eigenvector_centrality_next', 'l2', v);
      IF NOT norm < 'Infinity' THEN
        RAISE EXCEPTION 'eigenvector_centrality diverged: its values are not finite after % iterations', i
          USING ERRCODE = 'numeric_value_out_of_range',
                HINT = 'The edge weights are too large: their squares overflow double precision.';
      END IF;
      PERFORM warren.vector_const(graph, '_warren_eigenvector_centrality_scale',
                                  CASE WHEN norm = 0 THEN 1 ELSE norm END, persist := false, version := v);
      PERFORM warren.vector_map(graph, '_warren_eigenvector_centrality_next',
        'CASE WHEN _warren_eigenvector_centrality_scale <> 0
              THEN _warren_eigenvector_centrality_next / _warren_eigenvector_centrality_scale ELSE 0 END', false, v);
      delta := warren.vector_norm_diff(graph, '_warren_eigenvector_centrality_next',
                                       '_warren_eigenvector_centrality_x', 'l1', v, v);
      PERFORM warren.vector_map(graph, '_warren_eigenvector_centrality_x', '_warren_eigenvector_centrality_next',
                                false, v);
      EXIT WHEN delta < nodes * tol;
    END LOOP;
    IF NOT delta < nodes * tol THEN
      RAISE WARNING 'eigenvector_centrality did not converge in % iterations', i;
    END IF;
    PERFORM warren.vector_map(graph, "out", '_warren_eigenvector_centrality_x', true, v);
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_eigenvector_centrality_' || part, v, true) FROM unnest(parts) part;
    RAISE;
  END;
  PERFORM warren.drop_vector(graph, '_warren_eigenvector_centrality_' || part, v, true) FROM unnest(parts) part;
  RETURN i;
END $$;
