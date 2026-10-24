-- DESIGN.md §5.6: backends that miss the same committed version at once
-- load it once, into one shared segment, and a backend that reads it later
-- maps that copy; none of them holds it in its own cache. Where the shared
-- cache is off or unavailable (test/run.py --shared-cache off, or on
-- without the preload on PG 15 and 16), each backend loads its own copy
-- and no segment is created. The segments are counted, and the registry
-- read, through warren.shared_cache().
\ir include/shared_cache.sql
CREATE TABLE nodes AS SELECT generate_series(1, 200003)::bigint AS id;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT id, id % 200003 + 1 FROM nodes');
SELECT warren.refresh('g') AS version \gset
SELECT shared_segments(:version) AS before \gset
SELECT assert.equal(:before, 0::bigint, 'no segment holds the new version');

SELECT dblink_connect(session, :'conninfo') FROM unnest(ARRAY['a', 'b', 'c', 'd']) AS session;
SELECT dblink_send_query(session, $$SELECT warren.prewarm('g')$$)
FROM unnest(ARRAY['a', 'b', 'c', 'd']) AS session;
SELECT assert.equal(prewarmed, :version::bigint, 'each backend reads the version')
FROM unnest(ARRAY['a', 'b', 'c', 'd']) AS session,
     LATERAL dblink_get_result(session) AS t(prewarmed bigint);
-- Each async query ends with an empty result.
SELECT count(*) FROM unnest(ARRAY['a', 'b', 'c', 'd']) AS session,
     LATERAL dblink_get_result(session) AS t(prewarmed bigint);
SELECT assert.equal(shared_segments(:version), CASE WHEN :'shared' THEN 1 ELSE 0 END::bigint,
                    'concurrent misses load the version once');
SELECT assert.equal((SELECT string_agg(format('%s %s %s', state, pins, mappers), ', ')
                     FROM warren.shared_cache() WHERE kind = 'version' AND version = :version),
                    CASE WHEN :'shared' THEN 'ready 0 4' END,
                    'the registry holds it once, mapped by each backend, pinned by none');
SELECT assert.equal(evicted, CASE WHEN :'shared' THEN 0 ELSE 1 END::bigint,
                    'a shared version is in no backend''s own cache')
FROM unnest(ARRAY['a', 'b', 'c', 'd']) AS session,
     LATERAL dblink(session, $$SELECT own_evictions('g')$$) AS t(evicted bigint);

-- Another backend, and this one, read the shared copy.
SELECT dblink_connect('e', :'conninfo');
SELECT assert.equal(degree, 2::bigint, 'a later backend reads the version')
FROM dblink('e', $$SELECT warren.degree('g', 7::bigint, direction := 'both')$$) AS t(degree bigint);
SELECT assert.equal(warren.degree('g', 200003::bigint, direction := 'both'), 2::bigint,
                    'and so does this one');
SELECT assert.equal(shared_segments(:version), CASE WHEN :'shared' THEN 1 ELSE 0 END::bigint,
                    'later reads attach the shared copy');
SELECT dblink_disconnect(session) FROM unnest(ARRAY['a', 'b', 'c', 'd', 'e']) AS session;
