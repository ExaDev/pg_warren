-- Text node ids end to end (DESIGN.md §5.7): every function that maps ids,
-- on a text graph and on the same graph with bigint ids. The text id of
-- node n is an IRI ending in n zero-padded to six digits, so the text
-- graph's bytewise order is the bigint graph's numeric order, both give
-- each node the same dense id, and every result must be the same once its
-- ids are read back as numbers.
-- fixture: random --nodes 300 --edges 1500 --directed --seed 11

CREATE FUNCTION t(n bigint) RETURNS text IMMUTABLE LANGUAGE sql
RETURN 'https://example.org/node/' || lpad(n::text, 6, '0');
CREATE FUNCTION n(id text) RETURNS bigint IMMUTABLE LANGUAGE sql RETURN right(id, 6)::bigint;
CREATE FUNCTION n(ids text[]) RETURNS bigint[] IMMUTABLE STRICT LANGUAGE sql
RETURN ARRAY(SELECT n(id) FROM unnest(ids) WITH ORDINALITY u(id, at) ORDER BY at);

CREATE TABLE rel_edges AS
SELECT src, dst, CASE WHEN (src + dst) % 3 = 0 THEN 'b' ELSE 'a' END AS rel,
       ((src * 7 + dst) % 5 + 1)::float8 AS w
FROM edges;
CREATE TABLE text_nodes AS SELECT t(id) AS id FROM nodes;
CREATE TABLE text_edges AS SELECT t(src) AS src, t(dst)::varchar AS dst, rel, w FROM rel_edges;

SELECT warren.create_graph('gi', 'SELECT id FROM nodes', 'SELECT src, dst, rel, w FROM rel_edges',
                           weighted := true, has_relations := true);
SELECT warren.create_graph('gt', 'SELECT id FROM text_nodes', 'SELECT src, dst, rel, w FROM text_edges',
                           weighted := true, has_relations := true);
SELECT warren.create_graph('ui', 'SELECT id FROM nodes', 'SELECT src, dst, w FROM rel_edges',
                           directed := false, weighted := true);
SELECT warren.create_graph('ut', 'SELECT id FROM text_nodes', 'SELECT src, dst, w FROM text_edges',
                           directed := false, weighted := true);
SELECT warren.refresh(g) FROM unnest(ARRAY['gi', 'gt', 'ui', 'ut']) g;

SELECT assert.equal(warren.info('gt') ->> 'id_type', 'text', 'a varchar edge column is read as text');

-- Whether two queries return the same rows, in any order.
CREATE FUNCTION same(a text, b text) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE
  differing bigint;
  rows bigint;
BEGIN
  EXECUTE format('SELECT count(*) FROM (((%1$s) EXCEPT ALL (%2$s)) UNION ALL ((%2$s) EXCEPT ALL (%1$s))) d',
                 a, b) INTO differing;
  EXECUTE format('SELECT count(*) FROM (%s) r', a) INTO rows;
  IF rows = 0 THEN
    RAISE EXCEPTION 'no rows to compare: %', a;
  END IF;
  RETURN differing = 0;
END $$;

-- Identity and adjacency.
SELECT assert.that(same($$SELECT node_id, dense_id, live FROM warren.id_map('gi', NULL::bigint)$$,
                        $$SELECT n(node_id), dense_id, live FROM warren.id_map('gt', NULL::text)$$),
                   'the id maps give each node the same dense id');
SELECT assert.that(same($$SELECT id, warren.neighbours('gi', id) FROM nodes WHERE id % 7 = 0$$,
                        $$SELECT id, n(warren.neighbours('gt', t(id))) FROM nodes WHERE id % 7 = 0$$),
                   'neighbours');
SELECT assert.that(same($$SELECT id, warren.degree('gi', id, 'both', ARRAY['a']) FROM nodes$$,
                        $$SELECT id, warren.degree('gt', t(id), 'both', ARRAY['a']) FROM nodes$$),
                   'degree');
SELECT assert.that(same($$SELECT src, dst, warren.has_edge('gi', src, dst + 1, 'b') FROM rel_edges$$,
                        $$SELECT src, dst, warren.has_edge('gt', t(src), t(dst + 1), 'b') FROM rel_edges$$),
                   'has_edge');

-- Traversal.
SELECT assert.that(same($$SELECT node_id, depth, via FROM warren.bfs('gi', ARRAY[1, 2]::bigint[], 3)$$,
                        $$SELECT n(node_id), depth, n(via) FROM warren.bfs('gt', ARRAY[t(1), t(2)], 3)$$),
                   'bfs');
SELECT assert.that(same($$SELECT node_id, depth FROM warren.bfs('gi', ARRAY[1]::bigint[], 4,
                                                                node_filter := ARRAY[1, 3, 5, 7, 9]::bigint[])$$,
                        $$SELECT n(node_id), depth FROM warren.bfs('gt', ARRAY[t(1)], 4,
                                                                  node_filter := ARRAY[t(1), t(3), t(5), t(7), t(9)])$$),
                   'bfs with a node filter');
SELECT assert.that(same($$SELECT warren.reachable_set('gi', ARRAY[5]::bigint[], 2, direction := 'in')$$,
                        $$SELECT n(warren.reachable_set('gt', ARRAY[t(5)], 2, direction := 'in'))$$),
                   'reachable_set');
SELECT assert.that(same($$SELECT id, warren.reachable('gi', 0::bigint, id, 3) FROM nodes$$,
                        $$SELECT id, warren.reachable('gt', t(0), t(id), 3) FROM nodes$$),
                   'reachable');
SELECT assert.that(same($$SELECT id, warren.shortest_path('gi', 0::bigint, id) FROM nodes$$,
                        $$SELECT id, n(warren.shortest_path('gt', t(0), t(id))) FROM nodes$$),
                   'shortest_path');
SELECT assert.that(same($$SELECT id, d.path, d.cost FROM nodes, warren.dijkstra('gi', 3::bigint, id) d$$,
                        $$SELECT id, n(d.path), d.cost FROM nodes, warren.dijkstra('gt', t(3), t(id)) d$$),
                   'dijkstra');
SELECT assert.that(same(
  $$SELECT end_id, path, rels, count FROM warren.walk('gi', ARRAY[0, 1]::bigint[],
      ARRAY[warren.s(ARRAY['a']), warren.s(NULL, 'both', ARRAY(SELECT id FROM nodes WHERE id % 2 = 0))], 'paths')$$,
  $$SELECT n(end_id), n(path), rels, count FROM warren.walk('gt', ARRAY[t(0), t(1)],
      ARRAY[warren.s(ARRAY['a']), warren.s(NULL, 'both', ARRAY(SELECT t(id) FROM nodes WHERE id % 2 = 0))], 'paths')$$),
  'walk with a step filter');
SELECT assert.that(same($$SELECT source, target, weight FROM warren.minimum_spanning_forest('ui', NULL::bigint)$$,
                        $$SELECT n(source), n(target), weight FROM warren.minimum_spanning_forest('ut', NULL::text)$$),
                   'minimum_spanning_forest');

-- Vectors keyed by external id.
SELECT warren.vector_from_query('gi', 'x', 'SELECT id, (id % 17)::float8 FROM nodes');
SELECT warren.vector_from_query('gt', 'x', 'SELECT t(id), (id % 17)::float8 FROM nodes');
SELECT warren.vector_from_query('gi', 'm', 'SELECT id, ARRAY[id % 3, id % 5]::float8[] FROM nodes');
SELECT warren.vector_from_query('gt', 'm', 'SELECT t(id)::varchar, ARRAY[id % 3, id % 5]::float8[] FROM nodes');
SELECT assert.that(same($$SELECT * FROM warren.vector_values('gi', 'x', NULL::bigint)$$,
                        $$SELECT n(node_id), value FROM warren.vector_values('gt', 'x', NULL::text)$$),
                   'vector_from_query and vector_values');
SELECT assert.that(same($$SELECT * FROM warren.matrix_rows('gi', 'm', NULL::bigint)$$,
                        $$SELECT n(node_id), "row" FROM warren.matrix_rows('gt', 'm', NULL::text)$$),
                   'a matrix from a query with varchar ids, and matrix_rows');
SELECT warren.pagerank(g, 'pr', seed := 'x') FROM unnest(ARRAY['gi', 'gt']) g;
SELECT assert.that(same($$SELECT * FROM warren.vector_topk('gi', 'pr', 20, NULL::bigint)$$,
                        $$SELECT n(node_id), value FROM warren.vector_topk('gt', 'pr', 20, NULL::text)$$),
                   'pagerank with a seed vector, and vector_topk');
SELECT warren.sssp_vector('gi', 'sssp', 4::bigint);
SELECT warren.sssp_vector('gt', 'sssp', t(4));
SELECT assert.that(same($$SELECT * FROM warren.vector_values('gi', 'sssp', NULL::bigint)$$,
                        $$SELECT n(node_id), value FROM warren.vector_values('gt', 'sssp', NULL::text)$$),
                   'sssp_vector');
SELECT warren.vector_from_query(g, 'few', q)
FROM (VALUES ('gi', 'SELECT id, 1 FROM nodes WHERE id < 40'), ('gt', 'SELECT t(id), 1 FROM nodes WHERE id < 40')) v(g, q);
SELECT warren.betweenness_centrality(g, 'bc', sources := 'few') FROM unnest(ARRAY['gi', 'gt']) g;
SELECT assert.that(same($$SELECT * FROM warren.vector_values('gi', 'bc', NULL::bigint)$$,
                        $$SELECT n(node_id), value FROM warren.vector_values('gt', 'bc', NULL::text)$$),
                   'betweenness_centrality from a vector of sources');
SELECT assert.that(same($$SELECT * FROM warren.components('ui', NULL::bigint)$$,
                        $$SELECT n(node_id), component_id FROM warren.components('ut', NULL::text)$$),
                   'components');
SELECT warren.strong_components_vector(g, 'scc') FROM unnest(ARRAY['gi', 'gt']) g;
SELECT assert.that(same($$SELECT * FROM warren.vector_values('gi', 'scc', NULL::bigint)$$,
                        $$SELECT n(node_id), value FROM warren.vector_values('gt', 'scc', NULL::text)$$),
                   'strong_components_vector');

-- Messages and similarity, read by external id.
CREATE TABLE msgs AS SELECT id, k, 'm' || id || '.' || k AS body FROM nodes, generate_series(1, 2) k WHERE id % 3 > 0;
SELECT assert.that(same(
  $$SELECT node_id, value FROM warren.gather('gi', 'SELECT id, body FROM msgs ORDER BY k', NULL::bigint, NULL::text[])$$,
  $$SELECT n(node_id), value FROM warren.gather('gt', 'SELECT t(id), body FROM msgs ORDER BY k', NULL::text, NULL::text[])$$),
  'gather');
SELECT assert.that(same(
  $$SELECT * FROM warren.similarity('gi', 'SELECT src, dst FROM rel_edges', NULL::bigint)$$,
  $$SELECT n(source), n(target), score FROM warren.similarity('gt', 'SELECT src, dst FROM text_edges', NULL::text)$$),
  'similarity of pairs from a query, one column varchar');
SELECT assert.that(same($$SELECT * FROM warren.most_similar('gi', 9::bigint, 5)$$,
                        $$SELECT n(node_id), score FROM warren.most_similar('gt', t(9), 5)$$),
                   'most_similar');
SELECT assert.that(same($$SELECT * FROM warren.similar_pairs('gi', NULL::bigint, 3, nodes := 'few')$$,
                        $$SELECT n(source), n(target), score FROM warren.similar_pairs('gt', NULL::text, 3, nodes := 'few')$$),
                   'similar_pairs over a vector of nodes');

-- Communities: a text graph contracts into a bigint quotient.
SELECT warren.louvain(g, 'louvain') FROM unnest(ARRAY['ui', 'ut']) g;
SELECT assert.that(same($$SELECT * FROM warren.vector_values('ui', 'louvain', NULL::bigint)$$,
                        $$SELECT n(node_id), value FROM warren.vector_values('ut', 'louvain', NULL::text)$$),
                   'louvain');
SELECT assert.equal(warren.modularity('ut', 'louvain'), warren.modularity('ui', 'louvain'), 'modularity');
SELECT warren.leiden(g, 'leiden', seed := 3) FROM unnest(ARRAY['ui', 'ut']) g;
SELECT assert.that(same($$SELECT * FROM warren.vector_values('ui', 'leiden', NULL::bigint)$$,
                        $$SELECT n(node_id), value FROM warren.vector_values('ut', 'leiden', NULL::text)$$),
                   'leiden');
SELECT warren.vector_from_query(g, 'x', q)
FROM (VALUES ('ui', 'SELECT id, id::float8 FROM nodes'), ('ut', 'SELECT t(id), id::float8 FROM nodes')) v(g, q);
SELECT warren.contract(g, 'louvain', q) FROM (VALUES ('ui', 'qi'), ('ut', 'qt')) v(g, q);
SELECT assert.equal(warren.info('qt') ->> 'id_type', 'bigint', 'the quotient of a text graph has bigint ids');
SELECT warren.project(g, 'x', q, 'sum') FROM (VALUES ('ui', 'qi'), ('ut', 'qt')) v(g, q);
SELECT assert.that(same($$SELECT * FROM warren.vector_values('qi', 'sum', NULL::bigint)$$,
                        $$SELECT * FROM warren.vector_values('qt', 'sum', NULL::bigint)$$),
                   'project');
SELECT warren.lift(q, 'sum', g, 'lifted') FROM (VALUES ('ui', 'qi'), ('ut', 'qt')) v(g, q);
SELECT assert.that(same($$SELECT * FROM warren.vector_values('ui', 'lifted', NULL::bigint)$$,
                        $$SELECT n(node_id), value FROM warren.vector_values('ut', 'lifted', NULL::text)$$),
                   'lift');

-- Versions: a refresh with new and dropped ids, the diff, a compaction.
SELECT current_version AS i1 FROM warren.graphs WHERE name = 'ui' \gset
SELECT current_version AS t1 FROM warren.graphs WHERE name = 'ut' \gset
DELETE FROM nodes WHERE id % 10 = 0;
INSERT INTO nodes SELECT id FROM generate_series(1000, 1010) id;
INSERT INTO rel_edges SELECT id, id - 999, 'a', 1 FROM generate_series(1000, 1010) id;
TRUNCATE text_nodes, text_edges;
INSERT INTO text_nodes SELECT t(id) FROM nodes;
INSERT INTO text_edges SELECT t(src), t(dst), rel, w FROM rel_edges;
SELECT warren.refresh('ui') AS i2 \gset
SELECT warren.refresh('ut') AS t2 \gset
SELECT assert.equal(warren.diff('ut', :t1, :t2) - 'from' - 'to', warren.diff('ui', :i1, :i2) - 'from' - 'to', 'diff');
SELECT assert.that(same($$SELECT node_id, dense_id, live FROM warren.id_map('ui', NULL::bigint)$$,
                        $$SELECT n(node_id), dense_id, live FROM warren.id_map('ut', NULL::text)$$),
                   'a refresh keeps dense ids and appends new ids alike');
SELECT assert.equal((SELECT array_agg(warren.distance('ut', :t1, :t2, m) - 'from' - 'to') FROM unnest(ARRAY['edit', 'degree']) m),
                    (SELECT array_agg(warren.distance('ui', :i1, :i2, m) - 'from' - 'to') FROM unnest(ARRAY['edit', 'degree']) m),
                    'distance between versions');
SELECT warren.compact(g) FROM unnest(ARRAY['ui', 'ut']) g;
SELECT assert.that(same($$SELECT node_id, dense_id, live FROM warren.id_map('ui', NULL::bigint)$$,
                        $$SELECT n(node_id), dense_id, live FROM warren.id_map('ut', NULL::text)$$),
                   'a compaction renumbers alike');
SELECT assert.that(same($$SELECT id, warren.neighbours('ui', id) FROM nodes$$,
                        $$SELECT id, n(warren.neighbours('ut', t(id))) FROM nodes$$),
                   'neighbours after the compaction');

-- Ids are bytes, compared bytewise: an empty id and ids differing only in
-- case are distinct nodes; a literal is typed as text.
CREATE TABLE odd_nodes (id text);
INSERT INTO odd_nodes VALUES (''), ('a'), ('A'), ('é'), ('a b');
SELECT warren.create_graph('odd', 'SELECT id FROM odd_nodes', $$SELECT '', 'é' UNION ALL SELECT 'a', 'A'$$);
SELECT warren.refresh('odd');
SELECT assert.equal((SELECT string_agg(node_id || '=' || dense_id, ',' ORDER BY dense_id)
                     FROM warren.id_map('odd', NULL::text)),
                    '=0,A=1,a=2,a b=3,é=4', 'dense ids follow bytewise order');
SELECT assert.equal(ARRAY(SELECT warren.neighbours('odd', 'a'::text)), ARRAY['A'], 'neighbours of a text literal');
SELECT assert.equal(ARRAY(SELECT warren.neighbours('odd', ''::text)), ARRAY['é'], 'the empty id is a node');
SELECT assert.equal(warren.degree('odd', 'a'::varchar), 1::bigint, 'a varchar argument where no id is returned');
SELECT assert.raises($$SELECT warren.neighbours('odd', 'a'::varchar)$$, 'a varchar argument typing returned ids', '42804');
SELECT assert.raises($$SELECT warren.neighbours('odd', 'a')$$, 'an untyped literal names no id type', '42804');
SELECT assert.raises($$SELECT warren.neighbours('odd', 1::bigint)$$, 'a bigint id on a text graph', '42804');
SELECT assert.raises($$SELECT warren.id_map('odd', NULL::varchar)$$, 'a varchar witness is not text', '42804');

-- Refused sources.
SELECT assert.raises($$SELECT warren.create_graph('bad', 'SELECT id::bpchar FROM odd_nodes', 'SELECT NULL::text, NULL::text')$$,
                     'bpchar ids', '42804');
SELECT assert.raises($$SELECT warren.create_graph('bad', 'SELECT id::name FROM odd_nodes', 'SELECT NULL::text, NULL::text')$$,
                     'name ids', '42804');
SELECT assert.raises($$SELECT warren.create_graph('bad', 'SELECT id FROM odd_nodes', 'SELECT id, id::bigint FROM nodes')$$,
                     'edges of another id type', '42804');

-- A nondeterministic collation would make equal ids of different bytes.
SELECT EXISTS (SELECT FROM pg_collation WHERE collprovider = 'i') AS icu \gset
\if :icu
CREATE COLLATION case_insensitive (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
SELECT assert.raises($$SELECT warren.create_graph('bad', 'SELECT id COLLATE case_insensitive FROM odd_nodes',
                                                  'SELECT NULL::text, NULL::text')$$,
                     'a node id of a nondeterministic collation', '42804');
SELECT assert.raises($$SELECT warren.create_graph('bad', 'SELECT id FROM odd_nodes',
                                                  'SELECT id COLLATE case_insensitive, id FROM odd_nodes')$$,
                     'an edge endpoint of a nondeterministic collation', '42804');
SELECT warren.create_graph('folded', 'SELECT lower(id COLLATE case_insensitive) COLLATE "C" FROM odd_nodes',
                           'SELECT NULL::text, NULL::text');
SELECT assert.that(warren.refresh('folded') IS NOT NULL, 'a deterministic collation over folded ids builds');
\endif
