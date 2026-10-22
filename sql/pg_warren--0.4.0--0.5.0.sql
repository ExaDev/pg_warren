-- pg_warren 0.4.0 -> 0.5.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.4.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.4.0 yields the catalog of a fresh
-- 0.5.0 install. In a trusted install it runs as the bootstrap superuser
-- (DESIGN.md §7): create only in schema warren, schema-qualify, and use only
-- exact-match pg_catalog operators and functions in what runs here.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.5.0'" to load this file. \quit

-- warren.shared_cache() (DESIGN.md §14 item 37): what the shared cache holds
-- of this database's graphs that the caller may read, and the cluster's
-- totals against the cap.
CREATE FUNCTION warren."shared_cache"() RETURNS TABLE (
    "kind" TEXT,
    "graph" TEXT,
    "version" bigint,
    "vector" TEXT,
    "vector_id" bigint,
    "state" TEXT,
    "bytes" bigint,
    "pins" INT,
    "mappers" INT,
    "last_use" bigint,
    "cap" bigint
)
STRICT VOLATILE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'shared_cache_wrapper';

-- gather gains an include_self parameter (DESIGN.md §6.4: with the mode
-- combiner, majority-vote label propagation counts its votes in-engine), so
-- it is dropped and created again. As create_graph and compact were in
-- 0.3.0 -> 0.4.0, any privileges set on it are granted again by its owner,
-- and the initial privileges this script would record are deleted.
DO $upgrade$
DECLARE
    gather_acl pg_catalog.aclitem[] := (
        SELECT p.proacl FROM pg_catalog.pg_proc p
        WHERE p.oid OPERATOR(pg_catalog.=) 'warren.gather(text, text, anyelement, anycompatible, text, text, text[], text, boolean, bigint)'::pg_catalog.regprocedure::pg_catalog.oid);
    kept record;
BEGIN
    DROP FUNCTION warren.gather(text, text, anyelement, anycompatible, text, text, text[], text, boolean, bigint);
    CREATE FUNCTION warren.gather(graph text, messages text, id_type anyelement,
                                  value_type anycompatible, combine text DEFAULT 'collect',
                                  direction text DEFAULT 'in', rels text[] DEFAULT NULL,
                                  mask text DEFAULT NULL, mask_complement boolean DEFAULT false,
                                  version bigint DEFAULT NULL, include_self boolean DEFAULT false)
    RETURNS TABLE (node_id anyelement, value anycompatible)
    VOLATILE PARALLEL UNSAFE LANGUAGE c AS 'MODULE_PATHNAME', 'gather_wrapper';

    -- An explicit ACL: start from no grants, then grant each entry again.
    IF gather_acl IS NOT NULL THEN
        REVOKE ALL ON FUNCTION warren.gather(text, text, anyelement, anycompatible, text, text, text[],
                                             text, boolean, bigint, boolean) FROM PUBLIC;
    END IF;
    FOR kept IN
        SELECT a.grantee, a.is_grantable FROM pg_catalog.aclexplode(gather_acl) a
    LOOP
        EXECUTE pg_catalog.format('GRANT EXECUTE ON FUNCTION %s TO %s%s',
            'warren.gather(text, text, anyelement, anycompatible, text, text, text[], text, boolean, bigint, boolean)',
            CASE WHEN kept.grantee OPERATOR(pg_catalog.=) 0::pg_catalog.oid THEN 'PUBLIC'
                 ELSE kept.grantee::pg_catalog.regrole::pg_catalog.text END,
            CASE WHEN kept.is_grantable THEN ' WITH GRANT OPTION' ELSE '' END);
    END LOOP;
    DELETE FROM pg_catalog.pg_init_privs i
    WHERE i.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass::pg_catalog.oid
      AND i.objoid OPERATOR(pg_catalog.=)
          'warren.gather(text, text, anyelement, anycompatible, text, text, text[], text, boolean, bigint, boolean)'::pg_catalog.regprocedure::pg_catalog.oid;
END $upgrade$;

-- label_propagation_majority counts its votes with gather's mode combiner
-- (DESIGN.md §6.7); its results are unchanged.
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
