-- pg_warren 0.3.0 -> 0.4.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.3.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.3.0 yields the catalog of a fresh
-- 0.4.0 install. In a trusted install it runs as the bootstrap superuser
-- (DESIGN.md §7): create only in schema warren, schema-qualify, and use only
-- exact-match pg_catalog operators and functions in what runs here.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.4.0'" to load this file. \quit

-- Node order (DESIGN.md §14 item 22): how a graph's dense ids are ordered
-- at its first build and at compaction. Graphs created before the update
-- are backfilled 'id', the order every version of theirs was built in.
ALTER TABLE warren.graphs ADD COLUMN node_order text NOT NULL DEFAULT 'id'
    CHECK (node_order IN ('id'::pg_catalog.text, 'locality'::pg_catalog.text));

-- create_graph and compact gain a node_order parameter, so each is dropped
-- and created again. That would reset their privileges to the default
-- (EXECUTE to PUBLIC), so any privileges set on them are granted again,
-- recorded as granted by the functions' owner (the original grantor and
-- grant-option chains are not kept). Grants made by an extension script are
-- recorded as the functions' initial privileges, which pg_dump leaves out;
-- a fresh install records none for them, so those rows are deleted again
-- and a dump keeps the grants. Everything evaluated here uses exact-match
-- pg_catalog operators (DESIGN.md §7): this runs as the bootstrap
-- superuser, and a trusted install's installer can write the extension's
-- schema.
DO $upgrade$
DECLARE
    create_graph_acl pg_catalog.aclitem[] := (
        SELECT p.proacl FROM pg_catalog.pg_proc p
        WHERE p.oid OPERATOR(pg_catalog.=) 'warren.create_graph(text, text, text, boolean, boolean, boolean, boolean, integer, boolean)'::pg_catalog.regprocedure::pg_catalog.oid);
    compact_acl pg_catalog.aclitem[] := (
        SELECT p.proacl FROM pg_catalog.pg_proc p
        WHERE p.oid OPERATOR(pg_catalog.=) 'warren.compact(text)'::pg_catalog.regprocedure::pg_catalog.oid);
    kept record;
BEGIN
    DROP FUNCTION warren.create_graph(text, text, text, boolean, boolean, boolean, boolean, integer, boolean);
    CREATE FUNCTION warren."create_graph"(
        "name" TEXT,
        "node_query" TEXT,
        "edge_query" TEXT,
        "directed" bool DEFAULT true,
        "weighted" bool DEFAULT false,
        "has_relations" bool DEFAULT false,
        "build_reverse" bool DEFAULT true,
        "retain_versions" INT DEFAULT 2,
        "store_merged" bool DEFAULT true,
        "node_order" TEXT DEFAULT 'id'
    ) RETURNS void
    STRICT
    LANGUAGE c
    AS 'MODULE_PATHNAME', 'create_graph_wrapper';

    DROP FUNCTION warren.compact(text);
    CREATE FUNCTION warren."compact"(
        "name" TEXT,
        "node_order" TEXT DEFAULT NULL
    ) RETURNS bigint
    LANGUAGE c
    AS 'MODULE_PATHNAME', 'compact_wrapper';

    -- An explicit ACL: start from no grants, then grant each entry again.
    IF create_graph_acl IS NOT NULL THEN
        REVOKE ALL ON FUNCTION warren.create_graph(text, text, text, boolean, boolean, boolean,
                                                   boolean, integer, boolean, text) FROM PUBLIC;
    END IF;
    IF compact_acl IS NOT NULL THEN
        REVOKE ALL ON FUNCTION warren.compact(text, text) FROM PUBLIC;
    END IF;
    FOR kept IN
        SELECT f.signature, a.grantee, a.is_grantable
        FROM (VALUES
                ('warren.create_graph(text, text, text, boolean, boolean, boolean, boolean, integer, boolean, text)',
                 create_graph_acl),
                ('warren.compact(text, text)', compact_acl)) f (signature, acl),
             pg_catalog.aclexplode(f.acl) a
    LOOP
        EXECUTE pg_catalog.format('GRANT EXECUTE ON FUNCTION %s TO %s%s', kept.signature,
            CASE WHEN kept.grantee OPERATOR(pg_catalog.=) 0::pg_catalog.oid THEN 'PUBLIC'
                 ELSE kept.grantee::pg_catalog.regrole::pg_catalog.text END,
            CASE WHEN kept.is_grantable THEN ' WITH GRANT OPTION' ELSE '' END);
    END LOOP;
    DELETE FROM pg_catalog.pg_init_privs i
    WHERE i.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass::pg_catalog.oid
      AND i.objoid OPERATOR(pg_catalog.=) ANY (ARRAY[
          'warren.create_graph(text, text, text, boolean, boolean, boolean, boolean, integer, boolean, text)'::pg_catalog.regprocedure::pg_catalog.oid,
          'warren.compact(text, text)'::pg_catalog.regprocedure::pg_catalog.oid]);
END $upgrade$;

-- warren.eigenvector_centrality (DESIGN.md §6.7): eigenvector centrality
-- by power iteration, as NetworkX computes it. Centrality flows along edges
-- (the left eigenvector of A: a node is central when central nodes point at
-- it), weighted by edge weight (spmv's rule: weights if the graph has them,
-- unless weighted := false). From the uniform 1/N, each iteration takes
-- x + Aᵀx (the shift by I keeps a periodic graph from oscillating) and
-- scales it to unit L2 norm; it stops when the L1 change falls below
-- (live nodes) × tol, or after max_iter iterations with a WARNING (NetworkX
-- raises). Writes vector `out` (f64, unit L2 norm) and returns the
-- iterations run. Constants enter expressions as constant vectors.
CREATE FUNCTION warren.eigenvector_centrality(graph text, "out" text, tol float8 DEFAULT 1e-6,
                                              max_iter int DEFAULT 100, rels text[] DEFAULT NULL,
                                              weighted boolean DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS int LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['one', 'x', 'y', 'next', 'norm'];
  nodes float8;
  norm float8;
  delta float8;
  i int := 0;
BEGIN
  IF starts_with("out", '_warren_') THEN
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
    PERFORM warren.vector_const(graph, '_warren_eigenvector_centrality_one', 1, persist := false, version := v);
    nodes := warren.vector_reduce(graph, '_warren_eigenvector_centrality_one', 'sum', v);
    IF nodes = 0 THEN
      RAISE EXCEPTION 'graph "%" has no live nodes', graph USING ERRCODE = 'invalid_parameter_value';
    END IF;
    PERFORM warren.vector_const(graph, '_warren_eigenvector_centrality_x', 1.0 / nodes, persist := false,
                                version := v);
    WHILE i < max_iter LOOP
      i := i + 1;
      PERFORM warren.spmv(graph, '_warren_eigenvector_centrality_x', '_warren_eigenvector_centrality_y',
                          transpose := true, rels := rels, weighted := weighted, persist := false, version := v);
      PERFORM warren.vector_map(graph, '_warren_eigenvector_centrality_next',
        '_warren_eigenvector_centrality_x + _warren_eigenvector_centrality_y', false, v);
      norm := warren.vector_reduce(graph, '_warren_eigenvector_centrality_next', 'l2', v);
      PERFORM warren.vector_const(graph, '_warren_eigenvector_centrality_norm',
                                  CASE WHEN norm = 0 THEN 1 ELSE norm END, persist := false, version := v);
      PERFORM warren.vector_map(graph, '_warren_eigenvector_centrality_next',
        '_warren_eigenvector_centrality_next / _warren_eigenvector_centrality_norm', false, v);
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
