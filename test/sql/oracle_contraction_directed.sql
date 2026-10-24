-- warren.contract of a directed graph with two relations against
-- NetworkX's quotient graph (DESIGN.md §6.6, §12), counting edges
-- (weighted := false): each edge u → v counts once towards the edge from
-- u's community to v's, and an edge in both relations counts once, in the
-- first (a), so the all-relations quotient is NetworkX's of the graph of
-- distinct edges, as relation a's is (b's edges are all in a). 17
-- communities by a hash of the id.
-- fixture: random --nodes 250 --edges 1200 --directed --seed 3
-- oracle: quotient --count --directed --labels-sql 'SELECT id, (id * 104729) % 17 FROM nodes' --edges-sql 'SELECT src, dst FROM edges UNION ALL SELECT src, dst FROM edges WHERE (src + dst) % 3 = 0'
CREATE TABLE g_edges AS
SELECT src, dst, 'a'::text AS rel FROM edges
UNION ALL SELECT src, dst, 'b' FROM edges WHERE (src + dst) % 3 = 0;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst, rel FROM g_edges',
                           has_relations := true);
SELECT warren.refresh('g');
SELECT warren.vector_from_query('g', 'label', 'SELECT id, (id * 104729) % 17 FROM nodes');
SELECT warren.contract('g', 'label', 'q', weighted := false);
-- Each column of the quotient's A, over relation a and over both.
CREATE TABLE quotient_edges (rels text, source bigint, target bigint, weight float8);
DO $$
DECLARE
  d bigint;
BEGIN
  FOR d IN SELECT node_id FROM warren.id_map('q', NULL::bigint) WHERE live LOOP
    PERFORM warren.vector_from_query('q', 'indicator', format('SELECT %s::bigint, 1', d));
    PERFORM warren.spmv('q', 'indicator', 'a', rels := ARRAY['a']);
    PERFORM warren.spmv('q', 'indicator', 'all');
    INSERT INTO quotient_edges
    SELECT name, node_id, d, value FROM unnest(ARRAY['a', 'all']) name,
           LATERAL warren.vector_values('q', name, NULL::bigint) WHERE value <> 0;
  END LOOP;
END $$;
SELECT assert.equal((SELECT count(*) FROM quotient_edges WHERE rels = 'a'), (SELECT count(*) FROM oracle_quotient),
                    'relation a''s quotient has NetworkX''s edges');
SELECT assert.equal((SELECT count(*) FROM quotient_edges f JOIN oracle_quotient o USING (source, target)
                     WHERE f.rels = 'a' AND f.weight = o.weight), (SELECT count(*) FROM oracle_quotient),
                    'each counts the edges it joins, as NetworkX''s');
-- Over both relations, an edge in both counts once.
SELECT assert.equal((SELECT count(*) FROM quotient_edges WHERE rels = 'all'), (SELECT count(*) FROM oracle_quotient),
                    'the all-relations quotient has NetworkX''s edges');
SELECT assert.equal((SELECT count(*) FROM quotient_edges f JOIN oracle_quotient o USING (source, target)
                     WHERE f.rels = 'all' AND f.weight = o.weight), (SELECT count(*) FROM oracle_quotient),
                    'each counts the distinct edges it joins');
