-- DESIGN.md §5.6, §14 item 33: the graph reads that use no vector are
-- PARALLEL SAFE. A parallel worker reads a graph as another backend would:
-- it finds a shared version in the registry and maps it, or loads it into
-- the shared cache if no backend has, or, where the shared cache is off or
-- unavailable, loads it privately. Either way it returns the rows the
-- leader would; its pins and mappings end with it; and a traversal it runs
-- records nothing in last_stats(), which reports only the leader's calls.
\ir include/shared_cache.sql
CREATE TABLE nodes AS SELECT generate_series(1, 100003)::bigint AS id;
CREATE TABLE edges AS SELECT id AS src, id % 100003 + 1 AS dst FROM nodes;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('g') AS v1 \gset
SELECT CASE WHEN :'shared' THEN 1 ELSE 0 END::bigint AS one \gset
-- The setting that runs a query's plan in a worker; PG 16 renamed it.
SELECT CASE WHEN current_setting('server_version_num')::int >= 160000
            THEN 'debug_parallel_query' ELSE 'force_parallel_mode' END AS force \gset
CREATE TABLE pairs AS SELECT g AS src, g % 100003 + 1 AS dst FROM generate_series(1, 20000) g;
ANALYZE pairs;

-- The workers a statement's plan launched.
CREATE FUNCTION workers_launched(statement text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    plan json;
BEGIN
    EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, FORMAT JSON) ' || statement INTO plan;
    RETURN (SELECT coalesce(sum((m[1])::bigint), 0)
            FROM regexp_matches(plan::text, '"Workers Launched": (\d+)', 'g') AS m);
END
$$;

SELECT dblink_connect(session, :'conninfo') FROM unnest(ARRAY['a', 'b']) AS session;
SELECT assert.equal(shared_segments(:v1), 0::bigint, 'no backend has read v1 yet');

-- a's leader has not read g: its worker loads v1, into the shared cache
-- where there is one, and gives its pin and mapping back as it exits.
SELECT assert.equal(launched, 1::bigint, 'degree runs in a worker')
FROM dblink('a', format($$SET %s = on;
                         SELECT workers_launched('SELECT warren.degree(''g'', 1::bigint)')$$, :'force'))
     AS t(launched bigint);
SELECT assert.equal(shared_segments(:v1), :one, 'the worker loaded v1 into the shared cache');
SELECT assert.equal(CASE WHEN state IS NOT NULL THEN format('%s %s %s', state, pins, mappers) END,
                    CASE WHEN :'shared' THEN 'ready 0 0' END,
                    'the worker gave back its pin and its mapping as it exited')
FROM (SELECT NULL) AS nothing
LEFT JOIN warren.shared_cache() ON kind = 'version' AND version = :v1;

-- Each read returns in a worker the rows it returns in the leader.
CREATE TABLE reads (query text);
INSERT INTO reads VALUES
    ($$SELECT string_agg(n::text, ',' ORDER BY n) FROM warren.neighbours('g', 7::bigint, 'both') n$$),
    ($$SELECT warren.degree('g', 7::bigint, 'both')::text$$),
    ($$SELECT warren.has_edge('g', 7::bigint, 8::bigint)::text$$),
    ($$SELECT count(*)::text FROM warren.id_map('g', NULL::bigint) WHERE live$$),
    ($$SELECT (warren.info('g') -> 'current_version')::text$$),
    ($$SELECT warren.reachable('g', 1::bigint, 50::bigint)::text$$),
    ($$SELECT warren.shortest_path('g', 1::bigint, 6::bigint)::text$$),
    ($$SELECT string_agg(d::text, ',') FROM warren.dijkstra('g', 1::bigint, 6::bigint) d$$),
    ($$SELECT string_agg(b::text, ',' ORDER BY b) FROM warren.bfs('g', ARRAY[1::bigint], 4) b$$),
    ($$SELECT count(*)::text FROM warren.reachable_set('g', ARRAY[1::bigint], 1000)$$),
    ($$SELECT count(DISTINCT component_id)::text FROM warren.components('g', NULL::bigint)$$),
    ($$SELECT string_agg(w::text, ',' ORDER BY w)
      FROM warren.walk('g', ARRAY[1::bigint], ARRAY[warren.s(NULL), warren.s(NULL)]) w$$),
    ($$SELECT count(*)::text FROM pairs WHERE warren.has_edge('g', src, dst)$$);
SELECT assert.equal(in_worker, in_leader, query)
FROM reads,
     LATERAL dblink('b', format($$RESET %s; %s$$, :'force', query)) AS l(in_leader text),
     LATERAL dblink('b', format($$SET %s = on; %s$$, :'force', query)) AS w(in_worker text);
SELECT assert.equal(launched, 1::bigint, 'each read ran in a worker: ' || query)
FROM reads,
     LATERAL dblink('b', format($$SET %s = on; SELECT workers_launched(%L)$$, :'force', query))
         AS t(launched bigint);
SELECT dblink_exec('b', format('RESET %s', :'force'));

-- A parallel scan calls has_edge per row in its workers, as the leader
-- would serially.
SELECT assert.equal(n, 20000::bigint, 'every pair is an edge, counted in parallel')
FROM dblink('b', $$SET parallel_setup_cost = 0; SET parallel_tuple_cost = 0;
                   SET min_parallel_table_scan_size = 0; SET max_parallel_workers_per_gather = 2;
                   SELECT count(*) FROM pairs WHERE warren.has_edge('g', src, dst)$$) AS t(n bigint);
SELECT assert.that(launched >= 1, 'the scan ran in workers')
FROM dblink('b', $$SELECT workers_launched('SELECT count(*) FROM pairs WHERE warren.has_edge(''g'', src, dst)')$$)
     AS t(launched bigint);

-- A traversal a worker runs records nothing: last_stats() still
-- describes the leader's last call.
SELECT assert.equal(stats, 'reachable', 'a worker''s bfs is not the leader''s last call')
FROM dblink('a', format($$RESET %1$s; SELECT warren.reachable('g', 1::bigint, 3::bigint);
                          SET %1$s = on; SELECT count(*) FROM warren.bfs('g', ARRAY[1::bigint], 3);
                          RESET %1$s; SELECT warren.last_stats() ->> 'function'$$, :'force'))
     AS t(stats text);

SELECT dblink_disconnect(session) FROM unnest(ARRAY['a', 'b']) AS session;
