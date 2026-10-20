-- The SQL examples of README.md and docs/*.md, run as written, with the
-- results the docs state asserted. Each block names the doc section it
-- mirrors: change the example in both places.

------------------------------------------------------------------------
-- README.md, "A ten-line example"
------------------------------------------------------------------------
SELECT warren.version();                                           -- README "Install"
CREATE TABLE people  (id bigint PRIMARY KEY, name text NOT NULL);
CREATE TABLE follows (src bigint NOT NULL, dst bigint NOT NULL);
INSERT INTO people  VALUES (1, 'ada'), (2, 'bob'), (3, 'cy'), (4, 'dee'), (5, 'eve');
INSERT INTO follows VALUES (1, 2), (2, 3), (3, 1), (3, 4), (4, 5);

SELECT warren.create_graph('social', 'SELECT id FROM people', 'SELECT src, dst FROM follows');
SELECT warren.refresh('social');                                   -- builds version 1

SELECT p.name, b.depth                                             -- who ada reaches in 3 hops
FROM warren.bfs('social', ARRAY[1::bigint], 3) b
JOIN people p ON p.id = b.node_id ORDER BY b.depth, p.name;

SELECT warren.pagerank('social', 'pr');                            -- writes vector 'pr'
SELECT p.name, round(r.value::numeric, 3) AS rank
FROM warren.vector_topk('social', 'pr', 3, NULL::bigint) r
JOIN people p ON p.id = r.node_id ORDER BY rank DESC, p.name;

SELECT assert.equal((SELECT array_agg(p.name || ':' || b.depth ORDER BY b.depth, p.name)
                     FROM warren.bfs('social', ARRAY[1::bigint], 3) b JOIN people p ON p.id = b.node_id),
                    ARRAY['ada:0', 'bob:1', 'cy:2', 'dee:3'], 'README bfs');
SELECT assert.equal((SELECT array_agg(p.name ORDER BY r.value DESC, p.name)
                     FROM warren.vector_topk('social', 'pr', 3, NULL::bigint) r JOIN people p ON p.id = r.node_id),
                    ARRAY['cy', 'bob', 'eve'], 'README pagerank top 3');
SELECT assert.that(abs(warren.vector_reduce('social', 'pr', 'sum') - 1) < 1e-9, 'pagerank sums to 1');

------------------------------------------------------------------------
-- docs/semantics.md, "Versions"
------------------------------------------------------------------------
SELECT warren.refresh('social', promote := false, label := 'trial') AS staged \gset
SELECT assert.equal((warren.info('social') ->> 'current_version')::bigint, 1::bigint, 'staged is not current');
SELECT assert.equal((SELECT count(*) FROM warren.bfs('social', ARRAY[1::bigint], 3, version := :staged)),
                    4::bigint, 'a staged version is queryable by explicit version');
SELECT warren.promote('social', :staged);
SELECT assert.equal((warren.info('social') ->> 'current_version')::bigint, :staged::bigint, 'promoted');

SELECT version, seq, staged, current, label, node_count, edge_count, vectors
FROM jsonb_to_recordset(warren.info('social') -> 'versions')
     AS v(version bigint, seq bigint, staged bool, current bool, label text,
          node_count bigint, edge_count bigint, vectors int);

------------------------------------------------------------------------
-- docs/semantics.md, "Retention"
------------------------------------------------------------------------
-- retain_versions = 2: the current version and the newest other promoted
-- one stay. Version 1 holds vector 'pr', so it is kept too, with a NOTICE.
SELECT warren.refresh('social') AS third \gset
SELECT assert.equal((SELECT array_agg(v ORDER BY v) FROM jsonb_path_query(warren.info('social'), '$.versions[*].version') v),
                    ARRAY['1', :'staged', :'third']::jsonb[], 'version 1 kept for its vector');
SELECT warren.drop_vector('social', 'pr', version := 1);
SELECT warren.refresh('social') AS fourth \gset
SELECT assert.equal((SELECT array_agg(v ORDER BY v) FROM jsonb_path_query(warren.info('social'), '$.versions[*].version') v),
                    ARRAY[:'third', :'fourth']::jsonb[], 'retention deletes unpinned old versions');

------------------------------------------------------------------------
-- docs/semantics.md, "Stable dense ids and compaction"
------------------------------------------------------------------------
DELETE FROM follows WHERE dst = 5;
DELETE FROM people WHERE id = 5;
SELECT warren.refresh('social');
SELECT * FROM warren.id_map('social', NULL::bigint) ORDER BY dense_id;
SELECT assert.equal((SELECT array_agg(node_id || ':' || dense_id || ':' || live ORDER BY dense_id)
                     FROM warren.id_map('social', NULL::bigint)),
                    ARRAY['1:0:true', '2:1:true', '3:2:true', '4:3:true', '5:4:false'], 'eve is tombstoned');
INSERT INTO people VALUES (6, 'fay');
INSERT INTO follows VALUES (4, 6);
SELECT warren.refresh('social');
SELECT assert.equal((SELECT dense_id FROM warren.id_map('social', NULL::bigint) WHERE node_id = 6),
                    5::bigint, 'a new node is appended after the tombstone');
SELECT warren.pagerank('social', 'pr');
SELECT warren.compact('social') AS compacted \gset
SELECT * FROM warren.id_map('social', NULL::bigint) ORDER BY dense_id;
SELECT assert.equal((SELECT array_agg(node_id || ':' || dense_id ORDER BY dense_id)
                     FROM warren.id_map('social', NULL::bigint)),
                    ARRAY['1:0', '2:1', '3:2', '4:3', '6:4'], 'compaction renumbers live nodes');
SELECT assert.that(abs(warren.vector_reduce('social', 'pr', 'sum') - 1) < 1e-9, 'pr moved to the compacted version');

------------------------------------------------------------------------
-- docs/semantics.md, "Snapshots and staleness"
------------------------------------------------------------------------
INSERT INTO people VALUES (7, 'gus');
INSERT INTO follows VALUES (6, 7);
SELECT count(*) FROM warren.bfs('social', ARRAY[1::bigint], 10);    -- 5: gus is not in the version yet
SELECT assert.equal((SELECT count(*) FROM warren.bfs('social', ARRAY[1::bigint], 10)), 5::bigint, 'snapshot');
SELECT warren.info('social') ->> 'staleness' AS staleness,
       warren.info('social') #>> '{versions,-1,built_lsn}' AS built_lsn,
       pg_current_wal_lsn() AS now_lsn;
SELECT assert.that((warren.info('social') ->> 'staleness')::interval >= '0', 'staleness is an interval');
SELECT warren.refresh('social');
SELECT assert.equal((SELECT count(*) FROM warren.bfs('social', ARRAY[1::bigint], 10)), 6::bigint, 'after refresh');

------------------------------------------------------------------------
-- docs/semantics.md, "Transactions"
------------------------------------------------------------------------
SELECT (warren.info('social') ->> 'current_version')::bigint AS before_txn \gset
BEGIN;
INSERT INTO people VALUES (8, 'hal');
INSERT INTO follows VALUES (7, 8);
SELECT warren.refresh('social', promote := false) AS trial \gset
SELECT count(*) FROM warren.bfs('social', ARRAY[1::bigint], 10, version := :trial);  -- 7: sees hal
SELECT assert.equal((SELECT count(*) FROM warren.bfs('social', ARRAY[1::bigint], 10, version := :trial)),
                    7::bigint, 'refresh sees uncommitted writes');
ROLLBACK;
SELECT assert.raises(format($$SELECT warren.bfs('social', ARRAY[1::bigint], 1, version := %s)$$, :trial),
                     'the rolled-back version is gone');
SELECT assert.equal((warren.info('social') ->> 'current_version')::bigint, :before_txn::bigint, 'unchanged');

------------------------------------------------------------------------
-- docs/semantics.md, "Access"
------------------------------------------------------------------------
CREATE ROLE analyst;
CREATE ROLE stranger;
GRANT USAGE ON SCHEMA assert TO analyst, stranger;  -- test plumbing
SET ROLE stranger;
SELECT assert.raises($$SELECT warren.degree('social', 1)$$, 'not a reader', '42501');
RESET ROLE;
SELECT warren.vector_const('social', 'owners', 1);                   -- written by the owner
SELECT warren.grant('social', 'analyst');
SET ROLE analyst;
SELECT warren.degree('social', 3);                                   -- readers may query
SELECT assert.equal(warren.degree('social', 3), 2::bigint, 'a reader may query');
SELECT assert.raises($$SELECT warren.refresh('social')$$, 'only the owner refreshes', '42501');
SELECT assert.raises($$SELECT warren.drop_vector('social', 'owners')$$, 'only the writer or owner drops a vector', '42501');
SELECT warren.vector_const('social', 'mine', 1);                     -- readers may write their own vectors
SELECT warren.drop_vector('social', 'mine');
RESET ROLE;
SELECT warren.revoke('social', 'analyst');
REVOKE USAGE ON SCHEMA assert FROM analyst, stranger;
DROP ROLE analyst;
DROP ROLE stranger;

------------------------------------------------------------------------
-- docs/semantics.md, "Id types and type witnesses"
------------------------------------------------------------------------
SELECT warren.neighbours('social', 3::bigint);                        -- an id argument has the graph's type
SELECT assert.raises($$SELECT warren.neighbours('social', 3)$$, 'int4 literal on an int8 graph', '42804');
SELECT warren.degree('social', 3);                                   -- no ids in the result: int4 widens
SELECT * FROM warren.components('social', NULL::bigint);             -- no id argument: a witness
SELECT warren.pagerank('social', 'pr');
SELECT * FROM warren.vector_values('social', 'pr', NULL::bigint);

------------------------------------------------------------------------
-- docs/semantics.md, "Unknown and tombstoned ids"
------------------------------------------------------------------------
SELECT assert.equal(warren.degree('social', 5), 0::bigint, 'an id compacted away has degree 0');
SELECT assert.equal(warren.degree('social', 99), 0::bigint, 'an unknown id has degree 0');
SELECT assert.equal((SELECT count(*) FROM warren.neighbours('social', 99::bigint)), 0::bigint, 'no neighbours');
SELECT assert.equal(warren.reachable('social', 99::bigint, 99::bigint), false, 'unknown reaches nothing');
SELECT assert.equal(warren.shortest_path('social', 1::bigint, 99::bigint), NULL::bigint[], 'no path');
SELECT assert.equal((SELECT count(*) FROM warren.bfs('social', ARRAY[99::bigint, 1::bigint], 1)), 2::bigint,
                    'unknown starts are skipped');

------------------------------------------------------------------------
-- docs/compute.md: the 'roads' graph
------------------------------------------------------------------------
CREATE TABLE city (id int PRIMARY KEY, name text NOT NULL, pop int NOT NULL);
CREATE TABLE road (a int NOT NULL, b int NOT NULL, kind text NOT NULL, km float8 NOT NULL);
INSERT INTO city VALUES (1, 'ash', 100), (2, 'birch', 50), (3, 'cedar', 20), (4, 'dale', 80), (5, 'elm', 10);
INSERT INTO road VALUES (1, 2, 'highway', 10), (2, 3, 'lane', 4), (3, 4, 'highway', 7),
                        (1, 4, 'lane', 30), (4, 5, 'lane', 2);
SELECT warren.create_graph('roads', 'SELECT id FROM city', 'SELECT a, b, kind, km FROM road',
                           directed := false, weighted := true, has_relations := true);
SELECT warren.refresh('roads') AS v \gset

-- "Vectors"
SELECT warren.vector_from_query('roads', 'pop', 'SELECT id, pop FROM city');
SELECT warren.vector_const('roads', 'one', 1, persist := false);      -- transient: returns NULL
SELECT warren.vector_degree('roads', 'deg');
SELECT * FROM warren.vector_values('roads', 'pop', NULL::int) ORDER BY node_id;
SELECT warren.vector_stats('roads', 'pop');
SELECT assert.equal(warren.vector_const('roads', 'one', 1, persist := false), NULL::bigint, 'transient');
SELECT assert.equal((warren.vector_stats('roads', 'pop') ->> 'mean')::float8, 52::float8, 'stats');

-- "Semiring products"
SELECT warren.spmv('roads', 'pop', 'nbr_pop', 'plus_second');         -- sum of neighbours' pop
SELECT warren.spmv('roads', 'one', 'highways', 'plus_pair', rels := ARRAY['highway']);
SELECT warren.spmv('roads', 'one', 'km', 'plus_first');               -- sum of road lengths
SELECT n.node_id, n.value AS nbr_pop, h.value AS highways, k.value AS km
FROM warren.vector_values('roads', 'nbr_pop', NULL::int) n
JOIN warren.vector_values('roads', 'highways', NULL::int) h USING (node_id)
JOIN warren.vector_values('roads', 'km', NULL::int) k USING (node_id)
ORDER BY node_id;
SELECT assert.equal((SELECT array_agg(value ORDER BY node_id) FROM warren.vector_values('roads', 'nbr_pop', NULL::int)),
                    ARRAY[130, 120, 130, 130, 80]::float8[], 'plus_second');
SELECT assert.equal((SELECT array_agg(value ORDER BY node_id) FROM warren.vector_values('roads', 'highways', NULL::int)),
                    ARRAY[1, 1, 1, 1, 0]::float8[], 'plus_pair over highways');
SELECT assert.equal((SELECT array_agg(value ORDER BY node_id) FROM warren.vector_values('roads', 'km', NULL::int)),
                    ARRAY[40, 14, 11, 39, 2]::float8[], 'plus_first');

-- "Masks, replace and accumulate"
SELECT warren.vector_map('roads', 'big', 'CASE WHEN pop >= 50 THEN 1 ELSE 0 END');
SELECT warren.spmv('roads', 'pop', 'best', 'max_second');
SELECT assert.equal((SELECT array_agg(value ORDER BY node_id) FROM warren.vector_values('roads', 'best', NULL::int)),
                    ARRAY[80, 100, 80, 100, 80]::float8[], 'max_second');
SELECT warren.spmv('roads', 'pop', 'best', 'min_second', mask := 'big');                  -- outside: kept
SELECT assert.equal((SELECT array_agg(value ORDER BY node_id) FROM warren.vector_values('roads', 'best', NULL::int)),
                    ARRAY[50, 20, 80, 10, 80]::float8[], 'mask keeps y outside');
SELECT warren.spmv('roads', 'pop', 'best', 'min_second', mask := 'big', replace := true); -- outside: identity
SELECT assert.equal((SELECT array_agg(value ORDER BY node_id) FROM warren.vector_values('roads', 'best', NULL::int)),
                    ARRAY[50, 20, 'Infinity', 10, 'Infinity']::float8[], 'replace writes the identity');
SELECT warren.vector_const('roads', 'acc', 0);
SELECT warren.spmv('roads', 'pop', 'acc', 'plus_second', accumulate := 'plus');
SELECT warren.spmv('roads', 'pop', 'acc', 'plus_second', accumulate := 'plus');
SELECT assert.equal((SELECT array_agg(value ORDER BY node_id) FROM warren.vector_values('roads', 'acc', NULL::int)),
                    ARRAY[260, 240, 260, 260, 160]::float8[], 'accumulate adds into y');

-- "vector_map and explain_map"
SELECT warren.vector_map('roads', 'share', 'pop / NULLIF(nbr_pop, 0)');
SELECT warren.explain_map('roads', 'pop / NULLIF(nbr_pop, 0)');
SELECT warren.explain_map('roads', 'pop::numeric + 1');
SELECT assert.equal(warren.explain_map('roads', 'pop / NULLIF(nbr_pop, 0)') ->> 'path', 'compiled', 'compiled');
SELECT assert.equal(warren.explain_map('roads', 'pop::numeric + 1') ->> 'path', 'executor', 'executor');
SELECT assert.equal(warren.vector_reduce('roads', 'pop', 'sum'), 260::float8, 'reduce');
CREATE FUNCTION my_pagerank(graph text, damping float8 DEFAULT 0.85, tol float8 DEFAULT 1e-6)
RETURNS int LANGUAGE plpgsql AS $$
DECLARE i int := 0; delta float8;
BEGIN
  PERFORM warren.vector_degree(graph, 'deg', persist := false);
  PERFORM warren.vector_const(graph, 'pr', 1.0, persist := false);
  LOOP
    i := i + 1;
    PERFORM warren.vector_map(graph, 'msg', 'pr / NULLIF(deg, 0)', persist := false);
    PERFORM warren.spmv(graph, 'msg', 'gathered', 'plus_second', transpose := true, persist := false);
    PERFORM warren.vector_map(graph, 'next', format('%s + %s * gathered', 1 - damping, damping),
                              persist := false);
    delta := warren.vector_norm_diff(graph, 'next', 'pr', 'l1');
    PERFORM warren.vector_map(graph, 'pr', 'next', persist := false);
    EXIT WHEN delta < tol OR i >= 100;
  END LOOP;
  RETURN i;
END $$;
SELECT my_pagerank('roads') AS iterations \gset
SELECT assert.that(:iterations BETWEEN 2 AND 99, 'my_pagerank converges: ' || :iterations);
SELECT warren.drop_vector('roads', 'pr');
SELECT warren.drop_vector('roads', 'deg');

-- "gather"
SELECT * FROM warren.gather('roads', 'SELECT id, name FROM city', NULL::int, NULL::text[]) ORDER BY node_id;
SELECT * FROM warren.gather('roads', 'pop', NULL::int, NULL::float8, 'max') ORDER BY node_id;
SELECT * FROM warren.gather('roads', 'SELECT id, pop FROM city', NULL::int, NULL::bigint, 'sum',
                            rels := ARRAY['lane']) ORDER BY node_id;
SELECT assert.equal((SELECT value FROM warren.gather('roads', 'SELECT id, name FROM city', NULL::int, NULL::text[])
                     WHERE node_id = 4), ARRAY['ash', 'cedar', 'elm'], 'collect in sender order');
SELECT assert.equal((SELECT array_agg(value ORDER BY node_id)
                     FROM warren.gather('roads', 'SELECT id, pop FROM city', NULL::int, NULL::bigint, 'sum',
                                        rels := ARRAY['lane'])),
                    ARRAY[80, 20, 50, 110, 80]::bigint[], 'sum over lanes');
SELECT assert.raises($$SELECT * FROM warren.gather('roads', 'SELECT id, name FROM city', NULL::int, NULL::text)$$,
                     'a wrong witness names the right one', '42804');
-- compute.md "gather": message-source rules and read-only state
CREATE TABLE pop (id int, pop int);                                  -- same name as vector 'pop'
SELECT assert.raises($$SELECT * FROM warren.gather('roads', 'pop', NULL::int, NULL::float8, 'max')$$,
                     'a name that is both a vector and a relation', '42725');
SELECT assert.equal((SELECT count(*) FROM warren.gather('roads', 'TABLE pop', NULL::int, NULL::int, 'max')),
                    0::bigint, 'TABLE <name> means the relation');
DROP TABLE pop;
SELECT assert.raises($$SELECT * FROM warren.gather('roads', 'WITH d AS (DELETE FROM city RETURNING id, pop) SELECT * FROM d',
                                                   NULL::int, NULL::bigint, 'sum')$$,
                     'a data-modifying CTE is refused', '0A000');
CREATE FUNCTION docs_writes(i int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN DELETE FROM city WHERE id = i; RETURN i; END $$;
SELECT assert.raises($$SELECT * FROM warren.gather('roads', 'SELECT docs_writes(id), pop FROM city', NULL::int, NULL::bigint, 'sum')$$,
                     'a write through a called function is refused', '25006');
SELECT assert.raises($$SELECT warren.vector_from_query('roads', 'w', 'SELECT docs_writes(id), pop FROM city')$$,
                     'vector_from_query is read-only too', '25006');
SELECT assert.equal((SELECT count(*) FROM city), 5::bigint, 'nothing was deleted');

-- "The algorithm library"
SELECT warren.pagerank('roads', 'pr', weighted := false);
SELECT warren.components_vector('roads', 'comp');
SELECT warren.sssp_vector('roads', 'dist', 1);
SELECT warren.triangles('roads', 'tri');
SELECT warren.vector_from_query('roads', 'emb', 'SELECT id, ARRAY[pop, 100 - pop]::real[] FROM city', dtype := 'f32');
SELECT warren.aggregate_neighbours('roads', 'emb', 'emb_nbr');
SELECT * FROM warren.vector_values('roads', 'dist', NULL::int) ORDER BY node_id;
SELECT * FROM warren.matrix_rows('roads', 'emb_nbr', NULL::int) ORDER BY node_id;
SELECT assert.equal((SELECT array_agg(value ORDER BY node_id) FROM warren.vector_values('roads', 'dist', NULL::int)),
                    ARRAY[0, 10, 14, 21, 23]::float8[], 'sssp');
SELECT assert.equal((SELECT array_agg(value ORDER BY node_id) FROM warren.vector_values('roads', 'tri', NULL::int)),
                    ARRAY[0, 0, 0, 0, 0]::float8[], 'no triangles in roads');
SELECT assert.equal((SELECT row FROM warren.matrix_rows('roads', 'emb_nbr', NULL::int) WHERE node_id = 5),
                    ARRAY[80, 20]::float8[], 'mean of neighbour rows');
SELECT assert.equal((SELECT count(DISTINCT value) FROM warren.vector_values('roads', 'comp', NULL::int)),
                    1::bigint, 'one component');
-- compute.md: intermediates are dropped; the _warren_ prefix is reserved
SELECT assert.raises($$SELECT warren.vector_values('roads', '_warren_pagerank_one', NULL::int)$$,
                     'no intermediate left behind', '42704');
SELECT assert.raises($$SELECT warren.vector_const('roads', '_warren_mine', 1)$$, 'reserved prefix', '42939');
SELECT assert.raises($$SELECT * FROM warren.eigen('roads', 1, vectors_prefix := '_warren_e')$$,
                     'reserved eigen prefix', '42939');

-- "eigen"
SELECT * FROM warren.eigen('roads', 2, 'laplacian', 'smallest', vectors_prefix := 'lap', dtype := 'f64');
SELECT assert.that((SELECT abs(eigenvalue) < 1e-9 FROM warren.eigen('roads', 2, 'laplacian', 'smallest',
                    vectors_prefix := 'lap', dtype := 'f64') WHERE i = 1), 'a connected graph''s Laplacian has 0 once');

-- "distance"
SELECT warren.distance('roads', :v, :v, 'edit');
SELECT assert.equal((warren.distance('roads', :v, :v, 'edit') #>> '{overall,distance}')::float8, 0::float8, 'edit 0');

------------------------------------------------------------------------
-- docs/change-detection.md, pattern 1: a staged build in the transaction
------------------------------------------------------------------------
CREATE TABLE entities  (id bigint PRIMARY KEY);
CREATE TABLE relations (src bigint NOT NULL, dst bigint NOT NULL);
-- Two communities of four, joined by one edge.
INSERT INTO entities SELECT generate_series(1, 8);
INSERT INTO relations SELECT a, b FROM generate_series(1, 4) a, generate_series(1, 4) b WHERE a < b;
INSERT INTO relations SELECT a, b FROM generate_series(5, 8) a, generate_series(5, 8) b WHERE a < b;
INSERT INTO relations VALUES (4, 5);
SELECT warren.create_graph('kg', 'SELECT id FROM entities', 'SELECT src, dst FROM relations', directed := false);
SELECT warren.refresh('kg') AS base \gset
SELECT * FROM warren.eigen('kg', 2, 'normalized_laplacian', 'smallest', vectors_prefix := 'lap', dtype := 'f64');

BEGIN;
-- 1. ingest: a new entity that bridges the two communities
INSERT INTO entities VALUES (9);
INSERT INTO relations SELECT 9, id FROM entities WHERE id <> 9;
-- 2. build a staged version from this transaction's view of the tables
SELECT warren.refresh('kg', promote := false, label := 'corpus-2026-10-19') AS candidate \gset
-- 3. the structural delta
SELECT warren.diff('kg', :base, :candidate) -> 'nodes' AS nodes,
       warren.diff('kg', :base, :candidate) -> 'all_relations' AS edges;
-- 4. the spectral comparison
SELECT * FROM warren.eigen('kg', 2, 'normalized_laplacian', 'smallest', vectors_prefix := 'lap', dtype := 'f64',
                           version := :candidate);
SELECT warren.distance('kg', :base, :candidate, 'subspace', vectors_prefix := 'lap', k := 2) AS change \gset
SELECT warren.distance('kg', :base, :candidate, 'spectral', vectors_prefix := 'lap', k := 2,
                       matrix := 'normalized_laplacian') AS gap \gset
SELECT :'change'::jsonb ->> 'projection' AS projection, :'gap'::jsonb -> 'after' AS eigenvalues;
SELECT assert.that((:'change'::jsonb ->> 'projection')::float8 > 0, 'the bridge turns the subspace');
SELECT assert.that((:'gap'::jsonb ->> 'max')::float8 > 0.2, 'the bridge raises the spectral gap');
SELECT assert.equal((warren.diff('kg', :base, :candidate) #>> '{nodes,added}')::bigint, 1::bigint, 'diff');
-- compute.md "distance": a stored-vector spectral distance needs its matrix
SELECT assert.raises(format($$SELECT warren.distance('kg', %s, %s, 'spectral', vectors_prefix := 'lap', k := 2)$$,
                            :base, :candidate), 'spectral with vectors_prefix needs matrix');
-- change-detection.md: sibling staged versions with diverging new nodes
SAVEPOINT siblings;
DELETE FROM relations WHERE src = 9; DELETE FROM entities WHERE id = 9;
INSERT INTO entities VALUES (10);
INSERT INTO relations VALUES (10, 1);
SELECT warren.refresh('kg', promote := false) AS sibling \gset
SELECT assert.raises(format($$SELECT warren.distance('kg', %s, %s, 'edit')$$, :candidate, :sibling),
                     'siblings whose new nodes diverge', '22023');
SELECT assert.equal((warren.distance('kg', :base, :sibling, 'edit') #>> '{overall,added}')::bigint, 2::bigint,
                    'each sibling compares with the base');
ROLLBACK TO SAVEPOINT siblings;
-- 5. decide: here, the change is too large
ROLLBACK;
SELECT assert.equal((SELECT count(*) FROM warren.versions WHERE version_id = :candidate), 0::bigint, 'gone');
SELECT assert.equal((SELECT count(*) FROM entities), 8::bigint, 'the ingest rolled back too');

------------------------------------------------------------------------
-- docs/change-detection.md, pattern 2: a shadow graph
------------------------------------------------------------------------
CREATE SCHEMA shadow;
CREATE TABLE shadow.entities  AS SELECT id FROM entities;
CREATE TABLE shadow.relations AS SELECT src, dst FROM relations;
INSERT INTO shadow.entities VALUES (9);
INSERT INTO shadow.relations SELECT 9, id FROM shadow.entities WHERE id <> 9;
SELECT warren.create_graph('kg_shadow', 'SELECT id FROM shadow.entities',
                           'SELECT src, dst FROM shadow.relations', directed := false);
SELECT warren.refresh('kg_shadow') AS shadow \gset
SELECT * FROM warren.eigen('kg_shadow', 2, 'normalized_laplacian', 'smallest', vectors_prefix := 'lap', dtype := 'f64');
SELECT warren.distance('kg', :base, :shadow, 'subspace', vectors_prefix := 'lap', k := 2,
                       graph2 := 'kg_shadow') AS cross_change \gset
SELECT warren.distance('kg', :base, :shadow, 'edit', graph2 := 'kg_shadow') -> 'overall';
SELECT assert.that(abs((:'cross_change'::jsonb ->> 'projection')::float8
                       - (:'change'::jsonb ->> 'projection')::float8) < 1e-6,
                   'the shadow graph gives the same subspace distance');
SELECT assert.equal((warren.distance('kg', :base, :shadow, 'edit', graph2 := 'kg_shadow') #>> '{overall,removed}')::bigint,
                    0::bigint, 'aligned by external id, the shadow graph only adds edges');

------------------------------------------------------------------------
-- docs/limits.md
------------------------------------------------------------------------
SET warren.max_compute_memory = '64kB';
SELECT assert.raises($$SELECT warren.vector_from_query('kg', 'wide', 'SELECT id, array_fill(1.0::float8, ARRAY[2000]) FROM entities')$$, 'over the compute budget', '53400');
RESET warren.max_compute_memory;
-- limits.md "Memory model": the cache cap
SET warren.max_cache_memory = '64kB';
SELECT assert.raises($$SELECT warren.vector_from_query('kg', 'wide', 'SELECT id, array_fill(1.0::float8, ARRAY[2000]) FROM entities',
                                persist := false)$$, 'a transient vector over the cache cap', '53400');
SELECT warren.evict('kg');                                         -- frees kg's cached versions and vectors
SELECT count(*) FROM warren.bfs('kg', ARRAY[1::bigint], 2);        -- reloads; still works under the cap
SELECT assert.equal((SELECT count(*) FROM warren.bfs('kg', ARRAY[1::bigint], 2)), 5::bigint, 'works under the cap');
RESET warren.max_cache_memory;
SELECT assert.raises($$SELECT * FROM warren.walk('roads', ARRAY[1], ARRAY[warren.s(NULL, 'out', NULL::int[], 1, 3)],
                                                 mode := 'paths', max_results := 20000000)$$,
                     'max_results above warren.max_walk_paths', '54000');
SELECT warren.reachable_set('roads', ARRAY[1]) LIMIT 2;               -- select list: stops after 2 rows
SELECT (warren.last_stats() ->> 'nodes_visited')::int AS nodes_select_list \gset
SELECT * FROM warren.reachable_set('roads', ARRAY[1]) LIMIT 2;        -- FROM: runs to completion
SELECT (warren.last_stats() ->> 'nodes_visited')::int AS nodes_from \gset
SELECT assert.that(:nodes_select_list < :nodes_from, 'LIMIT stops a select-list SRF early');
-- semantics.md "Parallel query labels": restricted reads work in parallel plans
CREATE TABLE big AS SELECT g FROM generate_series(1, 100000) g;
ANALYZE big;
SET parallel_setup_cost = 0; SET parallel_tuple_cost = 0; SET min_parallel_table_scan_size = 0;
SELECT assert.equal((SELECT count(*) FROM big WHERE g < (SELECT warren.vector_reduce('roads', 'pop', 'sum'))),
                    259::bigint, 'a warren read in a parallel plan');
SELECT assert.equal((SELECT count(*) FROM big b JOIN warren.vector_values('roads', 'pop', NULL::int) v ON v.node_id = b.g),
                    5::bigint, 'a join with a vector read in a parallel plan');
RESET parallel_setup_cost; RESET parallel_tuple_cost; RESET min_parallel_table_scan_size;
