-- Appendix A.1 (fraud ring detection) end to end, at the Appendix's
-- fixture scale (100,000 honest accounts, 20 rings, hubs of 5,000, 800 and
-- 2,000 accounts): the suspicion program, min-label propagation on the
-- suspicious live subgraph, and the ring report, with the hub mask and
-- without it. Measured outcomes that differ from A.1's first estimates are
-- recorded in DESIGN.md A.1.
-- fixture: fraud --seed 1
CREATE VIEW fraud_nodes AS
  SELECT id, 'account' AS kind FROM accounts
  UNION ALL SELECT id, 'ip'     FROM ip_addresses
  UNION ALL SELECT id, 'device' FROM device_fingerprints
  UNION ALL SELECT id, 'email'  FROM email_addresses;

SELECT warren.create_graph('fraud',
  node_query := 'SELECT id FROM fraud_nodes',
  edge_query := $$
     SELECT account_id, ip_id,     'used_ip'     FROM account_ip_events
     UNION ALL SELECT account_id, device_id, 'used_device' FROM account_device_events
     UNION ALL SELECT account_id, email_id,  'has_email'   FROM account_emails $$,
  directed := false, has_relations := true);
SELECT warren.refresh('fraud');

-- The A.1 program, as written there, except that it returns the
-- iterations it ran (A.1's `RETURN i` after `FOR i` returns NULL: the loop
-- variable is not the declared one) and records its last L1 change.
CREATE TABLE convergence (hub_degree float8, iterations int, delta float8);
CREATE FUNCTION fraud_suspicion(graph text, hub_degree float8 DEFAULT 50, damping float8 DEFAULT 0.7)
RETURNS int LANGUAGE plpgsql AS $$
DECLARE ran int; delta float8;
BEGIN
  PERFORM warren.vector_from_query(graph, 'seed',       'SELECT id, 1.0 FROM accounts WHERE confirmed_fraud OR chargebacks >= 2');
  PERFORM warren.vector_from_query(graph, 'is_account', 'SELECT id, 1.0 FROM accounts');
  PERFORM warren.vector_degree(graph, 'deg');
  PERFORM warren.vector_map(graph, 'live',    format('CASE WHEN is_account = 1 OR deg <= %s THEN 1 ELSE 0 END', hub_degree));
  PERFORM warren.vector_map(graph, 'inv_deg', 'CASE WHEN deg > 0 THEN 1.0 / deg ELSE 0 END');
  PERFORM warren.vector_map(graph, 'score',   'seed');
  FOR i IN 1..50 LOOP
    ran := i;
    PERFORM warren.vector_map(graph, 'msg',      'score * inv_deg * live');
    PERFORM warren.spmv(graph, 'msg', 'gathered', semiring := 'plus_times', mask := 'live');
    PERFORM warren.vector_map(graph, 'next',     format('%s * seed + %s * gathered', 1 - damping, damping));
    delta := warren.vector_norm_diff(graph, 'next', 'score', 'l1');
    PERFORM warren.vector_map(graph, 'score',    'next');
    EXIT WHEN delta < 1e-6;
  END LOOP;
  INSERT INTO convergence VALUES (hub_degree, ran, delta);
  RETURN ran;
END $$;

-- Then min-label propagation on the suspicious, live subgraph, and the
-- ring report.
CREATE FUNCTION fraud_rings(graph text) RETURNS TABLE (ring bigint, accounts bigint, identifiers bigint,
                                                     total_suspicion float8, unflagged_members bigint[])
LANGUAGE plpgsql AS $$
BEGIN
  PERFORM warren.vector_map(graph, 'suspicious', 'CASE WHEN score > 0.01 AND live = 1 THEN 1 ELSE 0 END');
  PERFORM warren.label_propagation(graph, 'label', mask := 'suspicious');
  DROP TABLE IF EXISTS members;
  CREATE TEMP TABLE members AS
    SELECT l.node_id, l.value::bigint AS ring, s.value AS score, n.kind
    FROM warren.vector_values(graph, 'label', NULL::bigint) l
    JOIN warren.vector_values(graph, 'score', NULL::bigint) s USING (node_id)
    JOIN fraud_nodes n ON n.id = l.node_id
    WHERE s.value > 0.01;
  RETURN QUERY
  WITH members AS (
    SELECT l.node_id, l.value::bigint AS ring, s.value AS score, n.kind
    FROM warren.vector_values(graph, 'label', NULL::bigint) l
    JOIN warren.vector_values(graph, 'score', NULL::bigint) s USING (node_id)
    JOIN fraud_nodes n ON n.id = l.node_id
    WHERE s.value > 0.01)
  SELECT m.ring,
         count(*) FILTER (WHERE kind = 'account'),
         count(*) FILTER (WHERE kind <> 'account'),
         sum(score),
         array_agg(node_id) FILTER (WHERE kind = 'account' AND score < 1)
  FROM members m GROUP BY m.ring;
END $$;

-- With the hub mask.
SELECT fraud_suspicion('fraud');
CREATE TABLE rings AS SELECT * FROM fraud_rings('fraud');
CREATE TABLE masked_members AS TABLE members;
CREATE TABLE scores AS SELECT node_id, value AS score FROM warren.vector_values('fraud', 'score', NULL::bigint);
CREATE TABLE flagged AS SELECT id FROM accounts WHERE confirmed_fraud OR chargebacks >= 2;

-- Every vector_map expression of the program takes the compiled path.
SELECT assert.equal(string_agg(expr, '; '), NULL, 'every vector_map expression is compiled')
FROM unnest(ARRAY['CASE WHEN is_account = 1 OR deg <= 50 THEN 1 ELSE 0 END',
                  'CASE WHEN deg > 0 THEN 1.0 / deg ELSE 0 END', 'seed', 'score * inv_deg * live',
                  '0.30000000000000004 * seed + 0.7 * gathered', 'next',
                  'CASE WHEN score > 0.01 AND live = 1 THEN 1 ELSE 0 END']) expr
WHERE warren.explain_map('fraud', expr) ->> 'path' <> 'compiled';

-- Convergence: the L1 change falls by about `damping` per iteration, so
-- with A.1's absolute tolerance of 1e-6 (on ~700 units of seed mass) the
-- program stops at its 50-iteration cap, its change by then below 1e-4.
SELECT assert.that(delta < 1e-4, format('the suspicion program converges (%s iterations, change %s)', iterations, delta))
FROM convergence WHERE hub_degree = 50;

-- No output ring is huge: the hubs did not merge the population.
SELECT assert.that((SELECT max(accounts) FROM rings) <= 500, 'no ring has more than 500 accounts');

-- Every planted ring with a flagged account is among the 25 most
-- suspicious output rings: one of them holds at least half of its
-- accounts. (A ring with no flagged account sends no suspicion: seeded
-- propagation cannot find it.)
CREATE TABLE top AS
SELECT ring FROM rings WHERE accounts BETWEEN 3 AND 500 ORDER BY total_suspicion DESC LIMIT 25;
SELECT assert.that((SELECT count(DISTINCT ring) FROM truth_rings WHERE node_id IN (TABLE flagged)) >= 18,
                   'most planted rings have a flagged account');
SELECT assert.equal(string_agg(t.ring::text, ', '), NULL, 'every planted ring with a flagged account is in the top 25')
FROM (SELECT DISTINCT ring FROM truth_rings WHERE node_id IN (TABLE flagged)) t
WHERE NOT EXISTS (
  SELECT 1 FROM truth_rings r JOIN masked_members m ON m.node_id = r.node_id JOIN top tp ON tp.ring = m.ring
  WHERE r.ring = t.ring AND r.kind = 'account'
  GROUP BY m.ring
  HAVING 2 * count(*) >= (SELECT count(*) FROM truth_rings x WHERE x.ring = t.ring AND x.kind = 'account'));

-- The unflagged ring members are found; honest accounts are not.
SELECT assert.that(avg((s.score > 0.01)::int) >= 0.9, format('%s of unflagged ring members score > 0.01', avg((s.score > 0.01)::int)))
FROM truth_rings t JOIN scores s USING (node_id)
WHERE t.kind = 'account' AND t.node_id NOT IN (TABLE flagged);
SELECT assert.that(avg((s.score > 0.01)::int) <= 0.01, format('%s of honest accounts score > 0.01', avg((s.score > 0.01)::int)))
FROM accounts a JOIN scores s ON s.node_id = a.id
WHERE a.id NOT IN (SELECT node_id FROM truth_rings);

-- Without the hub mask the hubs join rings: output rings mix planted
-- rings and grow several-fold. (A.1's first estimate was a ring of more
-- than 5,000 accounts; hub neighbours' scores stay below 0.01, so a hub
-- joins only the suspicious accounts it links.)
CREATE FUNCTION mixed_rings(members regclass) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE found bigint;
BEGIN
  EXECUTE format('SELECT count(*) FROM (SELECT m.ring FROM %s m JOIN truth_rings t USING (node_id)
                  WHERE t.kind = %L GROUP BY m.ring HAVING count(DISTINCT t.ring) > 1) x', members, 'account')
  INTO found;
  RETURN found;
END $$;
SELECT assert.equal(mixed_rings('masked_members'), 0::bigint, 'with the hub mask no output ring mixes planted rings');
SELECT fraud_suspicion('fraud', hub_degree := 1e9);
CREATE TABLE unmasked AS SELECT * FROM fraud_rings('fraud');
SELECT assert.that(mixed_rings('members') > 0, 'without the hub mask output rings mix planted rings');
SELECT assert.that((SELECT max(accounts) FROM unmasked) >= 5 * (SELECT max(accounts) FROM rings),
                   format('without the hub mask the largest ring grows from %s to %s accounts',
                          (SELECT max(accounts) FROM rings), (SELECT max(accounts) FROM unmasked)));
