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
        'warren.leiden(text, text, double precision, bigint, integer, double precision, double precision, integer, boolean, text, bigint)'];
    news text[] := ARRAY[
        'warren.contract(text, text, text, boolean, boolean, boolean, bigint, boolean)',
        'warren.modularity(text, text, double precision, boolean, bigint, boolean)',
        'warren.local_moving(text, text, text, text, double precision, bigint, integer, double precision, boolean, boolean, bigint, boolean)',
        'warren.leiden_refine(text, text, text, text, double precision, bigint, double precision, boolean, boolean, bigint, boolean)',
        'warren.louvain(text, text, double precision, bigint, integer, double precision, boolean, text, bigint, boolean)',
        'warren.leiden(text, text, double precision, bigint, integer, double precision, double precision, integer, boolean, text, bigint, boolean)'];
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
