-- pg_warren 0.1.0 -> 0.2.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.1.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.1.0 yields the catalog of a fresh
-- 0.2.0 install.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.2.0'" to load this file. \quit

-- Vectors record the role that wrote them (§7). Vectors written before
-- 0.2.0 (only possible by direct catalog writes) are the graph owner's.
ALTER TABLE warren.vectors ADD COLUMN created_by regrole;
UPDATE warren.vectors v SET created_by = g.owner
FROM warren.graphs g WHERE g.graph_id = v.graph_id;
ALTER TABLE warren.vectors ALTER COLUMN created_by SET NOT NULL;

-- IEEE floats rarely compress, so skip pglz's attempt on every write.
ALTER TABLE warren.vector_chunks ALTER COLUMN data SET STORAGE EXTERNAL;

-- The vector and compute functions (M4, DESIGN.md §6.2, §6.3), as pgrx
-- generates them for 0.2.0.

CREATE  FUNCTION warren."drop_vector"(
	"graph" TEXT, /* Option < & str > */
	"name" TEXT, /* Option < & str > */
	"version" bigint DEFAULT NULL, /* Option < i64 > */
	"if_exists" bool DEFAULT false /* Option < bool > */
) RETURNS void
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'drop_vector_wrapper';

CREATE  FUNCTION warren."explain_map"(
	"graph" TEXT, /* Option < & str > */
	"expr" TEXT, /* Option < & str > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS jsonb /* JsonB */
STABLE PARALLEL SAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'explain_map_wrapper';

CREATE  FUNCTION warren."spmm"(
	"graph" TEXT, /* Option < & str > */
	"x" TEXT, /* Option < & str > */
	"y" TEXT, /* Option < & str > */
	"semiring" TEXT DEFAULT 'plus_times', /* Option < & str > */
	"transpose" bool DEFAULT false, /* Option < bool > */
	"rels" TEXT[] DEFAULT NULL, /* :: std :: option :: Option < Vec < Option < String > > > */
	"weighted" bool DEFAULT NULL, /* Option < bool > */
	"mask" TEXT DEFAULT NULL, /* Option < & str > */
	"mask_complement" bool DEFAULT false, /* Option < bool > */
	"replace" bool DEFAULT false, /* Option < bool > */
	"accumulate" TEXT DEFAULT NULL, /* Option < & str > */
	"persist" bool DEFAULT true, /* Option < bool > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS void
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'spmm_wrapper';

CREATE  FUNCTION warren."spmv"(
	"graph" TEXT, /* Option < & str > */
	"x" TEXT, /* Option < & str > */
	"y" TEXT, /* Option < & str > */
	"semiring" TEXT DEFAULT 'plus_times', /* Option < & str > */
	"transpose" bool DEFAULT false, /* Option < bool > */
	"rels" TEXT[] DEFAULT NULL, /* :: std :: option :: Option < Vec < Option < String > > > */
	"weighted" bool DEFAULT NULL, /* Option < bool > */
	"mask" TEXT DEFAULT NULL, /* Option < & str > */
	"mask_complement" bool DEFAULT false, /* Option < bool > */
	"replace" bool DEFAULT false, /* Option < bool > */
	"accumulate" TEXT DEFAULT NULL, /* Option < & str > */
	"persist" bool DEFAULT true, /* Option < bool > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS void
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'spmv_wrapper';

CREATE  FUNCTION warren."vector_const"(
	"graph" TEXT, /* Option < & str > */
	"name" TEXT, /* Option < & str > */
	"value" double precision, /* Option < f64 > */
	"dtype" TEXT DEFAULT 'f64', /* Option < & str > */
	"persist" bool DEFAULT true, /* Option < bool > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS bigint /* Option < i64 > */
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_const_wrapper';

CREATE  FUNCTION warren."vector_degree"(
	"graph" TEXT, /* Option < & str > */
	"name" TEXT, /* Option < & str > */
	"direction" TEXT DEFAULT 'out', /* Option < & str > */
	"rels" TEXT[] DEFAULT NULL, /* :: std :: option :: Option < Vec < Option < String > > > */
	"dtype" TEXT DEFAULT 'f64', /* Option < & str > */
	"persist" bool DEFAULT true, /* Option < bool > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS bigint /* Option < i64 > */
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_degree_wrapper';

CREATE  FUNCTION warren."vector_dense_id"(
	"graph" TEXT, /* Option < & str > */
	"name" TEXT, /* Option < & str > */
	"dtype" TEXT DEFAULT 'i64', /* Option < & str > */
	"persist" bool DEFAULT true, /* Option < bool > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS bigint /* Option < i64 > */
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_dense_id_wrapper';

CREATE  FUNCTION warren."vector_from_query"(
	"graph" TEXT, /* Option < & str > */
	"name" TEXT, /* Option < & str > */
	"query" TEXT, /* Option < & str > */
	"dtype" TEXT DEFAULT 'f64', /* Option < & str > */
	"fill" double precision DEFAULT 0, /* Option < f64 > */
	"persist" bool DEFAULT true, /* Option < bool > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS bigint /* Option < i64 > */
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_from_query_wrapper';

CREATE  FUNCTION warren."vector_map"(
	"graph" TEXT, /* Option < & str > */
	"y" TEXT, /* Option < & str > */
	"expr" TEXT, /* Option < & str > */
	"persist" bool DEFAULT true, /* Option < bool > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS void
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_map_wrapper';

CREATE  FUNCTION warren."vector_norm_diff"(
	"graph" TEXT, /* Option < & str > */
	"a" TEXT, /* Option < & str > */
	"b" TEXT, /* Option < & str > */
	"norm" TEXT DEFAULT 'l2', /* Option < & str > */
	"a_version" bigint DEFAULT NULL, /* Option < i64 > */
	"b_version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS double precision /* f64 */
STABLE PARALLEL SAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_norm_diff_wrapper';

CREATE  FUNCTION warren."vector_reduce"(
	"graph" TEXT, /* Option < & str > */
	"x" TEXT, /* Option < & str > */
	"op" TEXT, /* Option < & str > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS double precision /* Option < f64 > */
STABLE PARALLEL SAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_reduce_wrapper';

CREATE  FUNCTION warren."vector_stats"(
	"graph" TEXT, /* Option < & str > */
	"name" TEXT, /* Option < & str > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS jsonb /* JsonB */
STABLE PARALLEL SAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_stats_wrapper';

CREATE FUNCTION warren.id_map(graph text, id_type anyelement, version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, dense_id bigint, live boolean)
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'id_map_wrapper';

CREATE FUNCTION warren.matrix_rows(graph text, name text, id_type anyelement,
                                   version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, "row" float8[])
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'matrix_rows_wrapper';

CREATE FUNCTION warren.vector_topk(graph text, name text, k int, id_type anyelement,
                                   descending bool DEFAULT true, version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, value float8)
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'vector_topk_wrapper';

CREATE FUNCTION warren.vector_values(graph text, name text, id_type anyelement,
                                     version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, value float8)
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'vector_values_wrapper';

-- gather (M5, DESIGN.md §6.4).

CREATE FUNCTION warren.gather(graph text, messages text, id_type anyelement,
                              value_type anycompatible, combine text DEFAULT 'collect',
                              direction text DEFAULT 'in', rels text[] DEFAULT NULL,
                              mask text DEFAULT NULL, mask_complement boolean DEFAULT false,
                              version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, value anycompatible)
VOLATILE PARALLEL UNSAFE LANGUAGE c AS 'MODULE_PATHNAME', 'gather_wrapper';

-- The algorithm library (M5, DESIGN.md §6.7).

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
      PERFORM warren.vector_const(graph, '_warren_pagerank_total', warren.vector_reduce(graph, seed, 'sum', v),
                                  persist := false, version := v);
      PERFORM warren.vector_map(graph, '_warren_pagerank_p', format('%I / _warren_pagerank_total', seed), false, v);
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

-- warren.degree_vector (DESIGN.md §6.7): each node's degree in `direction`
-- over `rels`, as vector `out` (f64); returns its vector id.
CREATE FUNCTION warren.degree_vector(graph text, "out" text, direction text DEFAULT 'out',
                                     rels text[] DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS bigint LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
  IF starts_with("out", '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  RETURN warren.vector_degree(graph, "out", direction, rels, 'f64', true, version);
END $$;

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

-- warren.sssp_vector (DESIGN.md §6.7): single-source shortest-path
-- distances by Bellman-Ford relaxation (min_plus SpMV along edges,
-- accumulating min) until no distance falls, or after max_iter iterations.
-- By default the bound is the live node count, and distances still falling
-- then mean a negative cycle reachable from the source, an error (22023).
-- Weights as spmv reads them (1 if unweighted); unreachable nodes are
-- Infinity. Writes vector `out` (f64) and returns the iterations run.
CREATE FUNCTION warren.sssp_vector(graph text, "out" text, source anyelement,
                                   rels text[] DEFAULT NULL, weighted boolean DEFAULT NULL,
                                   max_iter int DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS int LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['x', 'next', 'fell'];
  limit_ int;
  falling float8;
  i int := 0;
BEGIN
  IF starts_with("out", '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  BEGIN
    PERFORM warren.vector_from_query(graph, '_warren_sssp_vector_x',
                                     format('SELECT %L::%s, 0', source, pg_typeof(source)),
                                     fill := 'Infinity', persist := false, version := v);
    IF warren.vector_reduce(graph, '_warren_sssp_vector_x', 'min', v) <> 0 THEN
      RAISE EXCEPTION 'node % is not a node of graph "%"', source, graph USING ERRCODE = 'invalid_parameter_value';
    END IF;
    limit_ := coalesce(max_iter, (warren.vector_stats(graph, '_warren_sssp_vector_x', v) ->> 'count')::int);
    WHILE i < limit_ LOOP
      i := i + 1;
      PERFORM warren.vector_map(graph, '_warren_sssp_vector_next', '_warren_sssp_vector_x', false, v);
      PERFORM warren.spmv(graph, '_warren_sssp_vector_x', '_warren_sssp_vector_next', 'min_plus',
                          transpose := true, rels := rels, weighted := weighted, accumulate := 'min',
                          persist := false, version := v);
      PERFORM warren.vector_map(graph, '_warren_sssp_vector_fell',
        'CASE WHEN _warren_sssp_vector_next < _warren_sssp_vector_x THEN 1 ELSE 0 END', false, v);
      falling := warren.vector_reduce(graph, '_warren_sssp_vector_fell', 'sum', v);
      PERFORM warren.vector_map(graph, '_warren_sssp_vector_x', '_warren_sssp_vector_next', false, v);
      EXIT WHEN falling = 0;
    END LOOP;
    IF max_iter IS NULL AND falling > 0 THEN
      RAISE EXCEPTION 'a negative cycle is reachable from node % of graph "%"', source, graph
        USING ERRCODE = 'invalid_parameter_value';
    END IF;
    PERFORM warren.vector_map(graph, "out", '_warren_sssp_vector_x', true, v);
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_sssp_vector_' || part, v, true) FROM unnest(parts) part;
    RAISE;
  END;
  PERFORM warren.drop_vector(graph, '_warren_sssp_vector_' || part, v, true) FROM unnest(parts) part;
  RETURN i;
END $$;

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
      PERFORM warren.vector_degree(graph, '_warren_aggregate_neighbours_degree', direction, rels, dtype, false, v);
    END IF;
    FOR hop IN 1..hops LOOP
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
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_aggregate_neighbours_' || part, v, true) FROM unnest(parts) part;
    RAISE;
  END;
  PERFORM warren.drop_vector(graph, '_warren_aggregate_neighbours_' || part, v, true) FROM unnest(parts) part;
END $$;

-- triangles (M5, DESIGN.md §6.7).

CREATE  FUNCTION warren."triangles"(
	"graph" TEXT, /* Option < & str > */
	"out" TEXT, /* Option < & str > */
	"rels" TEXT[] DEFAULT NULL, /* :: std :: option :: Option < Vec < Option < String > > > */
	"persist" bool DEFAULT true, /* Option < bool > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS bigint /* Option < i64 > */
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'triangles_wrapper';
