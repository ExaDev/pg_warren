CREATE TABLE IF NOT EXISTS warren_examples.tainted (
    id bigint PRIMARY KEY REFERENCES warren_examples.people(id)
);
-- Replaces this example's seed selection on each run. Use your business
-- rule here instead, for example a join to a sanctions or disruption table.
TRUNCATE warren_examples.tainted;
INSERT INTO warren_examples.tainted (id)
SELECT id FROM warren_examples.people ORDER BY md5(id::text), id LIMIT 100;
