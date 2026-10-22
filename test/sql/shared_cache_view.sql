-- DESIGN.md §14 item 37: warren.shared_cache() lists the shared versions
-- and persisted vectors of this database's graphs that the caller may
-- read (their state, bytes, pins, mappers and last use), then one cluster
-- row of every slot's bytes, pins and mappers against the cap. A role
-- sees only the graphs it may read, and every role sees the totals. Where
-- the shared cache is off or unavailable, only the totals row, with
-- nothing held.
\ir include/shared_cache.sql
CREATE TABLE nodes AS SELECT generate_series(1, 100003)::bigint AS id;
CREATE TABLE edges AS SELECT id AS src, id % 100003 + 1 AS dst FROM nodes;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.create_graph('h', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('g') AS g1 \gset
SELECT warren.refresh('h') AS h1 \gset

-- Nothing is shared yet: only the totals row, whose cap is the setting.
SELECT assert.equal(count(*), 0::bigint, 'nothing of this database is shared yet')
FROM warren.shared_cache() WHERE kind <> 'cluster';
SELECT assert.equal(count(*), 1::bigint, 'one totals row')
FROM warren.shared_cache() WHERE kind = 'cluster' AND graph IS NULL AND state IS NULL;
SELECT assert.equal(cap, pg_size_bytes(current_setting('warren.shared_cache_memory')),
                    'the cap in bytes')
FROM warren.shared_cache() WHERE kind = 'cluster';

-- Writing x reads g's version, shared from then on.
SELECT warren.vector_const('g', 'x', 1) AS x \gset
DO $$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = 'warren_viewer') THEN
        CREATE ROLE warren_viewer;
    END IF;
END
$$;
SELECT warren.grant('g', 'warren_viewer');
GRANT USAGE ON SCHEMA assert TO warren_viewer, warren_stranger;
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA assert TO warren_viewer, warren_stranger;
SELECT dblink_connect(session, :'conninfo') FROM unnest(ARRAY['a', 'b']) AS session;

-- a reads g's and h's versions and x; b reads g's version. This backend
-- maps g's version too: writing x read it.
SELECT * FROM dblink('a', $$SELECT warren.degree('g', 1::bigint) + warren.degree('h', 1::bigint)
                             + warren.vector_reduce('g', 'x', 'sum')::bigint$$) AS t(n bigint);
SELECT * FROM dblink('b', $$SELECT warren.degree('g', 1::bigint)$$) AS t(n bigint);
SELECT assert.equal(
    (SELECT string_agg(format('%s %s %s %s %s %s', kind, graph, version, coalesce(vector, '-'),
                              state, mappers), ', ' ORDER BY graph, kind)
     FROM warren.shared_cache() WHERE kind <> 'cluster'),
    CASE WHEN :'shared'
         THEN format('vector g %s x ready 1, version g %s - ready 3, version h %s - ready 1',
                     :g1, :g1, :h1) END,
    'the superuser sees every shared version and vector of this database');
SELECT assert.equal(vector_id, :x::bigint, 'a vector''s id')
FROM warren.shared_cache() WHERE kind = 'vector';
SELECT assert.that(bytes > 100003 * 8, 'a version''s bytes')
FROM warren.shared_cache() WHERE kind = 'version';
SELECT assert.equal(c.bytes >= e.bytes AND e.bytes > 0, :'shared'::boolean,
                    'the totals count at least this database''s entries')
FROM (SELECT bytes FROM warren.shared_cache() WHERE kind = 'cluster') AS c,
     (SELECT coalesce(sum(bytes), 0) AS bytes FROM warren.shared_cache()
      WHERE kind <> 'cluster') AS e;
SELECT bytes AS cluster_bytes FROM warren.shared_cache() WHERE kind = 'cluster' \gset

-- A reader of g sees g's version and vector, not h's; a role that reads
-- neither sees none of them; both see the totals.
SET ROLE warren_viewer;
SELECT assert.equal(string_agg(graph || ' ' || kind, ', ' ORDER BY kind),
                    CASE WHEN :'shared' THEN 'g vector, g version' END,
                    'a reader of g sees g''s entries only')
FROM warren.shared_cache() WHERE kind <> 'cluster';
SELECT assert.equal(bytes, :cluster_bytes::bigint, 'and the totals')
FROM warren.shared_cache() WHERE kind = 'cluster';
RESET ROLE;
SET ROLE warren_stranger;
SELECT assert.equal(count(*), 0::bigint, 'a role that reads no graph sees no entry')
FROM warren.shared_cache() WHERE kind <> 'cluster';
SELECT assert.equal(bytes, :cluster_bytes::bigint, 'but sees the totals')
FROM warren.shared_cache() WHERE kind = 'cluster';
RESET ROLE;

-- A statement running in b pins g's version, and its pin is a use, later
-- on the registry's clock.
\if :shared
SELECT last_use AS used FROM warren.shared_cache() WHERE kind = 'version' AND graph = 'g' \gset
SELECT dblink_send_query('b', $$SELECT warren.degree('g', 1::bigint), pg_sleep(1)$$);
SELECT pg_sleep(0.3);
SELECT assert.equal(pins, 1, 'b''s statement pins g''s version')
FROM warren.shared_cache() WHERE kind = 'version' AND graph = 'g';
SELECT assert.that(last_use > :used, 'a pin is a use')
FROM warren.shared_cache() WHERE kind = 'version' AND graph = 'g';
SELECT count(*) FROM dblink_get_result('b') AS t(degree bigint, slept text);
SELECT count(*) FROM dblink_get_result('b') AS t(degree bigint, slept text);
SELECT assert.equal(pins, 0, 'its pin ended with the statement')
FROM warren.shared_cache() WHERE kind = 'version' AND graph = 'g';
\endif

-- Evicted by the owner (which releases this backend's mapping), they
-- drain until a and b release them.
SELECT warren.evict('g');
SELECT assert.equal(string_agg(state, ', ' ORDER BY kind),
                    CASE WHEN :'shared' THEN 'draining, draining' END,
                    'mapped, evicted versions and vectors drain')
FROM warren.shared_cache() WHERE graph = 'g';
SELECT t.* FROM unnest(ARRAY['a', 'b']) AS session,
     LATERAL dblink(session, $$SELECT warren.degree('h', 1::bigint)$$) AS t(degree bigint);
SELECT assert.equal(count(*), 0::bigint, 'and are gone once released')
FROM warren.shared_cache() WHERE graph = 'g';

SELECT dblink_disconnect(session) FROM unnest(ARRAY['a', 'b']) AS session;
