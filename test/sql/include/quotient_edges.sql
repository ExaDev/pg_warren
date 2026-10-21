-- The edges of graph :'quotient' (current version) as rows of table
-- quotient_edges (source, target, weight): for each live node d, one
-- product with the indicator vector of d gives column d of A.
DROP TABLE IF EXISTS quotient_edges;
CREATE TABLE quotient_edges (source bigint, target bigint, weight float8);
DO $$
DECLARE
  d bigint;
BEGIN
  FOR d IN SELECT node_id FROM warren.id_map(current_setting('test.quotient'), NULL::bigint) WHERE live LOOP
    PERFORM warren.vector_from_query(current_setting('test.quotient'), 'indicator',
                                     format('SELECT %s::bigint, 1', d));
    PERFORM warren.spmv(current_setting('test.quotient'), 'indicator', 'column');
    INSERT INTO quotient_edges
    SELECT node_id, d, value FROM warren.vector_values(current_setting('test.quotient'), 'column', NULL::bigint)
    WHERE value <> 0;
  END LOOP;
END $$;
