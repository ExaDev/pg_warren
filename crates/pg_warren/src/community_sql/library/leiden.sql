-- warren.leiden (DESIGN.md §6.7): Leiden community detection (Traag,
-- Waltman and van Eck 2019) as a library over the primitives. Each level
-- moves nodes between communities (warren.local_moving, from the partition
-- the level below found), refines each community into well-connected parts
-- (warren.leiden_refine), and contracts the level's graph by the refined
-- partition (warren.contract); the next level starts from the unrefined
-- one, projected onto it (warren.project, min). Every community found is
-- connected. As Traag et al.'s, it goes on while there is something to
-- aggregate: it stops when a level's local moving leaves every node alone,
-- when the refinement leaves every node alone (the next level's graph
-- would be this one), or after `max_levels`. A level's local moving stops
-- when a sweep raises modularity by no more than `tolerance`. Arguments
-- and output as warren.louvain's; `randomness` is θ of the refinement. The levels' graphs are named
-- `_warren_leiden_<backend pid>_<level>` and dropped at the end.
CREATE FUNCTION warren.leiden(graph text, "out" text, resolution float8 DEFAULT 1.0,
                              seed bigint DEFAULT 0, max_levels int DEFAULT 100,
                              tolerance float8 DEFAULT 1e-7, randomness float8 DEFAULT 0.01,
                              weighted boolean DEFAULT NULL, levels_prefix text DEFAULT NULL,
                              version bigint DEFAULT NULL)
RETURNS float8 LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['moved', 'refined', 'initial', 'keys', 'down'];
  levels text[] := ARRAY[graph];
  versions bigint[] := ARRAY[v];
  g text := graph;
  gv bigint := v;
  level int := 0;
  nodes bigint;
  found bigint;
  refined bigint;
  q float8;
  i int;
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
      PERFORM warren.vector_map(g, '_warren_leiden_down', '_warren_leiden_moved', false, gv);
      FOR i IN REVERSE level - 1 .. 1 LOOP
        PERFORM warren.lift(levels[i + 1], '_warren_leiden_down', levels[i], '_warren_leiden_down',
                            persist := false, version := versions[i + 1]);
      END LOOP;
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
      levels := levels || format('_warren_leiden_%s_%s', pg_backend_pid(), level);
      versions := versions || warren.contract(g, '_warren_leiden_refined', levels[level + 1],
                                              weighted := CASE WHEN level = 1 THEN coalesce(weighted, true) ELSE true END,
                                              version := gv);
      -- The next level starts from the unrefined partition: every member
      -- of a refined community is in the same community, so min is it.
      PERFORM warren.project(g, '_warren_leiden_moved', levels[level + 1], '_warren_leiden_initial', 'min',
                             persist := false, version := versions[level + 1]);
      g := levels[level + 1];
      gv := versions[level + 1];
    END LOOP;
    PERFORM warren.vector_map(graph, "out", '_warren_leiden_down', true, v);
    q := warren.modularity(graph, "out", resolution, weighted, v);
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_leiden_' || part, v, true) FROM unnest(parts) part;
    RAISE;
  END;
  FOR i IN 1 .. array_length(levels, 1) LOOP
    PERFORM warren.drop_vector(levels[i], '_warren_leiden_' || part, versions[i], true) FROM unnest(parts) part;
  END LOOP;
  PERFORM warren.drop_graph(l) FROM unnest(levels[2:]) l;
  RETURN q;
END $$;
