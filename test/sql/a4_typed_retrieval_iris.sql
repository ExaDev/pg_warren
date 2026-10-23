-- Appendix A.4, typed multi-hop retrieval, on a research knowledge graph
-- keyed by IRIs (DESIGN.md §5.7, §9.2): the UK institutions reachable from
-- a root paper through 1–3 citation hops, then authorship by an active
-- person, then affiliation, as warren.walk finds them and as a chain of
-- recursive CTEs over the text keys does, against the fixture's truth.
-- fixture: citation --papers 20000 --people 6000 --institutions 400 --seed 1

CREATE FUNCTION iri(kind text, id bigint) RETURNS text IMMUTABLE LANGUAGE sql
RETURN 'https://example.org/' || kind || '/' || id;

CREATE TABLE kg_nodes AS
SELECT iri('paper', id) AS id FROM papers
UNION ALL SELECT iri('person', id) FROM people
UNION ALL SELECT iri('institution', id) FROM institutions;
CREATE TABLE kg_edges AS
SELECT iri('paper', citing) AS src, iri('paper', cited) AS dst, 'cites' AS rel FROM citations
UNION ALL SELECT iri('paper', paper_id), iri('person', person_id), 'authored_by' FROM authorship
UNION ALL SELECT iri('person', person_id), iri('institution', institution_id), 'affiliated_with' FROM affiliations;
CREATE INDEX ON kg_edges (src, rel);
CREATE TABLE active_people AS SELECT iri('person', id) AS id FROM people WHERE active;
CREATE TABLE uk AS SELECT iri('institution', id) AS id FROM institutions WHERE country = 'GB';
SELECT iri('paper', value::text::bigint) AS root FROM truth WHERE key = 'root' \gset

SELECT warren.create_graph('kg', 'SELECT id FROM kg_nodes', 'SELECT src, dst, rel FROM kg_edges',
                           has_relations := true);
SELECT warren.refresh('kg');

CREATE TABLE steps AS
SELECT ARRAY[warren.s(ARRAY['cites'], 'out', min_hops := 1, max_hops := 3),
             warren.s(ARRAY['authored_by'], 'out', ARRAY(SELECT id FROM active_people)),
             warren.s(ARRAY['affiliated_with'], 'out', ARRAY(SELECT id FROM uk))] AS filtered,
       ARRAY[warren.s(ARRAY['cites'], 'out', min_hops := 1, max_hops := 3),
             warren.s(ARRAY['authored_by'], 'out'),
             warren.s(ARRAY['affiliated_with'], 'out', ARRAY(SELECT id FROM uk))] AS unfiltered;

CREATE TABLE walked AS
SELECT end_id FROM warren.walk('kg', ARRAY[:'root'], (SELECT filtered FROM steps));
SELECT (warren.last_stats() ->> 'nodes_visited')::bigint AS visited_filtered \gset

-- The oracle: one recursive CTE per step, over the text keys.
CREATE TABLE oracle AS
WITH RECURSIVE near (id, depth) AS (
  SELECT :'root'::text, 0
  UNION
  SELECT e.dst, n.depth + 1 FROM near n JOIN kg_edges e ON e.src = n.id AND e.rel = 'cites' WHERE n.depth < 3),
authors AS (
  SELECT DISTINCT e.dst AS id
  FROM (SELECT DISTINCT id FROM near WHERE depth >= 1) p
  JOIN kg_edges e ON e.src = p.id AND e.rel = 'authored_by' JOIN active_people a ON a.id = e.dst)
SELECT DISTINCT e.dst AS id
FROM authors a JOIN kg_edges e ON e.src = a.id AND e.rel = 'affiliated_with' JOIN uk ON uk.id = e.dst;

SELECT assert.that((SELECT count(*) FROM walked) > 5, 'the walk reaches several institutions');
SELECT assert.equal(ARRAY(SELECT end_id FROM walked ORDER BY 1), ARRAY(SELECT id FROM oracle ORDER BY 1),
                    'walk ends equal the recursive CTEs over text keys');
SELECT assert.equal(ARRAY(SELECT end_id FROM walked ORDER BY 1),
                    ARRAY(SELECT iri('institution', institution_id) FROM truth_walk_ends ORDER BY 1),
                    'walk ends equal the fixture''s truth');
SELECT assert.equal((SELECT count FROM warren.walk('kg', ARRAY[:'root'], (SELECT filtered FROM steps), 'count')),
                    (SELECT count(*) FROM walked), 'count is the cardinality of ends');

-- Paths: at most max_results, each along edges of the right relations,
-- through active authors, ending at a UK institution.
CREATE TABLE paths AS
SELECT end_id, path, rels FROM warren.walk('kg', ARRAY[:'root'], (SELECT filtered FROM steps), 'paths',
                                           distinct_ends := false, max_results := 1000);
SELECT assert.that((SELECT count(*) FROM paths) BETWEEN 1 AND 1000, 'paths are bounded by max_results');
SELECT assert.equal((SELECT count(*) FROM paths p, generate_subscripts(p.rels, 1) i
                     WHERE NOT EXISTS (SELECT FROM kg_edges e WHERE e.src = p.path[i] AND e.dst = p.path[i + 1]
                                                               AND e.rel = p.rels[i])),
                    0::bigint, 'every hop of every path is an edge of its relation');
SELECT assert.equal((SELECT count(*) FROM paths p
                     WHERE p.path[array_length(p.path, 1) - 1] NOT IN (SELECT id FROM active_people)
                        OR p.end_id NOT IN (SELECT id FROM uk) OR p.end_id <> p.path[array_length(p.path, 1)]),
                    0::bigint, 'every path passes the filters');

-- Filters prune while walking: without the second step's, more is visited.
SELECT count(*) FROM warren.walk('kg', ARRAY[:'root'], (SELECT unfiltered FROM steps));
SELECT assert.that((warren.last_stats() ->> 'nodes_visited')::bigint > :visited_filtered,
                   'dropping the authorship filter visits more nodes');

-- Reversed from the ends, the walks lead back to the root.
SELECT assert.that(:'root' IN (SELECT end_id FROM warren.walk('kg', ARRAY(SELECT end_id FROM walked),
                                                              (SELECT filtered FROM steps), reverse := true)),
                   'the reversed walk reports the root among its starts');
