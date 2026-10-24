-- DESIGN.md §5.6, §14 item 34: shared versions against promotion, drop and
-- warren.evict. A promoted version is read by every backend's next
-- statement (the key is the version, so nothing stale is served), and its
-- load evicts the graph's older shared versions; a dropped version is
-- evicted by the next cache miss's prune; warren.evict by a role that does
-- not own the graph evicts no shared version, and by the owner evicts
-- those no statement pins. An evicted version a backend maps is freed once
-- that backend releases it, at its next read. Where the shared cache is
-- off or unavailable, nothing is shared.
\ir include/shared_cache.sql
CREATE TABLE nodes AS SELECT generate_series(1, 100003)::bigint AS id;
CREATE TABLE edges AS SELECT id AS src, id % 100003 + 1 AS dst FROM nodes;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.create_graph('h', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('g') AS v1 \gset
SELECT warren.refresh('h') AS h1 \gset
SELECT CASE WHEN :'shared' THEN 1 ELSE 0 END::bigint AS one \gset
SELECT dblink_connect(session, :'conninfo') FROM unnest(ARRAY['a', 'b', 'c', 'd']) AS session;

-- a and b read g's current version, and keep it mapped.
SELECT assert.equal(degree, 1::bigint, 'a and b read v1')
FROM unnest(ARRAY['a', 'b']) AS session,
     LATERAL dblink(session, $$SELECT warren.degree('g', 1::bigint)$$) AS t(degree bigint);
SELECT assert.equal(shared_segments(:v1), :one, 'v1 is shared');

-- A refresh promotes v2, with one more edge out of node 1: a's next
-- statement reads it, and its load evicts v1 and releases a's mapping of
-- it; b still maps v1, so v1 is freed once b releases it, at its next read.
INSERT INTO edges VALUES (1, 3);
SELECT warren.refresh('g') AS v2 \gset
SELECT assert.equal(degree, 2::bigint, 'a reads the promoted v2, not v1')
FROM dblink('a', $$SELECT warren.degree('g', 1::bigint)$$) AS t(degree bigint);
SELECT assert.equal(shared_segments(:v2), :one, 'v2 is shared');
SELECT assert.equal(shared_segments(:v1), :one, 'v1 drains while b maps it');
SELECT assert.equal(degree, 2::bigint, 'b reads v2 too')
FROM dblink('b', $$SELECT warren.degree('g', 1::bigint)$$) AS t(degree bigint);
SELECT assert.equal(shared_segments(:v1), 0::bigint, 'v1 is freed once b releases it');

-- b reads v1 on purpose (it is retained), then v1 is dropped: c's next
-- cache miss (h's version, which no one has read) prunes it, and b releases it at its next read.
SELECT * FROM dblink('b', format($$SELECT warren.prewarm('g', %s)$$, :v1)) AS t(version bigint);
SELECT assert.equal(shared_segments(:v1), :one, 'v1 is shared again');
SELECT warren.drop_version('g', :v1);
SELECT * FROM dblink('c', $$SELECT warren.degree('h', 1::bigint)$$) AS t(degree bigint);
SELECT * FROM dblink('b', $$SELECT warren.degree('g', 1::bigint)$$) AS t(degree bigint);
SELECT assert.equal(unnamed_shared_bytes(), :unnamed, 'a dropped version is evicted');

-- warren.evict: a role that does not own g evicts no shared version.
SELECT assert.equal(evicted, 0::bigint, 'c owns nothing of g''s to free')
FROM dblink('c', $$SELECT own_evictions('g')$$) AS t(evicted bigint);
SELECT assert.equal(shared_segments(:v2), :one, 'v2 stays shared');

-- The owner does not evict a version a statement pins (d's, here)...
SELECT dblink_send_query('d', $$SELECT warren.degree('g', 1::bigint), pg_sleep(1)$$);
SELECT pg_sleep(0.3);
SELECT assert.equal(evicted, 0::bigint, 'a pinned version is not evicted')
FROM dblink('c', $$SELECT warren.evict('g')$$) AS t(evicted bigint);
SELECT count(*) FROM dblink_get_result('d') AS t(degree bigint, slept text);
SELECT count(*) FROM dblink_get_result('d') AS t(degree bigint, slept text);

-- ...and evicts it once no statement pins it, counting it.
SELECT assert.equal(evicted, :one, 'the owner evicts g''s shared version')
FROM dblink('c', $$SELECT warren.evict('g')$$) AS t(evicted bigint);
SELECT t.* FROM unnest(ARRAY['a', 'b', 'd']) AS session,
     LATERAL dblink(session, $$SELECT warren.degree('h', 1::bigint)$$) AS t(degree bigint);
SELECT assert.equal(shared_segments(:v2), 0::bigint, 'freed once its mappers release it');

SELECT dblink_disconnect(session) FROM unnest(ARRAY['a', 'b', 'c', 'd']) AS session;
