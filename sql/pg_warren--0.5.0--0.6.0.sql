-- pg_warren 0.5.0 -> 0.6.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.5.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.5.0 yields the catalog of a fresh
-- 0.6.0 install. In a trusted install it runs as the bootstrap superuser
-- (DESIGN.md §7): create only in schema warren, schema-qualify, and use only
-- exact-match pg_catalog operators and functions in what runs here.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.6.0'" to load this file. \quit

-- warren.set_owner(name, owner) (DESIGN.md §5.7, §14 item 43): gives a graph
-- a new owner, as ALTER MATERIALIZED VIEW ... OWNER TO does.
CREATE FUNCTION warren."set_owner"(name text, owner regrole) RETURNS void
STRICT VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'set_owner_wrapper';

-- Cleanup after a dropped role (DESIGN.md §5.7, §14 items 44 and 45). The
-- event trigger on DROP OWNED that calls warren.on_drop_owned() is a
-- superuser's opt-in: no extension script creates it.
CREATE FUNCTION warren."reassign_owned"("old" regrole, "new" regrole) RETURNS bigint
STRICT VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'reassign_owned_wrapper';

CREATE FUNCTION warren."drop_owned"(role regrole) RETURNS bigint
STRICT VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'drop_owned_wrapper';

CREATE FUNCTION warren."on_drop_owned"() RETURNS event_trigger
VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'on_drop_owned_wrapper';

CREATE FUNCTION warren."orphans"() RETURNS TABLE (
	"kind" TEXT,
	"graph" TEXT,
	"role" oid
)
STRICT VOLATILE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'orphans_wrapper';

CREATE FUNCTION warren."drop_orphans"() RETURNS bigint
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'drop_orphans_wrapper';

-- Observability (DESIGN.md §5.7, §14 items 48 and 49): this backend's
-- private cache, and every backend's from the activity table in shared
-- memory. warren.shared_cache() keeps its shape.
CREATE FUNCTION warren."cache"() RETURNS TABLE (
	"kind" TEXT,
	"graph" TEXT,
	"version" bigint,
	"vector" TEXT,
	"vector_id" bigint,
	"bytes" bigint,
	"state" TEXT,
	"last_use" bigint,
	"cap" bigint
)
STRICT VOLATILE PARALLEL RESTRICTED
LANGUAGE c
AS 'MODULE_PATHNAME', 'cache_wrapper';

CREATE FUNCTION warren."backends"() RETURNS TABLE (
	"kind" text,
	"pid" int,
	"database" oid,
	"role" regrole,
	"private_bytes" bigint,
	"transient_bytes" bigint,
	"mapped_bytes" bigint,
	"lookups" bigint,
	"private_hits" bigint,
	"shared_hits" bigint,
	"waits" bigint,
	"private_loads" bigint,
	"shared_loads" bigint,
	"loaded_bytes" bigint,
	"load_seconds" float8,
	"private_evictions" bigint,
	"shared_evictions" bigint,
	"refusals" jsonb,
	"since" timestamptz
)
VOLATILE PARALLEL SAFE
LANGUAGE c AS 'MODULE_PATHNAME', 'backends_wrapper';

-- Progress (DESIGN.md §5.7, §14 item 49): every backend's command and the
-- operation inside it, from the activity table, and the report a PL/pgSQL
-- algorithm makes of its iterations.
CREATE FUNCTION warren."progress"() RETURNS TABLE (
	"pid" int,
	"database" oid,
	"role" regrole,
	"command" text,
	"graph" text,
	"version" bigint,
	"phase" text,
	"done" bigint,
	"total" bigint,
	"operation" text,
	"operation_phase" text,
	"operation_done" bigint,
	"operation_total" bigint,
	"detail" jsonb,
	"command_start" timestamptz,
	"operation_start" timestamptz
)
VOLATILE PARALLEL SAFE
LANGUAGE c AS 'MODULE_PATHNAME', 'progress_wrapper';

CREATE FUNCTION warren."report_progress"(
	"command" TEXT,
	"graph" TEXT,
	"phase" TEXT,
	"done" bigint DEFAULT NULL,
	"total" bigint DEFAULT NULL
) RETURNS void
VOLATILE PARALLEL RESTRICTED
LANGUAGE c
AS 'MODULE_PATHNAME', 'report_progress_wrapper';

-- The library's iterative functions report their iterations and levels
-- through warren.report_progress; their results are unchanged.

-- warren.aggregate_neighbours (DESIGN.md §6.7, A.5): each node's row of
-- matrix `x` replaced by the mean, sum or max of its neighbours' rows
-- (out-neighbours, or in-neighbours with direction := 'in'; distinct
-- neighbours over `rels`; weights are not read), `hops` times over. A node
-- with no neighbours gets NaN for mean (SQL's AVG of nothing), 0 for sum
-- and -Infinity (the least bigint) for max. mean needs an f32 or f64
-- matrix; the result keeps x's dtype. Writes matrix `out`.
CREATE OR REPLACE FUNCTION warren.aggregate_neighbours(graph text, x text, "out" text, agg text DEFAULT 'mean',
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
      PERFORM warren.report_progress('aggregate_neighbours', graph, 'hop', hop - 1, hops);
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

-- warren.components_vector (DESIGN.md §6.7): weakly connected components by
-- min-label propagation (min_second SpMV both ways, accumulating min) until
-- no label changes. Each component is labelled by its smallest dense id,
-- as warren.components labels it. Writes vector `out` (i64) and returns
-- the iterations run.
CREATE OR REPLACE FUNCTION warren.components_vector(graph text, "out" text, rels text[] DEFAULT NULL,
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
      PERFORM warren.report_progress('components_vector', graph, 'iterating', i - 1, NULL);
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
CREATE OR REPLACE FUNCTION warren.eigenvector_centrality(graph text, "out" text, tol float8 DEFAULT 1e-6,
                                              max_iter int DEFAULT 100, rels text[] DEFAULT NULL,
                                              weighted boolean DEFAULT NULL, version bigint DEFAULT NULL)
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
                          transpose := true, rels := rels, weighted := weighted, persist := false, version := v);
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
CREATE OR REPLACE FUNCTION warren.hits(graph text, hubs text, authorities text, tol float8 DEFAULT 1e-8,
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
      PERFORM warren.report_progress('hits', graph, 'iterating', i - 1, max_iter);
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

-- warren.katz_centrality (DESIGN.md §6.7): Katz centrality by power
-- iteration, as NetworkX computes it. Centrality flows along edges,
-- weighted by edge weight (spmv's rule: weights if the graph has them,
-- unless weighted := false), attenuated by alpha, plus each node's beta:
-- the constant `beta`, or beta × vector `seed` (NetworkX's per-node beta).
-- From 0, each iteration takes alpha · Aᵀx + β; it stops when the L1 change
-- falls below (live nodes) × tol, or after max_iter iterations with a
-- WARNING (NetworkX raises). Alpha at or above 1 / the largest eigenvalue
-- diverges: once a value or the L2 norm is not finite the run is refused
-- (22003), as NetworkX raises, and writes nothing. With normalized, the
-- result is scaled to unit L2 norm. Writes vector `out` (f64) and returns
-- the iterations run. Constants enter expressions as constant vectors; a
-- constant vector is 0 at tombstoned rows, where vector_map evaluates too,
-- so a division by one is guarded.
CREATE OR REPLACE FUNCTION warren.katz_centrality(graph text, "out" text, alpha float8 DEFAULT 0.1,
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
  IF "out" IS NULL OR alpha IS NULL OR beta IS NULL OR normalized IS NULL OR tol IS NULL OR max_iter IS NULL THEN
    RAISE EXCEPTION '% must not be NULL',
      CASE WHEN "out" IS NULL THEN 'out' WHEN alpha IS NULL THEN 'alpha' WHEN beta IS NULL THEN 'beta'
           WHEN normalized IS NULL THEN 'normalized' WHEN tol IS NULL THEN 'tol' ELSE 'max_iter' END
      USING ERRCODE = 'null_value_not_allowed';
  END IF;
  IF starts_with("out", '_warren_') OR starts_with(seed, '_warren_') THEN
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
    PERFORM warren.vector_const(graph, '_warren_katz_centrality_one', 1, persist := false, version := v);
    nodes := warren.vector_reduce(graph, '_warren_katz_centrality_one', 'sum', v);
    IF nodes = 0 THEN
      RAISE EXCEPTION 'graph "%" has no live nodes', graph USING ERRCODE = 'invalid_parameter_value';
    END IF;
    IF seed IS NULL THEN
      PERFORM warren.vector_const(graph, '_warren_katz_centrality_b', beta, persist := false, version := v);
    ELSE
      -- vector_reduce names a missing seed vector (42704); vector_map would
      -- call it a missing column.
      PERFORM warren.vector_reduce(graph, seed, 'nnz', v);
      PERFORM warren.vector_map(graph, '_warren_katz_centrality_b', format('%I::float8', seed), false, v);
      PERFORM warren.vector_const(graph, '_warren_katz_centrality_scale', beta, persist := false, version := v);
      PERFORM warren.vector_map(graph, '_warren_katz_centrality_b',
                                '_warren_katz_centrality_scale * _warren_katz_centrality_b', false, v);
    END IF;
    PERFORM warren.vector_const(graph, '_warren_katz_centrality_alpha', alpha, persist := false, version := v);
    PERFORM warren.vector_const(graph, '_warren_katz_centrality_x', 0, persist := false, version := v);
    WHILE i < max_iter LOOP
      i := i + 1;
      PERFORM warren.report_progress('katz_centrality', graph, 'iterating', i - 1, max_iter);
      PERFORM warren.spmv(graph, '_warren_katz_centrality_x', '_warren_katz_centrality_y',
                          transpose := true, rels := rels, weighted := weighted, persist := false, version := v);
      PERFORM warren.vector_map(graph, '_warren_katz_centrality_next',
        '_warren_katz_centrality_alpha * _warren_katz_centrality_y + _warren_katz_centrality_b', false, v);
      delta := warren.vector_norm_diff(graph, '_warren_katz_centrality_next', '_warren_katz_centrality_x',
                                       'l1', v, v);
      IF NOT delta < 'Infinity' THEN
        RAISE EXCEPTION 'katz_centrality diverged: its values are not finite after % iterations', i
          USING ERRCODE = 'numeric_value_out_of_range',
                HINT = 'alpha must be below 1 / the largest eigenvalue of the adjacency matrix.';
      END IF;
      PERFORM warren.vector_map(graph, '_warren_katz_centrality_x', '_warren_katz_centrality_next', false, v);
      EXIT WHEN delta < nodes * tol;
    END LOOP;
    IF NOT delta < nodes * tol THEN
      RAISE WARNING 'katz_centrality did not converge in % iterations', i;
    END IF;
    IF normalized THEN
      norm := warren.vector_reduce(graph, '_warren_katz_centrality_x', 'l2', v);
      IF NOT norm < 'Infinity' THEN
        RAISE EXCEPTION 'katz_centrality diverged: its L2 norm overflows after % iterations', i
          USING ERRCODE = 'numeric_value_out_of_range',
                HINT = 'alpha must be below 1 / the largest eigenvalue of the adjacency matrix.';
      END IF;
      PERFORM warren.vector_const(graph, '_warren_katz_centrality_scale', CASE WHEN norm = 0 THEN 1 ELSE norm END,
                                  persist := false, version := v);
      PERFORM warren.vector_map(graph, "out",
        'CASE WHEN _warren_katz_centrality_scale <> 0
              THEN _warren_katz_centrality_x / _warren_katz_centrality_scale ELSE 0 END', true, v);
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

-- warren.label_propagation (DESIGN.md §6.7, A.1): min-label propagation.
-- Labels start as vector `seed` (default: each node's dense id); each
-- iteration every node takes the smallest label among itself and its
-- neighbours (both directions, over `rels`), until no label changes or
-- after max_iter iterations. With `mask`, only the nodes it selects (a
-- non-zero value, GraphBLAS semantics) send or take labels; the others
-- keep theirs. It is deterministic min-propagation, not majority-vote
-- label propagation. Writes vector `out` (f64) and returns the iterations
-- run.
CREATE OR REPLACE FUNCTION warren.label_propagation(graph text, "out" text, seed text DEFAULT NULL,
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
      PERFORM warren.report_progress('label_propagation', graph, 'iterating', i - 1, max_iter);
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
CREATE OR REPLACE FUNCTION warren.label_propagation_majority(graph text, "out" text, seed text DEFAULT NULL,
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

-- warren.pagerank (DESIGN.md §6.7): PageRank by power iteration, as
-- NetworkX computes it. Rank flows along edges, split by out-weight (the
-- edge count when unweighted); dangling nodes' rank, and the 1 - damping
-- teleport, go to the personalisation: uniform, or proportional to vector
-- `seed`. Stops when the L1 change falls below (live nodes) × tol, or after
-- max_iter iterations. Writes vector `out` (f64, summing to 1) and returns
-- the iterations run. Constants enter expressions as constant vectors,
-- never as text, so no client setting (extra_float_digits) rounds them.
CREATE OR REPLACE FUNCTION warren.pagerank(graph text, "out" text, damping float8 DEFAULT 0.85,
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
      PERFORM warren.report_progress('pagerank', graph, 'iterating', i - 1, max_iter);
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

-- warren.sssp_vector (DESIGN.md §6.7): single-source shortest-path
-- distances by Bellman-Ford relaxation (min_plus SpMV along edges,
-- accumulating min) until no distance falls, or after max_iter iterations.
-- By default the bound is the live node count, and distances still falling
-- then mean a negative cycle reachable from the source, an error (22023).
-- Weights as spmv reads them (1 if unweighted); unreachable nodes are
-- Infinity. Writes vector `out` (f64) and returns the iterations run.
CREATE OR REPLACE FUNCTION warren.sssp_vector(graph text, "out" text, source anyelement,
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
      PERFORM warren.report_progress('sssp_vector', graph, 'iterating', i - 1, limit_);
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

-- warren.leiden (DESIGN.md §6.7): Leiden community detection (Traag,
-- Waltman and van Eck 2019) as a library over the primitives. Each level
-- moves nodes between communities (warren.local_moving, from the partition
-- the level below found) on the level's graph, refines each community
-- into well-connected parts (warren.leiden_refine), lifts both partitions
-- to `graph` (warren.lift), and contracts `graph` by the refined one
-- (warren.contract) into the next level's graph; the next level starts
-- from the unrefined partition, projected onto it (warren.project, min).
-- As warren.louvain's, every level's graph is a quotient of `graph`, and
-- only one exists at a time. Every community found is connected. As
-- Traag et al.'s, it goes on while there is something to aggregate: it
-- stops when a level's local moving leaves every node alone, when the
-- refinement leaves every node alone (the next level's graph would be this
-- one), or after `max_levels`; in the last two cases it returns the
-- refined partition, so every exit returns a connected one. A level's
-- local moving stops when a sweep raises modularity by no more than
-- `tolerance`. That multilevel pass is one iteration; each later one
-- starts its first level's moving from the partition the one before found,
-- not from each node alone, as igraph's `n_iterations` and leidenalg's
-- `optimise_partition` loop do. The run stops after `iterations`, or
-- after an iteration whose partition is the one it started from: with the
-- same start and seed, the next would find it again. A negative
-- `iterations` runs until then, at most 100 iterations (a WARNING if it
-- is still changing). Arguments and output as warren.louvain's;
-- `randomness` is θ of the refinement. `levels_prefix` keeps every
-- iteration's levels in turn (`<prefix>_1`, … numbered on across
-- iterations), so the last is `out`. The level's graph, a level graph of
-- warren.contract's (`_warren_level_<version>`), is dropped at the end of
-- each iteration.
CREATE OR REPLACE FUNCTION warren.leiden(graph text, "out" text, resolution float8 DEFAULT 1.0,
                              seed bigint DEFAULT 0, max_levels int DEFAULT 100,
                              tolerance float8 DEFAULT 1e-7, randomness float8 DEFAULT 0.01,
                              iterations int DEFAULT 2, weighted boolean DEFAULT NULL, levels_prefix text DEFAULT NULL,
                              version bigint DEFAULT NULL)
RETURNS float8 LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['moved', 'refined', 'initial', 'keys', 'down', 'refined_down', 'prev', 'changed'];
  g text := graph;
  gv bigint := v;
  next_g text;
  next_v bigint;
  level int;
  iteration int := 0;
  written int := 0;
  nodes bigint;
  found bigint;
  refined bigint;
  q float8;
BEGIN
  IF "out" IS NULL OR resolution IS NULL OR seed IS NULL OR max_levels IS NULL OR tolerance IS NULL
     OR randomness IS NULL OR iterations IS NULL THEN
    RAISE EXCEPTION '% must not be NULL',
      CASE WHEN "out" IS NULL THEN 'out' WHEN resolution IS NULL THEN 'resolution'
           WHEN seed IS NULL THEN 'seed' WHEN max_levels IS NULL THEN 'max_levels'
           WHEN tolerance IS NULL THEN 'tolerance' WHEN randomness IS NULL THEN 'randomness'
           ELSE 'iterations' END
      USING ERRCODE = 'null_value_not_allowed';
  END IF;
  IF starts_with("out", '_warren_') OR starts_with(levels_prefix, '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  IF max_levels < 1 THEN
    RAISE EXCEPTION 'max_levels must be at least 1, not %', max_levels USING ERRCODE = 'invalid_parameter_value';
  END IF;
  IF iterations = 0 THEN
    RAISE EXCEPTION 'iterations must be at least 1, or negative to run until the partition is stable, not 0'
      USING ERRCODE = 'invalid_parameter_value';
  END IF;
  IF tolerance = 'NaN'::float8 OR tolerance < 0 THEN
    RAISE EXCEPTION 'tolerance must be a number of at least 0, not %', tolerance
      USING ERRCODE = 'invalid_parameter_value';
  END IF;
  BEGIN
    LOOP
      iteration := iteration + 1;
      -- A later iteration's first level moves from the last partition.
      IF iteration > 1 THEN
        PERFORM warren.vector_map(graph, '_warren_leiden_prev', '_warren_leiden_down', false, v);
      END IF;
      level := 0;
      refined := NULL;
      LOOP
        level := level + 1;
        PERFORM warren.report_progress('leiden', graph, format('iteration %s, level %s', iteration, level),
                                       level - 1, max_levels);
        nodes := (SELECT (e ->> 'node_count')::bigint FROM jsonb_array_elements(warren.info(g) -> 'versions') e
                  WHERE (e ->> 'version')::bigint = gv);
        IF level > 1 THEN
          PERFORM warren.vector_from_query(g, '_warren_leiden_keys',
            format('SELECT node_id, node_id FROM warren.id_map(%L, NULL::bigint, %s)', g, gv), 'i64',
            persist := false, version := gv);
        END IF;
        found := warren.local_moving(g, '_warren_leiden_moved',
                                     initial := CASE WHEN level > 1 THEN '_warren_leiden_initial'
                                                     WHEN iteration > 1 THEN '_warren_leiden_prev' END,
                                     keys := CASE WHEN level > 1 THEN '_warren_leiden_keys' END,
                                     resolution := resolution, seed := seed, tolerance := tolerance,
                                     weighted := CASE WHEN level = 1 THEN weighted ELSE true END,
                                     persist := false, version := gv);
        -- This level's partition, on `graph`.
        IF level = 1 THEN
          PERFORM warren.vector_map(graph, '_warren_leiden_down', '_warren_leiden_moved', false, v);
        ELSE
          PERFORM warren.lift(g, '_warren_leiden_moved', graph, '_warren_leiden_down',
                              persist := false, version := gv);
        END IF;
        -- Every node alone: the partition is this level's nodes, the refined
        -- partition of the level below, which is connected.
        IF found < nodes THEN
          refined := warren.leiden_refine(g, '_warren_leiden_moved', '_warren_leiden_refined',
                                          keys := CASE WHEN level > 1 THEN '_warren_leiden_keys' END,
                                          resolution := resolution, seed := seed, randomness := randomness,
                                          weighted := CASE WHEN level = 1 THEN weighted ELSE true END,
                                          persist := false, version := gv);
          IF level = 1 THEN
            PERFORM warren.vector_map(graph, '_warren_leiden_refined_down', '_warren_leiden_refined', false, v);
          ELSE
            PERFORM warren.lift(g, '_warren_leiden_refined', graph, '_warren_leiden_refined_down',
                                persist := false, version := gv);
          END IF;
          -- Stopping here, return the refined partition, which is connected,
          -- as Traag et al.'s is; the moved one need not be.
          IF level >= max_levels OR refined = nodes THEN
            PERFORM warren.vector_map(graph, '_warren_leiden_down', '_warren_leiden_refined_down', false, v);
          END IF;
        END IF;
        IF levels_prefix IS NOT NULL THEN
          written := written + 1;
          PERFORM warren.vector_map(graph, format('%s_%s', levels_prefix, written), '_warren_leiden_down', true, v);
        END IF;
        EXIT WHEN found = nodes OR level >= max_levels OR refined = nodes;
        -- A level graph of its own, named after its version, a name no
        -- graph can have had.
        next_v := warren.contract(graph, '_warren_leiden_refined_down', NULL,
                                  weighted := coalesce(weighted, true), version := v);
        next_g := '_warren_level_' || next_v;
        -- The next level starts from the unrefined partition: every member
        -- of a refined community is in the same community, so min is it.
        PERFORM warren.project(graph, '_warren_leiden_down', next_g, '_warren_leiden_initial', 'min',
                               persist := false, version := next_v);
        IF g <> graph THEN
          PERFORM warren.drop_vector(g, '_warren_leiden_' || part, gv, true) FROM unnest(parts) part;
          PERFORM warren.drop_graph(g);
        END IF;
        g := next_g;
        gv := next_v;
      END LOOP;
      IF g <> graph THEN
        PERFORM warren.drop_vector(g, '_warren_leiden_' || part, gv, true) FROM unnest(parts) part;
        PERFORM warren.drop_graph(g);
        g := graph;
        gv := v;
      END IF;
      EXIT WHEN iteration = iterations;
      -- Labels are the least dense id of each community's members, so the
      -- partitions are the same exactly when the labels are.
      IF iteration > 1 THEN
        PERFORM warren.vector_map(graph, '_warren_leiden_changed',
                                  'CASE WHEN _warren_leiden_down = _warren_leiden_prev THEN 0 ELSE 1 END', false, v);
        EXIT WHEN warren.vector_reduce(graph, '_warren_leiden_changed', 'sum', v) = 0;
      END IF;
      IF iterations < 0 AND iteration = 100 THEN
        RAISE WARNING 'leiden did not stabilise in 100 iterations';
        EXIT;
      END IF;
    END LOOP;
    PERFORM warren.vector_map(graph, "out", '_warren_leiden_down', true, v);
    q := warren.modularity(graph, "out", resolution, weighted, v);
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_leiden_' || part, v, true) FROM unnest(parts) part;
    RAISE;
  END;
  PERFORM warren.drop_vector(graph, '_warren_leiden_' || part, v, true) FROM unnest(parts) part;
  IF g <> graph THEN
    PERFORM warren.drop_vector(g, '_warren_leiden_' || part, gv, true) FROM unnest(parts) part;
    PERFORM warren.drop_graph(g);
  END IF;
  RETURN q;
END $$;

-- warren.louvain (DESIGN.md §6.7): Louvain community detection (Blondel
-- et al. 2008) as a library over the primitives. Each level moves nodes
-- between communities (warren.local_moving, from each node alone) on the
-- level's graph, lifts the partition found to `graph` (warren.lift), and
-- contracts `graph` by it (warren.contract) into the next level's graph,
-- whose nodes are the communities: contracting `graph` by the composed
-- partition gives the graph that contracting the level's graph would, so
-- every level's graph is a quotient of `graph`, one lift away, and only
-- one exists at a time. It stops when a level merges no node, when a
-- level raises modularity by no more than `tolerance` (NetworkX's
-- `threshold`), or after `max_levels`. The graph is undirected for this
-- (an edge and its reverse add up; edges weighted if `weighted`, NULL: if
-- the graph has weights). Communities are labelled by the smallest dense
-- id of their members: a level's nodes are keyed by those dense ids. Writes
-- vector `out` (i64, on version `version`, NULL: the current one), with
-- each level's partition as `<levels_prefix>_<level>` if `levels_prefix`
-- is given, and returns the partition's modularity. The level's graph, a
-- level graph of warren.contract's (`_warren_level_<version>`), and the
-- intermediate vectors are dropped at the end.
CREATE OR REPLACE FUNCTION warren.louvain(graph text, "out" text, resolution float8 DEFAULT 1.0,
                               seed bigint DEFAULT 0, max_levels int DEFAULT 100,
                               tolerance float8 DEFAULT 1e-7, weighted boolean DEFAULT NULL,
                               levels_prefix text DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS float8 LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['single', 'labels', 'keys', 'down'];
  g text := graph;
  gv bigint := v;
  next_g text;
  next_v bigint;
  level int := 0;
  nodes bigint;
  found bigint;
  q float8;
  q_next float8;
BEGIN
  IF "out" IS NULL OR resolution IS NULL OR seed IS NULL OR max_levels IS NULL OR tolerance IS NULL THEN
    RAISE EXCEPTION '% must not be NULL',
      CASE WHEN "out" IS NULL THEN 'out' WHEN resolution IS NULL THEN 'resolution'
           WHEN seed IS NULL THEN 'seed' WHEN max_levels IS NULL THEN 'max_levels' ELSE 'tolerance' END
      USING ERRCODE = 'null_value_not_allowed';
  END IF;
  IF starts_with("out", '_warren_') OR starts_with(levels_prefix, '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  IF max_levels < 1 THEN
    RAISE EXCEPTION 'max_levels must be at least 1, not %', max_levels USING ERRCODE = 'invalid_parameter_value';
  END IF;
  IF tolerance = 'NaN'::float8 OR tolerance < 0 THEN
    RAISE EXCEPTION 'tolerance must be a number of at least 0, not %', tolerance
      USING ERRCODE = 'invalid_parameter_value';
  END IF;
  BEGIN
    PERFORM warren.vector_dense_id(graph, '_warren_louvain_single', persist := false, version := v);
    q := warren.modularity(graph, '_warren_louvain_single', resolution, weighted, v);
    LOOP
      level := level + 1;
      PERFORM warren.report_progress('louvain', graph, format('level %s', level), level - 1, max_levels);
      nodes := (SELECT (e ->> 'node_count')::bigint FROM jsonb_array_elements(warren.info(g) -> 'versions') e
                WHERE (e ->> 'version')::bigint = gv);
      IF level > 1 THEN
        -- A level's nodes are the communities found below it, keyed by
        -- the least dense id of their members in `graph`.
        PERFORM warren.vector_from_query(g, '_warren_louvain_keys',
          format('SELECT node_id, node_id FROM warren.id_map(%L, NULL::bigint, %s)', g, gv), 'i64',
          persist := false, version := gv);
      END IF;
      found := warren.local_moving(g, '_warren_louvain_labels',
                                   keys := CASE WHEN level > 1 THEN '_warren_louvain_keys' END,
                                   resolution := resolution, seed := seed, tolerance := 0,
                                   weighted := CASE WHEN level = 1 THEN weighted ELSE true END,
                                   persist := false, version := gv);
      EXIT WHEN level > 1 AND found = nodes;
      q_next := warren.modularity(g, '_warren_louvain_labels', resolution,
                                  CASE WHEN level = 1 THEN weighted ELSE true END, gv);
      -- This level's partition, on `graph`.
      IF level = 1 THEN
        PERFORM warren.vector_map(graph, '_warren_louvain_down', '_warren_louvain_labels', false, v);
      ELSE
        PERFORM warren.lift(g, '_warren_louvain_labels', graph, '_warren_louvain_down',
                            persist := false, version := gv);
      END IF;
      IF levels_prefix IS NOT NULL THEN
        PERFORM warren.vector_map(graph, format('%s_%s', levels_prefix, level), '_warren_louvain_down', true, v);
      END IF;
      EXIT WHEN found = nodes OR level >= max_levels OR q_next - q <= tolerance;
      q := q_next;
      -- A level graph of its own, named after its version, a name no
      -- graph can have had.
      next_v := warren.contract(graph, '_warren_louvain_down', NULL,
                                weighted := coalesce(weighted, true), version := v);
      next_g := '_warren_level_' || next_v;
      IF g <> graph THEN
        PERFORM warren.drop_vector(g, '_warren_louvain_' || part, gv, true) FROM unnest(parts) part;
        PERFORM warren.drop_graph(g);
      END IF;
      g := next_g;
      gv := next_v;
    END LOOP;
    PERFORM warren.vector_map(graph, "out", '_warren_louvain_down', true, v);
    q := warren.modularity(graph, "out", resolution, weighted, v);
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_louvain_' || part, v, true) FROM unnest(parts) part;
    RAISE;
  END;
  PERFORM warren.drop_vector(graph, '_warren_louvain_' || part, v, true) FROM unnest(parts) part;
  IF g <> graph THEN
    PERFORM warren.drop_vector(g, '_warren_louvain_' || part, gv, true) FROM unnest(parts) part;
    PERFORM warren.drop_graph(g);
  END IF;
  RETURN q;
END $$;
