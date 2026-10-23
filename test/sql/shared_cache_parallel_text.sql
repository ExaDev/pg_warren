-- A text graph's PARALLEL SAFE reads in a parallel worker (DESIGN.md §5.6,
-- §5.7, §14 item 33): the worker reads the text id map as the leader does,
-- from the shared version or its own private copy, and returns the rows
-- the leader would, ids and all.
\ir include/shared_cache.sql
CREATE TABLE nodes AS SELECT 'https://example.org/node/' || n AS id, n FROM generate_series(1, 20011) n;
CREATE TABLE edges AS SELECT a.id AS src, b.id AS dst FROM nodes a JOIN nodes b ON b.n = a.n % 20011 + 1;
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges');
SELECT warren.refresh('g');
CREATE TABLE pairs AS SELECT src, dst FROM edges;
ANALYZE pairs;
-- The setting that runs a query's plan in a worker; PG 16 renamed it.
SELECT CASE WHEN current_setting('server_version_num')::int >= 160000
            THEN 'debug_parallel_query' ELSE 'force_parallel_mode' END AS force \gset

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

SELECT dblink_connect('b', :'conninfo');

CREATE TABLE reads (query text);
INSERT INTO reads VALUES
    ($$SELECT string_agg(n, ',' ORDER BY n) FROM warren.neighbours('g', 'https://example.org/node/7'::text, 'both') n$$),
    ($$SELECT warren.degree('g', 'https://example.org/node/7'::text, 'both')::text$$),
    ($$SELECT warren.has_edge('g', 'https://example.org/node/7'::text, 'https://example.org/node/8'::text)::text$$),
    ($$SELECT count(*)::text FROM warren.id_map('g', NULL::text) WHERE live$$),
    ($$SELECT warren.shortest_path('g', 'https://example.org/node/1'::text, 'https://example.org/node/6'::text)::text$$),
    ($$SELECT string_agg(b::text, ',' ORDER BY b::text) FROM warren.bfs('g', ARRAY['https://example.org/node/1'], 4) b$$),
    ($$SELECT count(*)::text FROM warren.reachable_set('g', ARRAY['https://example.org/node/1'], 1000)$$),
    ($$SELECT count(DISTINCT component_id)::text FROM warren.components('g', NULL::text)$$),
    ($$SELECT count(*)::text FROM pairs WHERE warren.has_edge('g', src, dst)$$);
SELECT assert.equal(in_worker, in_leader, query)
FROM reads,
     LATERAL dblink('b', format($$RESET %s; %s$$, :'force', query)) AS l(in_leader text),
     LATERAL dblink('b', format($$SET %s = on; %s$$, :'force', query)) AS w(in_worker text);
SELECT assert.equal(launched, 1::bigint, 'each read ran in a worker: ' || query)
FROM reads,
     LATERAL dblink('b', format($$SET %s = on; SELECT workers_launched(%L)$$, :'force', query))
         AS t(launched bigint);
SELECT assert.equal(in_leader, '20011', 'every pair is an edge')
FROM dblink('b', format($$RESET %s; SELECT count(*)::text FROM pairs WHERE warren.has_edge('g', src, dst)$$, :'force'))
     AS t(in_leader text);

SELECT dblink_disconnect('b');
