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
