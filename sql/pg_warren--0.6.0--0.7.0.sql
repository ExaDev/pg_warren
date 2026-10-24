-- pg_warren 0.6.0 -> 0.7.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.6.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.6.0 yields the catalog of a fresh
-- 0.7.0 install. In a trusted install it runs as the bootstrap superuser
-- (DESIGN.md §7): create only in schema warren, schema-qualify, and use only
-- exact-match pg_catalog operators and functions in what runs here.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.7.0'" to load this file. \quit

-- per_relation (DESIGN.md §6.3): every function whose result a pair of
-- nodes linked in several relations can change takes a trailing
-- `per_relation boolean DEFAULT false`, so each is dropped and created
-- again. As gather in 0.4.0 -> 0.5.0, any privileges set on one are granted
-- again by its owner, and the initial privileges this script would record
-- are deleted.
DO $upgrade$
DECLARE
    olds text[] := ARRAY[
        'warren.contract(text, text, text, boolean, boolean, boolean, bigint)',
        'warren.modularity(text, text, double precision, boolean, bigint)',
        'warren.local_moving(text, text, text, text, double precision, bigint, integer, double precision, boolean, boolean, bigint)',
        'warren.leiden_refine(text, text, text, text, double precision, bigint, double precision, boolean, boolean, bigint)',
        'warren.louvain(text, text, double precision, bigint, integer, double precision, boolean, text, bigint)',
        'warren.leiden(text, text, double precision, bigint, integer, double precision, double precision, integer, boolean, text, bigint)',
        'warren.spmv(text, text, text, text, boolean, text[], boolean, text, boolean, boolean, text, boolean, bigint)',
        'warren.spmm(text, text, text, text, boolean, text[], boolean, text, boolean, boolean, text, boolean, bigint)',
        'warren.pagerank(text, text, double precision, double precision, integer, text, text[], boolean, bigint)',
        'warren.katz_centrality(text, text, double precision, double precision, text, boolean, double precision, integer, text[], boolean, bigint)',
        'warren.eigenvector_centrality(text, text, double precision, integer, text[], boolean, bigint)',
        'warren.hits(text, text, text, double precision, integer, boolean, text[], boolean, bigint)',
        'warren.sssp_vector(text, text, anyelement, text[], boolean, integer, bigint)',
        'warren.degree(text, anyelement, text, text[], bigint)',
        'warren.vector_degree(text, text, text, text[], text, boolean, bigint)',
        'warren.degree_vector(text, text, text, text[], bigint)',
        'warren.aggregate_neighbours(text, text, text, text, integer, text[], text, bigint)'];
    news text[] := ARRAY[
        'warren.contract(text, text, text, boolean, boolean, boolean, bigint, boolean)',
        'warren.modularity(text, text, double precision, boolean, bigint, boolean)',
        'warren.local_moving(text, text, text, text, double precision, bigint, integer, double precision, boolean, boolean, bigint, boolean)',
        'warren.leiden_refine(text, text, text, text, double precision, bigint, double precision, boolean, boolean, bigint, boolean)',
        'warren.louvain(text, text, double precision, bigint, integer, double precision, boolean, text, bigint, boolean)',
        'warren.leiden(text, text, double precision, bigint, integer, double precision, double precision, integer, boolean, text, bigint, boolean)',
        'warren.spmv(text, text, text, text, boolean, text[], boolean, text, boolean, boolean, text, boolean, bigint, boolean)',
        'warren.spmm(text, text, text, text, boolean, text[], boolean, text, boolean, boolean, text, boolean, bigint, boolean)',
        'warren.pagerank(text, text, double precision, double precision, integer, text, text[], boolean, bigint, boolean)',
        'warren.katz_centrality(text, text, double precision, double precision, text, boolean, double precision, integer, text[], boolean, bigint, boolean)',
        'warren.eigenvector_centrality(text, text, double precision, integer, text[], boolean, bigint, boolean)',
        'warren.hits(text, text, text, double precision, integer, boolean, text[], boolean, bigint, boolean)',
        'warren.sssp_vector(text, text, anyelement, text[], boolean, integer, bigint, boolean)',
        'warren.degree(text, anyelement, text, text[], bigint, boolean)',
        'warren.vector_degree(text, text, text, text[], text, boolean, bigint, boolean)',
        'warren.degree_vector(text, text, text, text[], bigint, boolean)',
        'warren.aggregate_neighbours(text, text, text, text, integer, text[], text, bigint, boolean)'];
    acls text[] := ARRAY[]::text[];
    kept record;
BEGIN
    FOR i IN 1 .. pg_catalog.array_length(olds, 1) LOOP
        acls[i] := (SELECT p.proacl::text FROM pg_catalog.pg_proc p
                    WHERE p.oid OPERATOR(pg_catalog.=) olds[i]::pg_catalog.regprocedure::pg_catalog.oid);
        EXECUTE pg_catalog.format('DROP FUNCTION %s', olds[i]);
    END LOOP;

CREATE FUNCTION warren."contract"(
	"graph" TEXT,
	"label_vector" TEXT,
	"out_graph" TEXT,
	"weighted" bool DEFAULT true,
	"drop_self_loops" bool DEFAULT false,
	"promote" bool DEFAULT true,
	"version" bigint DEFAULT NULL,
	"per_relation" bool DEFAULT false
) RETURNS bigint
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'contract_wrapper';

CREATE FUNCTION warren."modularity"(
	"graph" TEXT,
	"labels" TEXT,
	"resolution" double precision DEFAULT 1.0,
	"weighted" bool DEFAULT NULL,
	"version" bigint DEFAULT NULL,
	"per_relation" bool DEFAULT false
) RETURNS double precision
STABLE PARALLEL RESTRICTED
LANGUAGE c
AS 'MODULE_PATHNAME', 'modularity_wrapper';

CREATE FUNCTION warren."local_moving"(
	"graph" TEXT,
	"out" TEXT,
	"initial" TEXT DEFAULT NULL,
	"keys" TEXT DEFAULT NULL,
	"resolution" double precision DEFAULT 1.0,
	"seed" bigint DEFAULT 0,
	"max_sweeps" INT DEFAULT 100,
	"tolerance" double precision DEFAULT 1e-7,
	"weighted" bool DEFAULT NULL,
	"persist" bool DEFAULT true,
	"version" bigint DEFAULT NULL,
	"per_relation" bool DEFAULT false
) RETURNS bigint
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'local_moving_wrapper';

CREATE FUNCTION warren."leiden_refine"(
	"graph" TEXT,
	"communities" TEXT,
	"out" TEXT,
	"keys" TEXT DEFAULT NULL,
	"resolution" double precision DEFAULT 1.0,
	"seed" bigint DEFAULT 0,
	"randomness" double precision DEFAULT 0.01,
	"weighted" bool DEFAULT NULL,
	"persist" bool DEFAULT true,
	"version" bigint DEFAULT NULL,
	"per_relation" bool DEFAULT false
) RETURNS bigint
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'leiden_refine_wrapper';

CREATE FUNCTION warren.louvain(graph text, "out" text, resolution float8 DEFAULT 1.0,
                               seed bigint DEFAULT 0, max_levels int DEFAULT 100,
                               tolerance float8 DEFAULT 1e-7, weighted boolean DEFAULT NULL,
                               levels_prefix text DEFAULT NULL, version bigint DEFAULT NULL, per_relation boolean DEFAULT false)
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
    q := warren.modularity(graph, '_warren_louvain_single', resolution, weighted, v, per_relation);
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
                                   per_relation := level = 1 AND per_relation,
                                   persist := false, version := gv);
      EXIT WHEN level > 1 AND found = nodes;
      q_next := warren.modularity(g, '_warren_louvain_labels', resolution,
                                  CASE WHEN level = 1 THEN weighted ELSE true END, gv, level = 1 AND per_relation);
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
                                weighted := coalesce(weighted, true), version := v,
                                per_relation := per_relation);
      next_g := '_warren_level_' || next_v;
      IF g <> graph THEN
        PERFORM warren.drop_vector(g, '_warren_louvain_' || part, gv, true) FROM unnest(parts) part;
        PERFORM warren.drop_graph(g);
      END IF;
      g := next_g;
      gv := next_v;
    END LOOP;
    PERFORM warren.vector_map(graph, "out", '_warren_louvain_down', true, v);
    q := warren.modularity(graph, "out", resolution, weighted, v, per_relation);
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

CREATE FUNCTION warren.leiden(graph text, "out" text, resolution float8 DEFAULT 1.0,
                              seed bigint DEFAULT 0, max_levels int DEFAULT 100,
                              tolerance float8 DEFAULT 1e-7, randomness float8 DEFAULT 0.01,
                              iterations int DEFAULT 2, weighted boolean DEFAULT NULL, levels_prefix text DEFAULT NULL,
                              version bigint DEFAULT NULL, per_relation boolean DEFAULT false)
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
                                   per_relation := level = 1 AND per_relation,
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
                                   per_relation := level = 1 AND per_relation,
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
                                  weighted := coalesce(weighted, true), version := v,
                                per_relation := per_relation);
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
    q := warren.modularity(graph, "out", resolution, weighted, v, per_relation);
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

CREATE FUNCTION warren."spmv"(
	"graph" TEXT,
	"x" TEXT,
	"y" TEXT,
	"semiring" TEXT DEFAULT 'plus_times',
	"transpose" bool DEFAULT false,
	"rels" TEXT[] DEFAULT NULL,
	"weighted" bool DEFAULT NULL,
	"mask" TEXT DEFAULT NULL,
	"mask_complement" bool DEFAULT false,
	"replace" bool DEFAULT false,
	"accumulate" TEXT DEFAULT NULL,
	"persist" bool DEFAULT true,
	"version" bigint DEFAULT NULL,
	"per_relation" bool DEFAULT false
) RETURNS void
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'spmv_wrapper';

CREATE FUNCTION warren."spmm"(
	"graph" TEXT,
	"x" TEXT,
	"y" TEXT,
	"semiring" TEXT DEFAULT 'plus_times',
	"transpose" bool DEFAULT false,
	"rels" TEXT[] DEFAULT NULL,
	"weighted" bool DEFAULT NULL,
	"mask" TEXT DEFAULT NULL,
	"mask_complement" bool DEFAULT false,
	"replace" bool DEFAULT false,
	"accumulate" TEXT DEFAULT NULL,
	"persist" bool DEFAULT true,
	"version" bigint DEFAULT NULL,
	"per_relation" bool DEFAULT false
) RETURNS void
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'spmm_wrapper';

CREATE FUNCTION warren.pagerank(graph text, "out" text, damping float8 DEFAULT 0.85,
                                tol float8 DEFAULT 1e-6, max_iter int DEFAULT 100,
                                seed text DEFAULT NULL, rels text[] DEFAULT NULL,
                                weighted boolean DEFAULT NULL, version bigint DEFAULT NULL, per_relation boolean DEFAULT false)
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
                        rels := rels, weighted := weighted, per_relation := per_relation, persist := false, version := v);
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
                          rels := rels, weighted := weighted, per_relation := per_relation, persist := false, version := v);
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

CREATE FUNCTION warren.katz_centrality(graph text, "out" text, alpha float8 DEFAULT 0.1,
                                       beta float8 DEFAULT 1.0, seed text DEFAULT NULL,
                                       normalized boolean DEFAULT true, tol float8 DEFAULT 1e-6,
                                       max_iter int DEFAULT 1000, rels text[] DEFAULT NULL,
                                       weighted boolean DEFAULT NULL, version bigint DEFAULT NULL, per_relation boolean DEFAULT false)
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
                          transpose := true, rels := rels, weighted := weighted, per_relation := per_relation, persist := false, version := v);
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

CREATE FUNCTION warren.hits(graph text, hubs text, authorities text, tol float8 DEFAULT 1e-8,
                            max_iter int DEFAULT 100, normalized boolean DEFAULT true,
                            rels text[] DEFAULT NULL, weighted boolean DEFAULT NULL,
                            version bigint DEFAULT NULL, per_relation boolean DEFAULT false)
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
                          weighted := weighted, per_relation := per_relation, persist := false, version := v);
      PERFORM warren.spmv(graph, '_warren_hits_a', '_warren_hits_next', rels := rels, weighted := weighted, per_relation := per_relation,
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

CREATE FUNCTION warren.sssp_vector(graph text, "out" text, source anyelement,
                                   rels text[] DEFAULT NULL, weighted boolean DEFAULT NULL,
                                   max_iter int DEFAULT NULL, version bigint DEFAULT NULL, per_relation boolean DEFAULT false)
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
                          transpose := true, rels := rels, weighted := weighted, per_relation := per_relation, accumulate := 'min',
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

CREATE FUNCTION warren.degree(graph text, node anyelement, direction text DEFAULT 'out',
                              rels text[] DEFAULT NULL, version bigint DEFAULT NULL,
                              per_relation boolean DEFAULT false)
RETURNS bigint
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'degree_wrapper';

CREATE FUNCTION warren."vector_degree"(
	"graph" TEXT,
	"name" TEXT,
	"direction" TEXT DEFAULT 'out',
	"rels" TEXT[] DEFAULT NULL,
	"dtype" TEXT DEFAULT 'f64',
	"persist" bool DEFAULT true,
	"version" bigint DEFAULT NULL,
	"per_relation" bool DEFAULT false
) RETURNS bigint
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'vector_degree_wrapper';

CREATE FUNCTION warren.degree_vector(graph text, "out" text, direction text DEFAULT 'out',
                                     rels text[] DEFAULT NULL, version bigint DEFAULT NULL,
                                     per_relation boolean DEFAULT false)
RETURNS bigint LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
  IF starts_with("out", '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  RETURN warren.vector_degree(graph, "out", direction, rels, 'f64', true, version, per_relation);
END $$;

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

    FOR i IN 1 .. pg_catalog.array_length(news, 1) LOOP
        -- An explicit ACL: start from no grants, then grant each entry again.
        IF acls[i] IS NOT NULL THEN
            EXECUTE pg_catalog.format('REVOKE ALL ON FUNCTION %s FROM PUBLIC', news[i]);
            FOR kept IN
                SELECT a.grantee, a.is_grantable FROM pg_catalog.aclexplode(acls[i]::pg_catalog.aclitem[]) a
            LOOP
                EXECUTE pg_catalog.format('GRANT EXECUTE ON FUNCTION %s TO %s%s', news[i],
                    CASE WHEN kept.grantee OPERATOR(pg_catalog.=) 0::pg_catalog.oid THEN 'PUBLIC'
                         ELSE kept.grantee::pg_catalog.regrole::pg_catalog.text END,
                    CASE WHEN kept.is_grantable THEN ' WITH GRANT OPTION' ELSE '' END);
            END LOOP;
        END IF;
        DELETE FROM pg_catalog.pg_init_privs ip
        WHERE ip.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass::pg_catalog.oid
          AND ip.objoid OPERATOR(pg_catalog.=) news[i]::pg_catalog.regprocedure::pg_catalog.oid;
    END LOOP;
END $upgrade$;
