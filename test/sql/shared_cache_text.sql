-- DESIGN.md §5.6, §5.7: a text version in the shared cache. Backends read
-- one text graph's version through the shared region (its id bytes,
-- offsets and order, mapped in place), answer as the backend that built
-- it does, map more ids than a call maps by binary search (so through the
-- call's own index), and read a refresh's new ids at their next statement.
-- Where the shared cache is off or unavailable, nothing is shared and the
-- answers are the same.
\ir include/shared_cache.sql
CREATE FUNCTION iri(n bigint) RETURNS text IMMUTABLE LANGUAGE sql RETURN 'https://example.org/node/' || n;
CREATE TABLE nodes AS SELECT iri(n) AS id FROM generate_series(1, 70001) n;
CREATE TABLE edges AS SELECT iri(n) AS src, iri(n % 70001 + 1) AS dst FROM generate_series(1, 70001) n;
SELECT warren.create_graph('t', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('t') AS v1 \gset
SELECT CASE WHEN :'shared' THEN 1 ELSE 0 END::bigint AS one \gset
SELECT dblink_connect(session, :'conninfo') FROM unnest(ARRAY['a', 'b']) AS session;

SELECT assert.equal(n, iri(2), 'a and b read a text id')
FROM unnest(ARRAY['a', 'b']) AS session,
     LATERAL dblink(session, $$SELECT warren.neighbours('t', 'https://example.org/node/1'::text)$$) AS t(n text);
SELECT assert.equal(shared_segments(:v1), :one, 'the text version is shared');
SELECT assert.equal(c, 70001::bigint, 'every id reads back through the shared id map')
FROM dblink('a', $$SELECT count(*) FROM warren.id_map('t', NULL::text) m JOIN nodes n ON n.id = m.node_id
                   WHERE m.live$$) AS t(c bigint);

-- A vector from a query of every id: past the ids a call maps by binary
-- search, so through an index of the shared id map.
SELECT * FROM dblink('b', $$SELECT warren.vector_from_query('t', 'n', 'SELECT id, length(id)::float8 FROM nodes',
                                                            persist := false)$$) AS t(version bigint);
SELECT assert.equal(s, (SELECT sum(length(id))::float8 FROM nodes), 'each id''s value lands on its node')
FROM dblink('b', $$SELECT sum(x.value) FROM warren.vector_values('t', 'n', NULL::text) x
                   JOIN nodes n ON n.id = x.node_id AND length(n.id) = x.value$$) AS t(s float8);

-- A refresh adds an id: the next statement of each backend reads it.
INSERT INTO nodes VALUES ('é');
INSERT INTO edges VALUES (iri(1), 'é');
SELECT warren.refresh('t') AS v2 \gset
SELECT assert.equal(ns, ARRAY[iri(2), 'é'], 'a and b read the new id')
FROM unnest(ARRAY['a', 'b']) AS session,
     LATERAL dblink(session, $$SELECT array_agg(n ORDER BY n) FROM warren.neighbours('t', 'https://example.org/node/1'::text) n$$)
             AS t(ns text[]);
SELECT assert.equal(shared_segments(:v2), :one, 'the refreshed text version is shared');
SELECT dblink_disconnect(session) FROM unnest(ARRAY['a', 'b']) AS session;
