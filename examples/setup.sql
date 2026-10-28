-- Run once in a fresh database with pg_warren installed on the server.
CREATE EXTENSION IF NOT EXISTS pg_warren;
CREATE SCHEMA warren_examples;
CREATE TABLE warren_examples.people (id bigint PRIMARY KEY, name text NOT NULL);
CREATE TABLE warren_examples.follows (
    src bigint REFERENCES warren_examples.people(id),
    dst bigint REFERENCES warren_examples.people(id),
    PRIMARY KEY (src, dst)
);
INSERT INTO warren_examples.people
SELECT n, 'person-' || n FROM generate_series(1, 10000) AS n;

-- A ring guarantees reachability; four hash shortcuts per person create
-- varied in-degrees. UNION removes duplicate edges; exclude self-loops.
INSERT INTO warren_examples.follows
SELECT id, id % 10000 + 1 FROM warren_examples.people
UNION
SELECT id, dst
FROM warren_examples.people CROSS JOIN generate_series(1, 4) AS k
CROSS JOIN LATERAL (
    SELECT ('x' || substr(md5(id::text || ':' || k::text), 1, 8))::bit(32)::bigint % 10000 + 1 AS dst
) AS target
WHERE id <> dst;
ANALYZE warren_examples.people;
ANALYZE warren_examples.follows;

SELECT warren.create_graph('example_social',
    'SELECT id FROM warren_examples.people',
    'SELECT src, dst FROM warren_examples.follows');
SELECT warren.refresh('example_social');
SELECT (SELECT count(*) FROM warren_examples.people) AS nodes,
       (SELECT count(*) FROM warren_examples.follows) AS edges;
