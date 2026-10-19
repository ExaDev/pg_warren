-- warren.pagerank (DESIGN.md §6.7): PageRank by power iteration, as
-- NetworkX computes it. Rank flows along edges, split by out-weight (the
-- edge count when unweighted); dangling nodes' rank, and the 1 - damping
-- teleport, go to the personalisation: uniform, or proportional to vector
-- `seed`. Stops when the L1 change falls below (live nodes) × tol, or after
-- max_iter iterations. Writes vector `out` (f64, summing to 1) and returns
-- the iterations run.
CREATE FUNCTION warren.pagerank(graph text, "out" text, damping float8 DEFAULT 0.85,
                                tol float8 DEFAULT 1e-6, max_iter int DEFAULT 100,
                                seed text DEFAULT NULL, rels text[] DEFAULT NULL,
                                weighted boolean DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS int LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  nodes float8;
  dangling float8;
  delta float8;
  i int := 0;
BEGIN
  -- Every live node's out-weight: the row sums of A.
  PERFORM warren.vector_const(graph, '_warren_pagerank_one', 1, persist := false, version := v);
  PERFORM warren.spmv(graph, '_warren_pagerank_one', '_warren_pagerank_out', 'plus_first',
                      rels := rels, weighted := weighted, persist := false, version := v);
  nodes := warren.vector_reduce(graph, '_warren_pagerank_one', 'sum', v);
  IF seed IS NULL THEN
    PERFORM warren.vector_map(graph, '_warren_pagerank_p', format('_warren_pagerank_one / %s', nodes), false, v);
  ELSE
    PERFORM warren.vector_map(graph, '_warren_pagerank_p',
                              format('%I / %s', seed, warren.vector_reduce(graph, seed, 'sum', v)), false, v);
  END IF;
  PERFORM warren.vector_map(graph, '_warren_pagerank_x', format('_warren_pagerank_one / %s', nodes), false, v);
  WHILE i < max_iter LOOP
    i := i + 1;
    PERFORM warren.vector_map(graph, '_warren_pagerank_msg',
      'CASE WHEN _warren_pagerank_out > 0 THEN _warren_pagerank_x / _warren_pagerank_out ELSE 0 END', false, v);
    PERFORM warren.vector_map(graph, '_warren_pagerank_dangling',
      'CASE WHEN _warren_pagerank_out > 0 THEN 0 ELSE _warren_pagerank_x END', false, v);
    dangling := warren.vector_reduce(graph, '_warren_pagerank_dangling', 'sum', v);
    PERFORM warren.spmv(graph, '_warren_pagerank_msg', '_warren_pagerank_y', transpose := true,
                        rels := rels, weighted := weighted, persist := false, version := v);
    PERFORM warren.vector_map(graph, '_warren_pagerank_next',
      format('%s * (_warren_pagerank_y + %s * _warren_pagerank_p) + %s * _warren_pagerank_p',
             damping, dangling, 1 - damping), false, v);
    delta := warren.vector_norm_diff(graph, '_warren_pagerank_next', '_warren_pagerank_x', 'l1', v, v);
    PERFORM warren.vector_map(graph, '_warren_pagerank_x', '_warren_pagerank_next', false, v);
    EXIT WHEN delta < nodes * tol;
  END LOOP;
  PERFORM warren.vector_map(graph, "out", '_warren_pagerank_x', true, v);
  PERFORM warren.drop_vector(graph, '_warren_pagerank_' || part, v)
  FROM unnest(ARRAY['one', 'out', 'p', 'x', 'msg', 'dangling', 'y', 'next']) part;
  RETURN i;
END $$;
