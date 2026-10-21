-- warren.pagerank (DESIGN.md §6.7): PageRank by power iteration, as
-- NetworkX computes it. Rank flows along edges, split by out-weight (the
-- edge count when unweighted); dangling nodes' rank, and the 1 - damping
-- teleport, go to the personalisation: uniform, or proportional to vector
-- `seed`. Stops when the L1 change falls below (live nodes) × tol, or after
-- max_iter iterations. Writes vector `out` (f64, summing to 1) and returns
-- the iterations run. Constants enter expressions as constant vectors,
-- never as text, so no client setting (extra_float_digits) rounds them.
CREATE FUNCTION warren.pagerank(graph text, "out" text, damping float8 DEFAULT 0.85,
                                tol float8 DEFAULT 1e-6, max_iter int DEFAULT 100,
                                seed text DEFAULT NULL, rels text[] DEFAULT NULL,
                                weighted boolean DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS int LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['one', 'out', 'x', 'p', 'total', 'damping', 'teleport', 'msg', 'dangling_x',
                        'dangling', 'y', 'next'];
  nodes float8;
  dangling float8;
  delta float8;
  i int := 0;
BEGIN
  IF starts_with("out", '_warren_') OR starts_with(seed, '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  BEGIN
    -- Every live node's out-weight: the row sums of A.
    PERFORM warren.vector_const(graph, '_warren_pagerank_one', 1, persist := false, version := v);
    PERFORM warren.spmv(graph, '_warren_pagerank_one', '_warren_pagerank_out', 'plus_first',
                        rels := rels, weighted := weighted, persist := false, version := v);
    nodes := warren.vector_reduce(graph, '_warren_pagerank_one', 'sum', v);
    PERFORM warren.vector_const(graph, '_warren_pagerank_x', 1.0 / nodes, persist := false, version := v);
    IF seed IS NULL THEN
      PERFORM warren.vector_map(graph, '_warren_pagerank_p', '_warren_pagerank_x', false, v);
    ELSE
      -- Times the reciprocal: a constant vector is 0 at tombstoned rows,
      -- where vector_map evaluates too, so dividing by one would fail.
      PERFORM warren.vector_const(graph, '_warren_pagerank_total', 1.0 / warren.vector_reduce(graph, seed, 'sum', v),
                                  persist := false, version := v);
      PERFORM warren.vector_map(graph, '_warren_pagerank_p', format('%I * _warren_pagerank_total', seed), false, v);
    END IF;
    PERFORM warren.vector_const(graph, '_warren_pagerank_damping', damping, persist := false, version := v);
    PERFORM warren.vector_const(graph, '_warren_pagerank_teleport', 1 - damping, persist := false, version := v);
    WHILE i < max_iter LOOP
      i := i + 1;
      PERFORM warren.vector_map(graph, '_warren_pagerank_msg',
        'CASE WHEN _warren_pagerank_out > 0 THEN _warren_pagerank_x / _warren_pagerank_out ELSE 0 END', false, v);
      PERFORM warren.vector_map(graph, '_warren_pagerank_dangling_x',
        'CASE WHEN _warren_pagerank_out > 0 THEN 0 ELSE _warren_pagerank_x END', false, v);
      dangling := warren.vector_reduce(graph, '_warren_pagerank_dangling_x', 'sum', v);
      PERFORM warren.vector_const(graph, '_warren_pagerank_dangling', dangling, persist := false, version := v);
      PERFORM warren.spmv(graph, '_warren_pagerank_msg', '_warren_pagerank_y', transpose := true,
                          rels := rels, weighted := weighted, persist := false, version := v);
      PERFORM warren.vector_map(graph, '_warren_pagerank_next',
        '_warren_pagerank_damping * (_warren_pagerank_y + _warren_pagerank_dangling * _warren_pagerank_p)
         + _warren_pagerank_teleport * _warren_pagerank_p', false, v);
      delta := warren.vector_norm_diff(graph, '_warren_pagerank_next', '_warren_pagerank_x', 'l1', v, v);
      PERFORM warren.vector_map(graph, '_warren_pagerank_x', '_warren_pagerank_next', false, v);
      EXIT WHEN delta < nodes * tol;
    END LOOP;
    PERFORM warren.vector_map(graph, "out", '_warren_pagerank_x', true, v);
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_pagerank_' || part, v, true) FROM unnest(parts) part;
    RAISE;
  END;
  PERFORM warren.drop_vector(graph, '_warren_pagerank_' || part, v, true) FROM unnest(parts) part;
  RETURN i;
END $$;
