-- DESIGN.md §5.6, §14 item 38: a persisted vector shares as a version does.
-- Backends that read one load it once, into one segment that each maps,
-- and a shared vector is charged to no call's budget; a bool vector is
-- read privately. Replacing or dropping a vector evicts it, the prune
-- evicts one whose row is gone (with its version, here), and warren.evict
-- by the owner evicts the graph's shared vectors, by another role none.
-- An evicted vector a backend maps is freed once that backend releases it,
-- at its next read. Where the shared cache is off or unavailable, nothing
-- is shared.
\ir include/shared_cache.sql
CREATE TABLE nodes AS SELECT generate_series(1, 100003)::bigint AS id;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT id, id % 100003 + 1 FROM nodes');
SELECT warren.refresh('g') AS v1 \gset
SELECT warren.vector_const('g', 'x', 2) AS x \gset
SELECT warren.vector_const('g', 'flags', 1, 'bool') AS flags \gset
SELECT CASE WHEN :'shared' THEN 1 ELSE 0 END::bigint AS one \gset
SELECT CASE WHEN :'shared' THEN 2 ELSE 0 END::bigint AS two \gset
SELECT dblink_connect(session, :'conninfo') FROM unnest(ARRAY['a', 'b', 'c']) AS session;

-- a and b read x: loaded once, into one segment that both map.
SELECT assert.equal(total, 200006::float8, 'a and b read x')
FROM unnest(ARRAY['a', 'b']) AS session,
     LATERAL dblink(session, $$SELECT warren.vector_reduce('g', 'x', 'sum')$$) AS t(total float8);
SELECT assert.equal(shared_vector_segments(:x), :one, 'x is loaded once');
SELECT assert.equal(count(*), :one, 'the view lists x, mapped by a and b')
FROM warren.shared_cache()
WHERE kind = 'vector' AND vector_id = :x AND vector = 'x' AND graph = 'g' AND version = :v1
  AND state = 'ready' AND mappers = 2 AND pins = 0 AND bytes > 800024;
-- a's own cache holds g's version and x only where nothing is shared.
SELECT assert.equal(evicted, CASE WHEN :'shared' THEN 0 ELSE 2 END::bigint,
                    'a shared vector is in no backend''s own cache')
FROM dblink('a', $$SELECT own_evictions('g')$$) AS t(evicted bigint);

-- A shared vector is charged to no call: c reads x (800 kB) under a
-- 64 kB budget.
\if :shared
SELECT assert.equal(total, 200006::float8, 'a shared vector is charged to no call')
FROM dblink('c', $$SET warren.max_compute_memory = '64kB';
                   SELECT warren.vector_reduce('g', 'x', 'sum')$$) AS t(total float8);
SELECT * FROM dblink('c', $$RESET warren.max_compute_memory$$) AS t(reset text);
\endif

-- A bool vector is read privately: it cannot be read in place unchecked.
SELECT * FROM dblink('a', $$SELECT warren.vector_stats('g', 'flags')::text$$) AS t(stats text);
SELECT assert.equal(shared_vector_segments(:flags), 0::bigint, 'a bool vector is not shared');

-- Writing x again evicts the old x: it drains while b and c map it (a
-- released its mapping in own_evictions), and is freed once they release
-- it at their next read, which shares the new one.
SELECT warren.vector_const('g', 'x', 3) AS x2 \gset
SELECT assert.equal(shared_vector_segments(:x), :one, 'the replaced x drains while mapped');
SELECT assert.equal(total, 300009::float8, 'a, b and c read the new x')
FROM unnest(ARRAY['a', 'b', 'c']) AS session,
     LATERAL dblink(session, $$SELECT warren.vector_reduce('g', 'x', 'sum')$$) AS t(total float8);
SELECT assert.equal(shared_vector_segments(:x), 0::bigint, 'the replaced x is freed');
SELECT assert.equal(shared_vector_segments(:x2), :one, 'the new x is shared');

-- Dropping x evicts it; it is freed once a, b and c release it.
SELECT warren.drop_vector('g', 'x');
SELECT t.* FROM unnest(ARRAY['a', 'b', 'c']) AS session,
     LATERAL dblink(session, $$SELECT warren.degree('g', 1::bigint)$$) AS t(degree bigint);
SELECT assert.equal(shared_vector_segments(:x2), 0::bigint, 'a dropped vector is freed');

-- z, bound to v1, is shared; v1 is dropped (and z with it) after a
-- promotion: c's next cache miss prunes z, and a releases it at its next
-- read.
SELECT warren.vector_const('g', 'z', 1) AS z \gset
SELECT * FROM dblink('a', $$SELECT warren.vector_reduce('g', 'z', 'sum')$$) AS t(total float8);
SELECT assert.equal(shared_vector_segments(:z), :one, 'z is shared');
INSERT INTO nodes VALUES (100004);
SELECT warren.refresh('g') AS v2 \gset
SELECT warren.drop_version('g', :v1);
SELECT * FROM dblink('c', $$SELECT warren.degree('g', 1::bigint)$$) AS t(degree bigint);
SELECT * FROM dblink('a', $$SELECT warren.degree('g', 1::bigint)$$) AS t(degree bigint);
SELECT assert.equal(shared_vector_segments(:z), 0::bigint, 'a vector whose row is gone is evicted');

-- warren.evict: a role that does not own g evicts no shared vector; the
-- owner evicts g's shared version and vector that no statement pins.
SELECT warren.vector_const('g', 'w', 1) AS w \gset
SELECT * FROM dblink('a', $$SELECT warren.vector_reduce('g', 'w', 'sum')$$) AS t(total float8);
SELECT * FROM dblink('b', $$SELECT own_evictions('g')$$) AS t(evicted bigint);
SELECT assert.equal(shared_vector_segments(:w), :one, 'w stays shared');
SELECT assert.equal(evicted, :two, 'the owner evicts g''s shared version and vector')
FROM dblink('b', $$SELECT warren.evict('g')$$) AS t(evicted bigint);
SELECT * FROM dblink('a', $$SELECT warren.degree('g', 1::bigint)$$) AS t(degree bigint);
SELECT assert.equal(shared_vector_segments(:w), 0::bigint, 'freed once its mappers release it');

SELECT dblink_disconnect(session) FROM unnest(ARRAY['a', 'b', 'c']) AS session;
