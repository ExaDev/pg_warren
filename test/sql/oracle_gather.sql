-- Oracle for warren.gather (DESIGN.md §6.4, §12): for every combiner, both
-- message kinds (fixed-width numbers and booleans; text and jsonb in their
-- binary form), every direction, relation subsets and masks, on graphs with
-- and without a reverse CSR, gather equals the SQL join of the distinct
-- (sender, receiver) edge pairs with the messages, aggregated per receiver
-- in collect order (ascending sender dense id, then message order).
-- fixture: random --nodes 200 --edges 900 --directed --seed 3

-- Two relations; some edges are in both, which still makes their sender a
-- sender once.
CREATE TABLE rel_edges AS
SELECT src, dst, CASE WHEN (src + dst) % 3 = 0 THEN 'b' ELSE 'a' END AS rel FROM edges
UNION ALL SELECT src, dst, 'b' FROM edges WHERE (src * dst) % 5 = 0 AND (src + dst) % 3 <> 0;

-- Messages: none from some nodes, several from others, NULLs, unknown ids.
CREATE TABLE msgs AS
SELECT id, k AS ord, (id * 7 + k) % 23 - 11 AS n, (((id + k) % 9) / 2.0 - 2)::float8 AS f,
       (id + k) % 4 = 0 AS b, 'm' || id || '.' || k AS t, jsonb_build_object('id', id, 'k', k) AS j
FROM nodes, generate_series(1, 3) k
WHERE k <= id % 4;
INSERT INTO msgs VALUES (5, 9, NULL, NULL, NULL, NULL, NULL), (100000, 1, 1, 1, true, 'x', '{}');

SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst, rel FROM rel_edges',
                           has_relations := true);
SELECT warren.refresh('g');
SELECT warren.create_graph('g_forward', 'SELECT id FROM nodes', 'SELECT src, dst, rel FROM rel_edges',
                           has_relations := true, build_reverse := false);
SELECT warren.refresh('g_forward');
SELECT warren.vector_from_query(graph, 'mask', 'SELECT id, (id % 3 <> 0)::int FROM nodes')
FROM (VALUES ('g'), ('g_forward')) v(graph);

CREATE TABLE dense AS
SELECT graph, node_id, dense_id FROM (VALUES ('g'), ('g_forward')) v(graph), warren.id_map(graph, NULL::bigint);

-- The differences between gather and its SQL oracle for one case.
CREATE FUNCTION differences(graph text, combine text, column_name text, value_type text, aggregate text,
                            direction text, rels text[], masked boolean, complement boolean)
RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
  pairs text;
  found bigint;
BEGIN
  pairs := format($q$SELECT DISTINCT r, s FROM (
                       SELECT dst AS r, src AS s FROM rel_edges WHERE %1$L <> 'out' AND (%2$L::text[] IS NULL OR rel = ANY (%2$L))
                       UNION ALL
                       SELECT src, dst FROM rel_edges WHERE %1$L <> 'in' AND (%2$L::text[] IS NULL OR rel = ANY (%2$L))) e
                     WHERE NOT %3$L OR ((r %% 3 <> 0) <> %4$L)$q$, direction, rels, masked, complement);
  EXECUTE format($q$
    WITH oracle AS (
      SELECT p.r AS node_id, (%1$s)::text AS value
      FROM (%2$s) p JOIN msgs m ON m.id = p.s AND m.%3$I IS NOT NULL JOIN dense d ON d.graph = %4$L AND d.node_id = p.s
      GROUP BY p.r),
    gathered AS (
      SELECT node_id, value::text FROM warren.gather(%4$L, 'SELECT id, %3$I FROM msgs ORDER BY ord',
                                                    NULL::bigint, NULL::%5$s, %6$L, %7$L, %8$L,
                                                    CASE WHEN %9$L THEN 'mask' END, %10$L))
    SELECT count(*) FROM ((TABLE oracle EXCEPT ALL TABLE gathered) UNION ALL (TABLE gathered EXCEPT ALL TABLE oracle)) x$q$,
    aggregate, pairs, column_name, graph, value_type, combine, direction, rels, masked, complement)
  INTO found;
  RETURN found;
END $$;

CREATE TABLE cases (combine text, column_name text, value_type text, aggregate text);
INSERT INTO cases VALUES
  ('collect', 'n', 'bigint[]', 'array_agg(m.n ORDER BY d.dense_id, m.ord)'),
  ('count', 'n', 'bigint', 'count(*)'),
  ('sum', 'n', 'bigint', 'sum(m.n)'),
  ('min', 'n', 'bigint', 'min(m.n)'),
  ('max', 'n', 'bigint', 'max(m.n)'),
  ('first', 'n', 'bigint', '(array_agg(m.n ORDER BY d.dense_id, m.ord))[1]'),
  ('collect', 'f', 'double precision[]', 'array_agg(m.f ORDER BY d.dense_id, m.ord)'),
  ('sum', 'f', 'double precision', 'sum(m.f)'),
  ('max', 'f', 'double precision', 'max(m.f)'),
  ('bool_or', 'b', 'boolean', 'bool_or(m.b)'),
  ('bool_and', 'b', 'boolean', 'bool_and(m.b)'),
  ('collect', 'b', 'boolean[]', 'array_agg(m.b ORDER BY d.dense_id, m.ord)'),
  ('collect', 't', 'text[]', 'array_agg(m.t ORDER BY d.dense_id, m.ord)'),
  ('concat', 't', 'text', 'string_agg(m.t, '''' ORDER BY d.dense_id, m.ord)'),
  ('first', 't', 'text', '(array_agg(m.t ORDER BY d.dense_id, m.ord))[1]'),
  ('count', 't', 'bigint', 'count(*)'),
  ('collect', 'j', 'jsonb[]', 'array_agg(m.j ORDER BY d.dense_id, m.ord)'),
  ('first', 'j', 'jsonb', '(array_agg(m.j ORDER BY d.dense_id, m.ord))[1]');

-- The f messages are halves, so float sums are exact in any order.
CREATE TABLE results AS
SELECT graph, c.combine, c.column_name, direction, rels, masked, complement,
       differences(graph, c.combine, c.column_name, c.value_type, c.aggregate, direction, rels, masked, complement) AS differences
FROM cases c,
     (VALUES ('g'), ('g_forward')) g(graph),
     (VALUES ('in'), ('out'), ('both')) dir(direction),
     (VALUES (NULL::text[]), (ARRAY['b']), (ARRAY['a', 'b', 'nope'])) r(rels),
     (VALUES (false, false), (true, false), (true, true)) m(masked, complement);

SELECT assert.equal((SELECT count(*) FROM results), 18 * 2 * 3 * 3 * 3::bigint, 'every case ran');
SELECT assert.equal((SELECT string_agg(format('%s %s(%s) %s %s mask=%s/%s: %s', graph, combine, column_name, direction,
                                              rels, masked, complement, differences), '; ')
                     FROM results WHERE differences <> 0), NULL, 'gather equals the SQL oracle');
