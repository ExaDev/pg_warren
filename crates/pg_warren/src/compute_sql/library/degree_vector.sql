-- warren.degree_vector (DESIGN.md §6.7): each node's degree in `direction`
-- over `rels`, as vector `out` (f64); returns its vector id.
CREATE FUNCTION warren.degree_vector(graph text, "out" text, direction text DEFAULT 'out',
                                     rels text[] DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS bigint LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
  IF starts_with("out", '_warren_') THEN
    RAISE EXCEPTION 'vector names beginning with _warren_ are reserved for the library''s intermediates'
      USING ERRCODE = 'reserved_name';
  END IF;
  RETURN warren.vector_degree(graph, "out", direction, rels, 'f64', true, version);
END $$;
