-- The packages' smoke test (packaging/check-deb.sh, check-image.sh):
-- README's ten-line example, checked.
\set ON_ERROR_STOP 1
CREATE EXTENSION pg_warren;
SELECT warren.version();
CREATE TABLE people  (id bigint PRIMARY KEY, name text NOT NULL);
CREATE TABLE follows (src bigint NOT NULL, dst bigint NOT NULL);
INSERT INTO people  VALUES (1, 'ada'), (2, 'bob'), (3, 'cy'), (4, 'dee'), (5, 'eve');
INSERT INTO follows VALUES (1, 2), (2, 3), (3, 1), (3, 4), (4, 5);
SELECT warren.create_graph('social', 'SELECT id FROM people', 'SELECT src, dst FROM follows');
SELECT warren.refresh('social');
DO $$
BEGIN
  IF (SELECT string_agg(p.name || ' ' || b.depth, ', ' ORDER BY b.depth, p.name)
      FROM warren.bfs('social', ARRAY[1::bigint], 3) b JOIN people p ON p.id = b.node_id)
     IS DISTINCT FROM 'ada 0, bob 1, cy 2, dee 3' THEN
    RAISE EXCEPTION 'bfs from ada is wrong';
  END IF;
END $$;
SELECT warren.pagerank('social', 'pr');
DO $$
BEGIN
  IF (SELECT string_agg(p.name, ', ' ORDER BY round(r.value::numeric, 3) DESC, p.name)
      FROM warren.vector_topk('social', 'pr', 3, NULL::bigint) r JOIN people p ON p.id = r.node_id)
     IS DISTINCT FROM 'cy, bob, eve' THEN
    RAISE EXCEPTION 'pagerank''s top three are wrong';
  END IF;
END $$;
