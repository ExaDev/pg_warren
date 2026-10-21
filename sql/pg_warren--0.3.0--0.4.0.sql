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

-- pagerank scales a seed by the reciprocal of its sum (DESIGN.md §6.7):
-- dividing by a constant vector failed at tombstoned rows.
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
CREATE FUNCTION warren.eigenvector_centrality(graph text, "out" text, tol float8 DEFAULT 1e-6,
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
CREATE FUNCTION warren.katz_centrality(graph text, "out" text, alpha float8 DEFAULT 0.1,
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

-- warren.clustering (DESIGN.md §6.7): the local clustering coefficient,
-- as NetworkX's clustering computes it for an undirected, unweighted
-- graph: 2T / (d(d - 1)) from each node's triangles T (warren.triangles:
-- directions and self-loops ignored) and its distinct neighbours d in
-- either direction over `rels`, and 0 where T is 0. Unlike NetworkX, a
-- self-loop counts its node among its own neighbours in d (vector_degree
-- does; no primitive tells which nodes have one). Edge weights are not
-- read. Writes vector `out` (f64).
CREATE FUNCTION warren.clustering(graph text, "out" text, rels text[] DEFAULT NULL,
                                  version bigint DEFAULT NULL)
RETURNS void LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['triangles', 'degree'];
BEGIN
  IF "out" IS NULL THEN
    RAISE EXCEPTION 'out must not be NULL' USING ERRCODE = 'null_value_not_allowed';
  END IF;
  IF starts_with("out", '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  BEGIN
    PERFORM warren.triangles(graph, '_warren_clustering_triangles', rels, false, v);
    PERFORM warren.vector_degree(graph, '_warren_clustering_degree', 'both', rels, 'f64', false, v);
    PERFORM warren.vector_map(graph, "out",
      'CASE WHEN _warren_clustering_triangles = 0 THEN 0
            ELSE 2 * _warren_clustering_triangles::float8
                 / (_warren_clustering_degree * (_warren_clustering_degree - 1)) END', true, v);
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_clustering_' || part, v, true) FROM unnest(parts) part;
    RAISE;
  END;
  PERFORM warren.drop_vector(graph, '_warren_clustering_' || part, v, true) FROM unnest(parts) part;
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
-- matches step for step. Tier 2: each iteration is one `gather` (collect)
-- whose received labels are counted by SQL, N + M rows through the
-- executor. Writes vector `out` (f64) and returns the iterations run.
CREATE FUNCTION warren.label_propagation_majority(graph text, "out" text, seed text DEFAULT NULL,
                                                  max_iter int DEFAULT 100, rels text[] DEFAULT NULL,
                                                  version bigint DEFAULT NULL)
RETURNS int LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  info jsonb := warren.info(graph);
  v bigint := coalesce(version, (info ->> 'current_version')::bigint);
  parts text[] := ARRAY['x', 'next'];
  -- Each node's own label and its neighbours', counted; the most votes
  -- win, then the smallest label.
  votes text := format(
    'SELECT DISTINCT ON (node_id) node_id, label
     FROM (SELECT node_id, label, count(*) AS votes
           FROM (SELECT g.node_id, unnest(g.value) AS label
                 FROM warren.gather(%1$L, %2$L, NULL::%3$s, NULL::float8[], ''collect'', ''both'', %4$L::text[],
                                    version := %5$s) g
                 UNION ALL
                 SELECT x.node_id, x.value FROM warren.vector_values(%1$L, %2$L, NULL::%3$s, %5$s) x) m
           GROUP BY node_id, label) c
     ORDER BY node_id, votes DESC, label',
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

-- Structural kernels of the algorithm library (DESIGN.md §6.7, planned
-- library group 2).

CREATE FUNCTION warren."core_number"(
	"graph" TEXT,
	"out" TEXT,
	"rels" TEXT[] DEFAULT NULL,
	"persist" bool DEFAULT true,
	"version" bigint DEFAULT NULL
) RETURNS bigint
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'core_number_wrapper';

CREATE FUNCTION warren."strong_components_vector"(
	"graph" TEXT,
	"out" TEXT,
	"rels" TEXT[] DEFAULT NULL,
	"persist" bool DEFAULT true,
	"version" bigint DEFAULT NULL
) RETURNS bigint
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'strong_components_vector_wrapper';

CREATE FUNCTION warren."topological_order"(
	"graph" TEXT,
	"out" TEXT,
	"rels" TEXT[] DEFAULT NULL,
	"persist" bool DEFAULT true,
	"version" bigint DEFAULT NULL
) RETURNS bigint
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'topological_order_wrapper';

CREATE FUNCTION warren."dag_longest_path"(
	"graph" TEXT,
	"out" TEXT,
	"rels" TEXT[] DEFAULT NULL,
	"weighted" bool DEFAULT NULL,
	"persist" bool DEFAULT true,
	"version" bigint DEFAULT NULL
) RETURNS double precision
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'dag_longest_path_wrapper';

CREATE FUNCTION warren.minimum_spanning_forest(graph text, id_type anyelement, rels text[] DEFAULT NULL,
                                               weighted bool DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS TABLE (source anyelement, target anyelement, weight float8)
STABLE PARALLEL RESTRICTED LANGUAGE c AS 'MODULE_PATHNAME', 'minimum_spanning_forest_wrapper';

-- Contraction (DESIGN.md §6.6): a graph's versions may be derived from
-- another graph's versions by warren.contract. graphs.source says which;
-- versions.parent_version names the version a contraction was contracted
-- from, and is cleared when that version is dropped; versions.label_vector
-- names the vector it was contracted on, and is cleared by a trigger when
-- that vector is deleted (it cannot be a foreign key: with
-- vectors.version_id it would make a cycle, which pg_dump cannot order).
-- Every row is a query graph's, with no parent, before the update.
ALTER TABLE warren.graphs ADD CONSTRAINT graphs_source_check
    CHECK (source IN ('query'::pg_catalog.text, 'contraction'::pg_catalog.text));
ALTER TABLE warren.versions DROP CONSTRAINT versions_parent_version_fkey,
    ADD CONSTRAINT versions_parent_version_fkey FOREIGN KEY (parent_version)
        REFERENCES warren.versions ON DELETE SET NULL;
CREATE INDEX versions_label_vector ON warren.versions (label_vector)
    WHERE label_vector IS NOT NULL;
CREATE INDEX versions_parent_version ON warren.versions (parent_version)
    WHERE parent_version IS NOT NULL;
CREATE FUNCTION warren.label_vector_is_forgotten() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    UPDATE warren.versions SET label_vector = NULL
    WHERE label_vector OPERATOR(pg_catalog.=) OLD.vector_id;
    RETURN OLD;
END $$;
CREATE TRIGGER label_vector_is_forgotten AFTER DELETE ON warren.vectors
    FOR EACH ROW EXECUTE FUNCTION warren.label_vector_is_forgotten();
REVOKE EXECUTE ON FUNCTION warren.label_vector_is_forgotten() FROM PUBLIC;

CREATE FUNCTION warren."contract"(
	"graph" TEXT,
	"label_vector" TEXT,
	"out_graph" TEXT,
	"weighted" bool DEFAULT true,
	"drop_self_loops" bool DEFAULT false,
	"promote" bool DEFAULT true,
	"version" bigint DEFAULT NULL
) RETURNS bigint
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'contract_wrapper';

CREATE FUNCTION warren."project"(
	"graph" TEXT,
	"x" TEXT,
	"out_graph" TEXT,
	"y" TEXT,
	"agg" TEXT DEFAULT 'sum',
	"persist" bool DEFAULT true,
	"version" bigint DEFAULT NULL
) RETURNS void
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'project_wrapper';

CREATE FUNCTION warren."lift"(
	"out_graph" TEXT,
	"y" TEXT,
	"graph" TEXT,
	"x" TEXT,
	"persist" bool DEFAULT true,
	"version" bigint DEFAULT NULL
) RETURNS void
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'lift_wrapper';

-- Shortest-path centralities and neighbourhood similarity (planned library
-- group 2, DESIGN.md §6.7).

CREATE FUNCTION warren."betweenness_centrality"(
	"graph" TEXT,
	"out" TEXT,
	"k" bigint DEFAULT NULL,
	"seed" bigint DEFAULT 0,
	"normalized" bool DEFAULT true,
	"endpoints" bool DEFAULT false,
	"directed" bool DEFAULT true,
	"sources" TEXT DEFAULT NULL,
	"rels" TEXT[] DEFAULT NULL,
	"weighted" bool DEFAULT NULL,
	"persist" bool DEFAULT true,
	"version" bigint DEFAULT NULL
) RETURNS bigint
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'betweenness_centrality_wrapper';

CREATE FUNCTION warren."closeness_centrality"(
	"graph" TEXT,
	"out" TEXT,
	"k" bigint DEFAULT NULL,
	"seed" bigint DEFAULT 0,
	"wf_improved" bool DEFAULT true,
	"directed" bool DEFAULT true,
	"sources" TEXT DEFAULT NULL,
	"rels" TEXT[] DEFAULT NULL,
	"weighted" bool DEFAULT NULL,
	"persist" bool DEFAULT true,
	"version" bigint DEFAULT NULL
) RETURNS bigint
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'closeness_centrality_wrapper';

CREATE FUNCTION warren."harmonic_centrality"(
	"graph" TEXT,
	"out" TEXT,
	"k" bigint DEFAULT NULL,
	"seed" bigint DEFAULT 0,
	"directed" bool DEFAULT true,
	"sources" TEXT DEFAULT NULL,
	"rels" TEXT[] DEFAULT NULL,
	"weighted" bool DEFAULT NULL,
	"persist" bool DEFAULT true,
	"version" bigint DEFAULT NULL
) RETURNS bigint
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'harmonic_centrality_wrapper';

CREATE FUNCTION warren.similarity(graph text, pairs text, id_type anyelement,
                                  measure text DEFAULT 'jaccard', rels text[] DEFAULT NULL,
                                  version bigint DEFAULT NULL)
RETURNS TABLE (source anyelement, target anyelement, score float8)
STABLE PARALLEL RESTRICTED LANGUAGE c AS 'MODULE_PATHNAME', 'similarity_wrapper';

CREATE FUNCTION warren.most_similar(graph text, node anyelement, k bigint DEFAULT 10,
                                    measure text DEFAULT 'jaccard', rels text[] DEFAULT NULL,
                                    version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, score float8)
STABLE PARALLEL RESTRICTED LANGUAGE c AS 'MODULE_PATHNAME', 'most_similar_wrapper';

CREATE FUNCTION warren.similar_pairs(graph text, id_type anyelement, k bigint DEFAULT 10,
                                     measure text DEFAULT 'jaccard', nodes text DEFAULT NULL,
                                     rels text[] DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS TABLE (source anyelement, target anyelement, score float8)
STABLE PARALLEL RESTRICTED LANGUAGE c AS 'MODULE_PATHNAME', 'similar_pairs_wrapper';

-- Community detection (DESIGN.md §6.7, group 3): the partition primitives
-- over a version's undirected weighted view, and Louvain and Leiden over
-- them, contract, project and lift.
CREATE FUNCTION warren."modularity"(
	"graph" TEXT,
	"labels" TEXT,
	"resolution" double precision DEFAULT 1.0,
	"weighted" bool DEFAULT NULL,
	"version" bigint DEFAULT NULL
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
	"version" bigint DEFAULT NULL
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
	"version" bigint DEFAULT NULL
) RETURNS bigint
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'leiden_refine_wrapper';

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
CREATE FUNCTION warren.louvain(graph text, "out" text, resolution float8 DEFAULT 1.0,
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
  BEGIN
    PERFORM warren.vector_dense_id(graph, '_warren_louvain_single', persist := false, version := v);
    q := warren.modularity(graph, '_warren_louvain_single', resolution, weighted, v);
    LOOP
      level := level + 1;
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
-- one), or after `max_levels`. A level's local moving stops when a sweep
-- raises modularity by no more than `tolerance`. Arguments and output as
-- warren.louvain's; `randomness` is θ of the refinement. The level's
-- graph, a level graph of warren.contract's (`_warren_level_<version>`),
-- is dropped at the end.
CREATE FUNCTION warren.leiden(graph text, "out" text, resolution float8 DEFAULT 1.0,
                              seed bigint DEFAULT 0, max_levels int DEFAULT 100,
                              tolerance float8 DEFAULT 1e-7, randomness float8 DEFAULT 0.01,
                              weighted boolean DEFAULT NULL, levels_prefix text DEFAULT NULL,
                              version bigint DEFAULT NULL)
RETURNS float8 LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['moved', 'refined', 'initial', 'keys', 'down', 'refined_down'];
  g text := graph;
  gv bigint := v;
  next_g text;
  next_v bigint;
  level int := 0;
  nodes bigint;
  found bigint;
  refined bigint;
  q float8;
BEGIN
  IF "out" IS NULL OR resolution IS NULL OR seed IS NULL OR max_levels IS NULL OR tolerance IS NULL
     OR randomness IS NULL THEN
    RAISE EXCEPTION '% must not be NULL',
      CASE WHEN "out" IS NULL THEN 'out' WHEN resolution IS NULL THEN 'resolution'
           WHEN seed IS NULL THEN 'seed' WHEN max_levels IS NULL THEN 'max_levels'
           WHEN tolerance IS NULL THEN 'tolerance' ELSE 'randomness' END
      USING ERRCODE = 'null_value_not_allowed';
  END IF;
  IF starts_with("out", '_warren_') OR starts_with(levels_prefix, '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  IF max_levels < 1 THEN
    RAISE EXCEPTION 'max_levels must be at least 1, not %', max_levels USING ERRCODE = 'invalid_parameter_value';
  END IF;
  BEGIN
    LOOP
      level := level + 1;
      nodes := (SELECT (e ->> 'node_count')::bigint FROM jsonb_array_elements(warren.info(g) -> 'versions') e
                WHERE (e ->> 'version')::bigint = gv);
      IF level > 1 THEN
        PERFORM warren.vector_from_query(g, '_warren_leiden_keys',
          format('SELECT node_id, node_id FROM warren.id_map(%L, NULL::bigint, %s)', g, gv), 'i64',
          persist := false, version := gv);
      END IF;
      found := warren.local_moving(g, '_warren_leiden_moved',
                                   initial := CASE WHEN level > 1 THEN '_warren_leiden_initial' END,
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
      IF levels_prefix IS NOT NULL THEN
        PERFORM warren.vector_map(graph, format('%s_%s', levels_prefix, level), '_warren_leiden_down', true, v);
      END IF;
      EXIT WHEN found = nodes OR level >= max_levels;
      refined := warren.leiden_refine(g, '_warren_leiden_moved', '_warren_leiden_refined',
                                      keys := CASE WHEN level > 1 THEN '_warren_leiden_keys' END,
                                      resolution := resolution, seed := seed, randomness := randomness,
                                      weighted := CASE WHEN level = 1 THEN weighted ELSE true END,
                                      persist := false, version := gv);
      EXIT WHEN refined = nodes;
      IF level = 1 THEN
        PERFORM warren.vector_map(graph, '_warren_leiden_refined_down', '_warren_leiden_refined', false, v);
      ELSE
        PERFORM warren.lift(g, '_warren_leiden_refined', graph, '_warren_leiden_refined_down',
                            persist := false, version := gv);
      END IF;
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
