-- Appendix A.3 (entity resolution by neighbourhood accumulation, tier 2)
-- end to end, at a tenth of the Appendix's fixture (20,000 records of 6,000
-- entities, 3 sources, 2% shared identifiers) for CI time.
--
-- The graph links each record to the identifiers it carries. Superstep 1:
-- identifiers send their values (text) to their records, which collect
-- them. Superstep 2: records send their own id to their identifiers, and
-- superstep 3: identifiers send every record id they collected on to their
-- records (one message each), so each record collects the records it shares
-- an identifier with. The trust rule, in SQL over the collected sets: two
-- records conflict if both carry identifiers of some kind but none in
-- common; an identifier is untrusted if any two of its holders conflict (a
-- shared office phone links unrelated records whose other identifiers
-- disagree); a link is kept if its records share a trusted identifier.
-- components_vector over the kept links gives entity ids; without the rule
-- every link is kept.
-- fixture: entity-resolution --records 20000 --entities 6000 --seed 1
-- A runaway join must fail the test, not fill the disk.
SET temp_file_limit = '2GB';

CREATE VIEW er_nodes AS SELECT id FROM records UNION ALL SELECT id FROM identifiers;
SELECT warren.create_graph('er', 'SELECT id FROM er_nodes', 'SELECT record_id, identifier_id FROM record_identifiers',
                           directed := false);
SELECT warren.refresh('er');

-- Superstep 1: variable-width (text) messages round-trip, in collect order.
CREATE TABLE idents AS
SELECT node_id AS record_id, value AS idents
FROM warren.gather('er', $$SELECT id, kind || ':' || value FROM identifiers$$, NULL::bigint, NULL::text[]);
CREATE TABLE dense AS SELECT node_id, dense_id FROM warren.id_map('er', NULL::bigint);
CREATE TABLE idents_oracle AS
SELECT ri.record_id, array_agg(i.kind || ':' || i.value ORDER BY d.dense_id) AS idents
FROM (SELECT DISTINCT record_id, identifier_id FROM record_identifiers) ri
JOIN identifiers i ON i.id = ri.identifier_id JOIN dense d ON d.node_id = i.id
GROUP BY ri.record_id;
SELECT assert.equal((SELECT count(*) FROM ((TABLE idents EXCEPT TABLE idents_oracle)
                                           UNION ALL (TABLE idents_oracle EXCEPT TABLE idents)) x), 0::bigint,
                    'superstep 1: gather collect equals the SQL join and array_agg');
SELECT assert.that((SELECT count(*) FROM idents) > 19000, 'nearly every record carries an identifier');

-- Superstep 2: identifiers collect their records' ids.
CREATE TABLE holders AS
SELECT node_id AS identifier_id, value AS records
FROM warren.gather('er', 'SELECT id, id FROM records', NULL::bigint, NULL::bigint[]);
SELECT assert.equal((SELECT count(*) FROM ((TABLE holders EXCEPT
                       SELECT ri.identifier_id, array_agg(ri.record_id ORDER BY d.dense_id)
                       FROM (SELECT DISTINCT record_id, identifier_id FROM record_identifiers) ri
                       JOIN dense d ON d.node_id = ri.record_id GROUP BY ri.identifier_id)) x), 0::bigint,
                    'superstep 2: gather collect equals the SQL join and array_agg');

-- Superstep 3: each record collects the records it shares identifiers with.
CREATE TABLE neighbourhood AS
SELECT node_id AS record_id, value AS records
FROM warren.gather('er', 'SELECT identifier_id, unnest(records) FROM holders', NULL::bigint, NULL::bigint[]);
SELECT assert.equal((SELECT count(*) FROM ((TABLE neighbourhood EXCEPT
                       SELECT ri.record_id, array_agg(h.r ORDER BY d.dense_id, h.ord)
                       FROM (SELECT DISTINCT record_id, identifier_id FROM record_identifiers) ri
                       JOIN holders ON holders.identifier_id = ri.identifier_id
                       CROSS JOIN LATERAL unnest(holders.records) WITH ORDINALITY h(r, ord)
                       JOIN dense d ON d.node_id = ri.identifier_id
                       GROUP BY ri.record_id)) x), 0::bigint,
                    'superstep 3: gather collect equals the SQL join and array_agg');

-- Links between records, and whether the trust rule keeps them.
CREATE TABLE carried AS SELECT record_id, split_part(x, ':', 1) AS kind, x FROM idents, unnest(idents) x;
CREATE INDEX ON carried (record_id);
CREATE TABLE links AS
SELECT DISTINCT n.record_id AS a, r AS b FROM neighbourhood n, unnest(n.records) r WHERE r <> n.record_id;
-- Statistics, so the planner sizes these joins right.
ANALYZE links; ANALYZE carried;
CREATE TABLE conflicts AS
SELECT DISTINCT a, b FROM (
  SELECT l.a, l.b FROM links l JOIN carried ca ON ca.record_id = l.a JOIN carried cb ON cb.record_id = l.b AND cb.kind = ca.kind
  GROUP BY l.a, l.b, ca.kind HAVING bool_and(ca.x <> cb.x)) c;
CREATE TABLE held AS SELECT identifier_id, r AS record_id FROM holders, unnest(records) r;
ANALYZE held; ANALYZE conflicts;
CREATE TABLE untrusted AS
SELECT DISTINCT h1.identifier_id FROM held h1 JOIN held h2 USING (identifier_id) JOIN conflicts c ON (c.a, c.b) = (h1.record_id, h2.record_id);
ALTER TABLE links ADD COLUMN trusted boolean DEFAULT false;
UPDATE links l SET trusted = true
FROM (SELECT DISTINCT h1.record_id AS a, h2.record_id AS b FROM held h1 JOIN held h2 USING (identifier_id)
      WHERE h1.record_id <> h2.record_id AND identifier_id NOT IN (TABLE untrusted)) t
WHERE (t.a, t.b) = (l.a, l.b);
SELECT assert.that((SELECT count(*) FROM untrusted) > 0, 'the trust rule finds untrusted identifiers');

-- Entity ids: components over the links kept (records with no link are
-- alone).
CREATE FUNCTION resolve(name text, kept text) RETURNS TABLE (record_id bigint, entity bigint) LANGUAGE plpgsql AS $$
BEGIN
  PERFORM warren.create_graph(name, 'SELECT id FROM records', format('SELECT a, b FROM links WHERE %s', kept),
                              directed := false);
  PERFORM warren.refresh(name);
  PERFORM warren.components_vector(name, 'entity');
  RETURN QUERY SELECT node_id, value::bigint FROM warren.vector_values(name, 'entity', NULL::bigint);
END $$;

-- Pairwise precision and recall against the generator's entities.
CREATE FUNCTION pairs(n bigint) RETURNS numeric LANGUAGE sql AS 'SELECT n * (n - 1) / 2.0';
CREATE FUNCTION quality(name text, kept text, OUT pair_precision numeric, OUT pair_recall numeric) LANGUAGE sql AS $$
  WITH resolved AS (SELECT r.record_id, r.entity, t.entity_id FROM resolve(name, kept) r JOIN truth_entities t USING (record_id)),
  both_ AS (SELECT sum(pairs(c)) AS n FROM (SELECT count(*) c FROM resolved GROUP BY entity, entity_id) x),
  predicted AS (SELECT sum(pairs(c)) AS n FROM (SELECT count(*) c FROM resolved GROUP BY entity) x),
  actual AS (SELECT sum(pairs(c)) AS n FROM (SELECT count(*) c FROM resolved GROUP BY entity_id) x)
  SELECT both_.n / predicted.n, both_.n / actual.n FROM both_, predicted, actual
$$;
SELECT * FROM quality('trusted', 'trusted') \gset trusted_
SELECT * FROM quality('untrusted', 'true') \gset untrusted_

SELECT assert.that(:trusted_pair_precision >= 0.98, format('trusted precision %s', :trusted_pair_precision));
SELECT assert.that(:trusted_pair_recall >= 0.95, format('trusted recall %s', :trusted_pair_recall));
SELECT assert.that(:untrusted_pair_precision < 0.9, format('untrusted precision %s', :untrusted_pair_precision));

-- The memory cap: gather's messages, kernel buffers and result are charged
-- to warren.max_compute_memory, and a superstep over it fails with the
-- documented error before allocating.
SET warren.max_compute_memory = '64kB';
SELECT assert.raises($$SELECT count(*) FROM warren.gather('er', 'SELECT identifier_id, unnest(records) FROM holders',
                                                       NULL::bigint, NULL::bigint[])$$,
                     'a superstep over the compute memory cap fails', '53400');
RESET warren.max_compute_memory;
