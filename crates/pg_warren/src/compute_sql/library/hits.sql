-- warren.hits (DESIGN.md §6.7): HITS hubs and authorities by power
-- iteration, as NetworkX computes them (its default, method :=
-- 'power_iteration'). From hubs uniform at 1/N, each iteration takes the
-- authorities a = Aᵀh (a node pointed at by good hubs) and the hubs h = A·a
-- (a node pointing at good authorities), weighted by edge weight (spmv's
-- rule: weights if the graph has them, unless weighted := false), and
-- scales h to a largest value of 1; it stops when the L1 change of h falls
-- below tol (not scaled by N, as NetworkX), or after max_iter iterations
-- with a WARNING (NetworkX raises). With normalized, each result sums to
-- 1; otherwise each has a largest value of 1. A zero sum or maximum (no
-- edges) leaves the zeros as they are. Values too large for double
-- precision are refused (22003) and write nothing. Writes vectors `hubs`
-- and `authorities` (f64) and returns the iterations run. A constant
-- vector is 0 at tombstoned rows, where vector_map evaluates too, so a
-- division by one is guarded.
CREATE FUNCTION warren.hits(graph text, hubs text, authorities text, tol float8 DEFAULT 1e-8,
                            max_iter int DEFAULT 100, normalized boolean DEFAULT true,
                            rels text[] DEFAULT NULL, weighted boolean DEFAULT NULL,
                            version bigint DEFAULT NULL)
RETURNS int LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['one', 'x', 'a', 'next', 'scale'];
  nodes float8;
  scale float8;
  hub_scale float8;
  delta float8;
  i int := 0;
BEGIN
  IF hubs IS NULL OR authorities IS NULL OR normalized IS NULL OR tol IS NULL OR max_iter IS NULL THEN
    RAISE EXCEPTION '% must not be NULL',
      CASE WHEN hubs IS NULL THEN 'hubs' WHEN authorities IS NULL THEN 'authorities'
           WHEN normalized IS NULL THEN 'normalized' WHEN tol IS NULL THEN 'tol' ELSE 'max_iter' END
      USING ERRCODE = 'null_value_not_allowed';
  END IF;
  IF starts_with(hubs, '_warren_') OR starts_with(authorities, '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  IF hubs = authorities THEN
    RAISE EXCEPTION 'hubs and authorities must be different vectors' USING ERRCODE = 'invalid_parameter_value';
  END IF;
  IF tol < 0 THEN
    RAISE EXCEPTION 'tol must be at least 0, not %', tol USING ERRCODE = 'invalid_parameter_value';
  END IF;
  IF max_iter < 1 THEN
    RAISE EXCEPTION 'max_iter must be at least 1, not %', max_iter USING ERRCODE = 'invalid_parameter_value';
  END IF;
  BEGIN
    PERFORM warren.vector_const(graph, '_warren_hits_one', 1, persist := false, version := v);
    nodes := warren.vector_reduce(graph, '_warren_hits_one', 'sum', v);
    IF nodes = 0 THEN
      RAISE EXCEPTION 'graph "%" has no live nodes', graph USING ERRCODE = 'invalid_parameter_value';
    END IF;
    PERFORM warren.vector_const(graph, '_warren_hits_x', 1.0 / nodes, persist := false, version := v);
    WHILE i < max_iter LOOP
      i := i + 1;
      PERFORM warren.spmv(graph, '_warren_hits_x', '_warren_hits_a', transpose := true, rels := rels,
                          weighted := weighted, persist := false, version := v);
      PERFORM warren.spmv(graph, '_warren_hits_a', '_warren_hits_next', rels := rels, weighted := weighted,
                          persist := false, version := v);
      scale := warren.vector_reduce(graph, '_warren_hits_next', 'max', v);
      IF NOT abs(scale) < 'Infinity' THEN
        RAISE EXCEPTION 'hits diverged: its values are not finite after % iterations', i
          USING ERRCODE = 'numeric_value_out_of_range',
                HINT = 'The edge weights are too large: their products overflow double precision.';
      END IF;
      PERFORM warren.vector_const(graph, '_warren_hits_scale', CASE WHEN scale = 0 THEN 1 ELSE scale END,
                                  persist := false, version := v);
      PERFORM warren.vector_map(graph, '_warren_hits_next',
        'CASE WHEN _warren_hits_scale <> 0 THEN _warren_hits_next / _warren_hits_scale ELSE 0 END', false, v);
      delta := warren.vector_norm_diff(graph, '_warren_hits_next', '_warren_hits_x', 'l1', v, v);
      PERFORM warren.vector_map(graph, '_warren_hits_x', '_warren_hits_next', false, v);
      EXIT WHEN delta < tol;
    END LOOP;
    IF NOT delta < tol THEN
      RAISE WARNING 'hits did not converge in % iterations', i;
    END IF;
    IF normalized THEN
      hub_scale := warren.vector_reduce(graph, '_warren_hits_x', 'sum', v);
      scale := warren.vector_reduce(graph, '_warren_hits_a', 'sum', v);
    ELSE
      hub_scale := 1;
      scale := warren.vector_reduce(graph, '_warren_hits_a', 'max', v);
    END IF;
    IF NOT abs(hub_scale) < 'Infinity' OR NOT abs(scale) < 'Infinity' THEN
      RAISE EXCEPTION 'hits diverged: its values are not finite after % iterations', i
        USING ERRCODE = 'numeric_value_out_of_range',
              HINT = 'The edge weights are too large: their products overflow double precision.';
    END IF;
    PERFORM warren.vector_const(graph, '_warren_hits_scale', CASE WHEN hub_scale = 0 THEN 1 ELSE hub_scale END,
                                persist := false, version := v);
    PERFORM warren.vector_map(graph, hubs,
      'CASE WHEN _warren_hits_scale <> 0 THEN _warren_hits_x / _warren_hits_scale ELSE 0 END', true, v);
    PERFORM warren.vector_const(graph, '_warren_hits_scale', CASE WHEN scale = 0 THEN 1 ELSE scale END,
                                persist := false, version := v);
    PERFORM warren.vector_map(graph, authorities,
      'CASE WHEN _warren_hits_scale <> 0 THEN _warren_hits_a / _warren_hits_scale ELSE 0 END', true, v);
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_hits_' || part, v, true) FROM unnest(parts) part;
    RAISE;
  END;
  PERFORM warren.drop_vector(graph, '_warren_hits_' || part, v, true) FROM unnest(parts) part;
  RETURN i;
END $$;
