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
-- the graph has weights; an edge in several relations one edge, or with
-- `per_relation` an edge in each, as warren.modularity counts them).
-- Communities are labelled by the smallest dense id of their members: a
-- level's nodes are keyed by those dense ids. Writes
-- vector `out` (i64, on version `version`, NULL: the current one), with
-- each level's partition as `<levels_prefix>_<level>` if `levels_prefix`
-- is given, and returns the partition's modularity. The level's graph, a
-- level graph of warren.contract's (`_warren_level_<version>`), and the
-- intermediate vectors are dropped at the end.
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
