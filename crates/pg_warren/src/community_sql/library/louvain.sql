-- warren.louvain (DESIGN.md §6.7): Louvain community detection (Blondel
-- et al. 2008) as a library over the primitives. Each level moves nodes
-- between communities (warren.local_moving, from each node alone), then
-- contracts the level's graph by the communities found (warren.contract)
-- into the next level's graph, whose nodes are the communities. It stops
-- when a level merges no node, when a level raises modularity by no more
-- than `tolerance` (NetworkX's `threshold`), or after `max_levels`. The
-- graph is undirected for this (an edge and its reverse add up; edges
-- weighted if `weighted`, NULL: if the graph has weights). Communities are
-- labelled by the smallest dense id of their members: each level labels
-- its communities by the least label of their members, which are those
-- dense ids. Writes vector `out` (i64, on version `version`, NULL: the
-- current one), with each level's partition as `<levels_prefix>_<level>`
-- if `levels_prefix` is given, and returns the partition's modularity.
-- The levels' graphs, named `_warren_louvain_<backend pid>_<level>`, and
-- the intermediate vectors are dropped at the end.
CREATE FUNCTION warren.louvain(graph text, "out" text, resolution float8 DEFAULT 1.0,
                               seed bigint DEFAULT 0, max_levels int DEFAULT 100,
                               tolerance float8 DEFAULT 1e-7, weighted boolean DEFAULT NULL,
                               levels_prefix text DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS float8 LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
  v bigint := coalesce(version, (warren.info(graph) ->> 'current_version')::bigint);
  parts text[] := ARRAY['single', 'labels', 'keys', 'down'];
  levels text[] := ARRAY[graph];
  versions bigint[] := ARRAY[v];
  g text := graph;
  gv bigint := v;
  level int := 0;
  nodes bigint;
  found bigint;
  q float8;
  q_next float8;
  i int;
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
        -- A level's nodes are the communities of the level below, whose
        -- ids are the least dense id of their members in `graph`.
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
      -- This level's partition, lifted down to `graph`.
      PERFORM warren.vector_map(g, '_warren_louvain_down', '_warren_louvain_labels', false, gv);
      FOR i IN REVERSE level - 1 .. 1 LOOP
        PERFORM warren.lift(levels[i + 1], '_warren_louvain_down', levels[i], '_warren_louvain_down',
                            persist := false, version := versions[i + 1]);
      END LOOP;
      IF levels_prefix IS NOT NULL THEN
        PERFORM warren.vector_map(graph, format('%s_%s', levels_prefix, level), '_warren_louvain_down', true, v);
      END IF;
      EXIT WHEN found = nodes OR level >= max_levels OR q_next - q <= tolerance;
      q := q_next;
      levels := levels || format('_warren_louvain_%s_%s', pg_backend_pid(), level);
      versions := versions || warren.contract(g, '_warren_louvain_labels', levels[level + 1],
                                              weighted := CASE WHEN level = 1 THEN coalesce(weighted, true) ELSE true END,
                                              version := gv);
      g := levels[level + 1];
      gv := versions[level + 1];
    END LOOP;
    PERFORM warren.vector_map(graph, "out", '_warren_louvain_down', true, v);
    q := warren.modularity(graph, "out", resolution, weighted, v);
  EXCEPTION WHEN OTHERS OR query_canceled THEN
    PERFORM warren.drop_vector(graph, '_warren_louvain_' || part, v, true) FROM unnest(parts) part;
    RAISE;
  END;
  FOR i IN 1 .. array_length(levels, 1) LOOP
    PERFORM warren.drop_vector(levels[i], '_warren_louvain_' || part, versions[i], true) FROM unnest(parts) part;
  END LOOP;
  PERFORM warren.drop_graph(l) FROM unnest(levels[2:]) l;
  RETURN q;
END $$;
