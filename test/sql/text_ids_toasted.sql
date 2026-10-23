-- Text ids stored out of line (DESIGN.md §5.7): a build reads ids of over
-- 2 kB from the node and the edge query, compressed and not, detoasting
-- each in the rows' memory context, and maps them as any other id.

-- Node n's id: 3,000 bytes, compressible (EXTENDED) or not (EXTERNAL,
-- random bytes in hex).
CREATE TABLE compressed (n int, id text);
INSERT INTO compressed SELECT n, repeat(md5(n::text), 94) FROM generate_series(1, 300) n;
CREATE TABLE external (n int, id text);
ALTER TABLE external ALTER COLUMN id SET STORAGE EXTERNAL;
INSERT INTO external
SELECT n, string_agg(md5(n::text || ':' || k), '' ORDER BY k) FROM generate_series(1, 300) n,
       generate_series(1, 94) k GROUP BY n;

SELECT assert.that((SELECT bool_and(length(id) > 2048) FROM compressed)
                   AND (SELECT bool_and(length(id) > 2048) FROM external), 'every id is over 2 kB');
SELECT assert.that(pg_column_compression(id) IS NOT NULL, 'compressed ids are compressed')
FROM compressed LIMIT 1;
SELECT assert.that(pg_relation_size(reltoastrelid) > 0, 'external ids are stored out of line')
FROM pg_class WHERE relname = 'external';

-- A path 1 → 2 → … → 300 over each table's ids.
CREATE FUNCTION path_query(ids text) RETURNS text LANGUAGE sql
RETURN format('SELECT a.id, b.id FROM %1$I a JOIN %1$I b ON b.n = a.n + 1', ids);
SELECT warren.create_graph(t, format('SELECT id FROM %I', t), path_query(t))
FROM unnest(ARRAY['compressed', 'external']) t;
SELECT warren.refresh(t) FROM unnest(ARRAY['compressed', 'external']) t;

SELECT assert.equal((warren.info('compressed') #>> '{versions,0,edge_count}')::bigint, 299::bigint, 'compressed: every edge');
SELECT assert.equal((warren.info('external') #>> '{versions,0,edge_count}')::bigint, 299::bigint, 'external: every edge');
SELECT assert.equal(
  (SELECT array_agg(n ORDER BY n) FROM warren.neighbours('compressed', (SELECT id FROM compressed WHERE n = 7), 'both') x
   JOIN compressed c ON c.id = x),
  ARRAY[6, 8], 'compressed ids map as any other');
SELECT assert.equal(
  (SELECT array_agg(n ORDER BY n) FROM warren.neighbours('external', (SELECT id FROM external WHERE n = 7), 'both') x
   JOIN external e ON e.id = x),
  ARRAY[6, 8], 'external ids map as any other');
