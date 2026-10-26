"""A.1, fraud ring detection, at about 10M nodes: the fixture's structure
(`bench/fixtures/fraud.py`) generated in SQL with its proportions kept.

- 3.35M honest accounts, each with 1–3 identifiers of its own (IP address,
  device fingerprint, email address), about 6.7M in all.
- Planted rings at the fixture's density (20 per 100,000 honest accounts,
  so 670): 5–40 accounts sharing 2–6 identifiers; every member uses the
  ring's first identifier and others by draw, and every identifier has at
  least two users.
- Three hubs used by 5%, 0.8% and 2% of all accounts (the fixture's
  5,000, 800 and 2,000 of 100,000): a NAT IP, an office device, a
  disposable-email domain.
- 30% of ring accounts and 0.5% of honest accounts flagged.

The program is A.1's, with every vector it writes transient
(`persist := false`): as written, persisting every iteration's vectors in
the one transaction of a call, it would write four whole-graph vectors an
iteration that no VACUUM can reclaim before the call ends (the
`persisted write` measure gives the bytes). Ring checks are scaled: the
top `rings + rings / 4` output rings (25 for the fixture's 20)."""

from .plan import Step, UseCase, at_least, at_most, between, equals

HONEST = 3_350_000
RINGS_PER_100K = 20
RING_SLOTS = 40  # the largest ring: each ring has this many account slots
HUBS = (0.05, 0.008, 0.02)  # share of all accounts using the NAT IP, office device, email domain
GRAPH = "fraud"


def sizes(scale):
    honest = HONEST // scale
    rings = max(3, round(RINGS_PER_100K * honest / 100_000))
    ring_accounts = rings * 22.5  # 5–40 accounts, uniform
    accounts = honest + ring_accounts
    identifiers = 2 * honest + rings * 4 + 3
    links = 2 * honest + ring_accounts * 2.5 + sum(HUBS) * accounts
    return {"honest_accounts": honest, "rings": rings, "nodes": round(accounts + identifiers),
            "edges": round(links), "top_rings": rings + rings // 4}


def generate(honest, rings):
    cap = honest + RING_SLOTS * rings  # account slots; shuffle() maps them onto ids 1..cap
    ring_id = f"{cap} + 1 + 3 * {honest} + 8 * m.r + i"
    return (
        f"""CREATE TABLE ring_members AS
WITH sizes AS (SELECT r, 5 + pick(r, 1, 36) AS size, 2 + pick(r, 2, 5) AS identifiers FROM generate_series(0, {rings - 1}) r)
SELECT r, j, {honest} + {RING_SLOTS} * r + j AS idx, size, identifiers FROM sizes, generate_series(0, size - 1) j""",
        f"""CREATE TABLE accounts AS
WITH a AS (SELECT idx, CASE WHEN ring THEN draw(idx, 20) < 0.3 ELSE draw(idx, 21) < 0.005 END AS flagged
           FROM (SELECT idx, false AS ring FROM generate_series(0, {honest - 1}) idx
                 UNION ALL SELECT idx, true FROM ring_members) x)
SELECT shuffle(idx, {cap}) AS id, flagged AND draw(idx, 22) < 0.5 AS confirmed_fraud,
       (CASE WHEN flagged AND NOT draw(idx, 22) < 0.5 THEN 2 + pick(idx, 23, 3)
             WHEN flagged THEN pick(idx, 24, 5) ELSE pick(idx, 25, 2) END)::int AS chargebacks
FROM a""",
        f"""CREATE VIEW fraud_links AS
SELECT shuffle(h, {cap}) AS account_id, {cap} + 1 + 3 * h + i AS identifier_id, pick(3 * h + i, 5, 3)::int AS kind
FROM generate_series(0, {honest - 1}) h, generate_series(0, pick(h, 4, 3)) i
UNION ALL
SELECT shuffle(m.idx, {cap}), {ring_id}, pick(8 * m.r + i, 6, 3)::int
FROM ring_members m, generate_series(0, m.identifiers - 1) i
WHERE i = 0 OR draw(8 * m.idx + i, 7) < 0.5 OR i = 1 + pick(m.idx, 8, m.identifiers - 1) OR m.j IN (i, i + 1)
UNION ALL
SELECT shuffle(a.idx, {cap}), {cap} + 1 + 3 * {honest} + 8 * {rings} + k, k
FROM (SELECT idx FROM generate_series(0, {honest - 1}) idx UNION ALL SELECT idx FROM ring_members) a,
     generate_series(0, 2) k
WHERE draw(3 * a.idx + k, 9) < (ARRAY{list(HUBS)})[k + 1]""",
        "CREATE TABLE account_ip_events AS SELECT account_id, identifier_id AS ip_id FROM fraud_links WHERE kind = 0",
        "CREATE TABLE account_device_events AS SELECT account_id, identifier_id AS device_id FROM fraud_links WHERE kind = 1",
        "CREATE TABLE account_emails AS SELECT account_id, identifier_id AS email_id FROM fraud_links WHERE kind = 2",
        "CREATE TABLE ip_addresses AS SELECT DISTINCT ip_id AS id FROM account_ip_events",
        "CREATE TABLE device_fingerprints AS SELECT DISTINCT device_id AS id FROM account_device_events",
        "CREATE TABLE email_addresses AS SELECT DISTINCT email_id AS id FROM account_emails",
        f"""CREATE TABLE truth_rings AS
SELECT r AS ring, shuffle(idx, {cap}) AS node_id, 'account'::text AS kind FROM ring_members
UNION ALL
SELECT m.r, {ring_id}, (ARRAY['ip', 'device', 'email'])[pick(8 * m.r + i, 6, 3) + 1]
FROM (SELECT DISTINCT r, identifiers FROM ring_members) m, generate_series(0, m.identifiers - 1) i""",
        "CREATE INDEX ON truth_rings (node_id)",
        "DROP VIEW fraud_links",
        "DROP TABLE ring_members",
        "ANALYZE",
    )


GRAPH_SQL = (
    """CREATE VIEW fraud_nodes AS
  SELECT id, 'account' AS kind FROM accounts
  UNION ALL SELECT id, 'ip'     FROM ip_addresses
  UNION ALL SELECT id, 'device' FROM device_fingerprints
  UNION ALL SELECT id, 'email'  FROM email_addresses""",
    """SELECT warren.create_graph('fraud',
  node_query := 'SELECT id FROM fraud_nodes',
  edge_query := $$
     SELECT account_id, ip_id,     'used_ip'     FROM account_ip_events
     UNION ALL SELECT account_id, device_id, 'used_device' FROM account_device_events
     UNION ALL SELECT account_id, email_id,  'has_email'   FROM account_emails $$,
  directed := false, has_relations := true)""",
)

# A.1's program, its vectors transient; vector_map goes through mapped() so
# the compiled-path check sees every expression.
PROGRAM = (
    "CREATE TABLE expressions (expr text)",
    """CREATE FUNCTION mapped(graph text, y text, expr text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO expressions VALUES (expr);
  PERFORM warren.vector_map(graph, y, expr, persist := false);
END $$""",
    "CREATE TABLE convergence (hub_degree float8, iterations int, delta float8, mass float8)",
    """CREATE FUNCTION fraud_suspicion(graph text, hub_degree float8 DEFAULT 50, damping float8 DEFAULT 0.7)
RETURNS int LANGUAGE plpgsql AS $$
DECLARE ran int := 0; delta float8; mass float8;
BEGIN
  PERFORM warren.vector_from_query(graph, 'seed', 'SELECT id, 1.0 FROM accounts WHERE confirmed_fraud OR chargebacks >= 2',
                                   persist := false);
  mass := warren.vector_reduce(graph, 'seed', 'sum');
  PERFORM warren.vector_from_query(graph, 'is_account', 'SELECT id, 1.0 FROM accounts', persist := false);
  PERFORM warren.vector_degree(graph, 'deg', persist := false);
  PERFORM mapped(graph, 'live',    format('CASE WHEN is_account = 1 OR deg <= %s THEN 1 ELSE 0 END', hub_degree));
  PERFORM mapped(graph, 'inv_deg', 'CASE WHEN deg > 0 THEN 1.0 / deg ELSE 0 END');
  PERFORM mapped(graph, 'score',   'seed');
  FOR i IN 1..50 LOOP
    ran := i;
    PERFORM mapped(graph, 'msg',      'score * inv_deg * live');
    PERFORM warren.spmv(graph, 'msg', 'gathered', semiring := 'plus_times', mask := 'live', persist := false);
    PERFORM mapped(graph, 'next',     format('%s * seed + %s * gathered', 1 - damping, damping));
    delta := warren.vector_norm_diff(graph, 'next', 'score', 'l1');
    PERFORM mapped(graph, 'score',    'next');
    EXIT WHEN delta < 1e-6 * mass;
  END LOOP;
  INSERT INTO convergence VALUES (hub_degree, ran, delta, mass);
  RETURN ran;
END $$""",
    """CREATE FUNCTION chunk_bytes_of(statement text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE before bigint := pg_total_relation_size('warren.vector_chunks');
BEGIN EXECUTE statement; RETURN pg_total_relation_size('warren.vector_chunks') - before; END $$""",
    """CREATE FUNCTION mixed_rings(members regclass) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE found bigint;
BEGIN
  EXECUTE format('SELECT count(*) FROM (SELECT m.ring FROM %s m JOIN truth_rings t USING (node_id)
                  WHERE t.kind = %L GROUP BY m.ring HAVING count(DISTINCT t.ring) > 1) x', members, 'account')
  INTO found;
  RETURN found;
END $$""",
)

AGGREGATION = """SELECT ring,
       count(*) FILTER (WHERE kind = 'account')  AS accounts,
       count(*) FILTER (WHERE kind <> 'account') AS identifiers,
       sum(score)                                AS total_suspicion,
       array_agg(node_id) FILTER (WHERE kind = 'account' AND score < 1) AS unflagged_members
FROM ({members}) m GROUP BY ring
HAVING count(*) FILTER (WHERE kind = 'account') BETWEEN 3 AND 500
ORDER BY total_suspicion DESC LIMIT 50"""

MEMBERS = """SELECT l.node_id, l.value::bigint AS ring, s.value AS score, n.kind
  FROM warren.vector_values('fraud', 'label', NULL::bigint) l
  JOIN warren.vector_values('fraud', 'score', NULL::bigint) s USING (node_id)
  JOIN fraud_nodes n ON n.id = l.node_id
  WHERE s.value > 0.01"""

RINGS = """SELECT m.ring, count(*) FILTER (WHERE kind = 'account') AS accounts, sum(score) AS total_suspicion
FROM {table} m GROUP BY m.ring"""


def rings_steps(suffix, hub_degree):
    """The program, label propagation and the ring members, for one hub mask."""
    return (
        Step(f"suspicion program{suffix}", f"SELECT fraud_suspicion('fraud', hub_degree := {hub_degree})",
             target=60 if not suffix else None),
        Step(f"suspicious mask{suffix}",
             "SELECT mapped('fraud', 'suspicious', 'CASE WHEN score > 0.01 AND live = 1 THEN 1 ELSE 0 END')"),
        Step(f"label propagation{suffix}", "SELECT warren.label_propagation('fraud', 'label', mask := 'suspicious')",
             target=30 if not suffix else None),
    )


def use_case(scale):
    s = sizes(scale)
    top = s["top_rings"]
    steps = (
        Step("generate", ";\n".join(generate(s["honest_accounts"], s["rings"]))),
        Step("hub degrees",
             "SELECT string_agg(c::text, ',' ORDER BY id) FROM (SELECT ip_id AS id, count(*) c FROM account_ip_events "
             "GROUP BY 1 UNION ALL SELECT device_id, count(*) FROM account_device_events GROUP BY 1 "
             "UNION ALL SELECT email_id, count(*) FROM account_emails GROUP BY 1 ORDER BY 2 DESC LIMIT 3) h",
             kind="measure", key="hub_degrees"),
        Step("graph", ";\n".join(GRAPH_SQL)),
        Step("refresh", "SELECT warren.refresh('fraud')"),
        Step("version", "SELECT warren.info('fraud') -> 'versions' -> 0", kind="measure", key="version"),
        Step("program", ";\n".join(PROGRAM)),
        *rings_steps("", 50),
        Step("aggregation", AGGREGATION.format(members=MEMBERS), target=30),
        Step("ring members", f"CREATE TABLE masked_members AS {MEMBERS}"),
        Step("output rings", f"CREATE TABLE rings AS {RINGS.format(table='masked_members')}"),
        Step("scores", "CREATE TABLE scores AS SELECT node_id, value AS score "
                       "FROM warren.vector_values('fraud', 'score', NULL::bigint)"),
        Step("flagged", "CREATE TABLE flagged AS SELECT id FROM accounts WHERE confirmed_fraud OR chargebacks >= 2; "
                        "ANALYZE masked_members, rings, scores, flagged"),
        Step("persisted write", "SELECT chunk_bytes_of($$SELECT warren.vector_map('fraud', 'score_persisted', "
                                "'score')$$)", kind="measure", key="persisted_write_bytes"),
        Step("compiled", "SELECT count(*) FILTER (WHERE warren.explain_map('fraud', expr) ->> 'path' <> 'compiled') "
                         "|| '/' || count(*) FROM (SELECT DISTINCT expr FROM expressions) e",
             kind="check", expect=lambda v: v is not None and v.startswith("0/") and int(v[2:]) >= 7,
             claim="every vector_map expression of the program takes the compiled path"),
        Step("convergence", "SELECT iterations FROM convergence WHERE hub_degree = 50 AND delta < 1e-6 * mass",
             kind="check", expect=between(30, 45),
             claim="the program converges to 1e-6 of the seed mass in about 40 of its 50 iterations"),
        Step("largest ring", "SELECT max(accounts) FROM rings", kind="measure", key="largest_masked"),
        Step("no huge ring", "SELECT max(accounts) FROM rings", kind="check", expect=at_most(500),
             claim="no output ring has more than 500 accounts (the hubs did not merge the population)"),
        Step("flagged rings", "SELECT count(DISTINCT ring) FROM truth_rings WHERE node_id IN (TABLE flagged)",
             kind="measure", key="flagged_rings"),
        Step("rings found", f"""WITH top AS (SELECT ring FROM rings WHERE accounts BETWEEN 3 AND 500
                                ORDER BY total_suspicion DESC LIMIT {top})
SELECT count(*) FROM (SELECT DISTINCT ring FROM truth_rings WHERE node_id IN (TABLE flagged)) t
WHERE NOT EXISTS (
  SELECT 1 FROM truth_rings r JOIN masked_members m ON m.node_id = r.node_id JOIN top tp ON tp.ring = m.ring
  WHERE r.ring = t.ring AND r.kind = 'account' GROUP BY m.ring
  HAVING 2 * count(*) >= (SELECT count(*) FROM truth_rings x WHERE x.ring = t.ring AND x.kind = 'account'))""",
             kind="check", expect=equals("0"),
             claim=f"every planted ring with a flagged account has half its accounts in one of the top {top} rings"),
        Step("unflagged members found",
             "SELECT avg((s.score > 0.01)::int) FROM truth_rings t JOIN scores s USING (node_id) "
             "WHERE t.kind = 'account' AND t.node_id NOT IN (TABLE flagged)",
             kind="check", expect=at_least(0.9), claim="≥ 90% of unflagged ring members score > 0.01"),
        Step("honest not found",
             "SELECT avg((s.score > 0.01)::int) FROM accounts a JOIN scores s ON s.node_id = a.id "
             "WHERE NOT EXISTS (SELECT FROM truth_rings t WHERE t.node_id = a.id)",
             kind="check", expect=at_most(0.01), claim="≤ 1% of honest accounts score > 0.01"),
        Step("masked rings unmixed", "SELECT mixed_rings('masked_members')", kind="check", expect=equals("0"),
             claim="with the hub mask no output ring mixes planted rings"),
        *rings_steps(" without hub mask", "1e9"),
        Step("ring members without hub mask", f"CREATE TABLE unmasked_members AS {MEMBERS}"),
        Step("unmasked rings mixed", "SELECT mixed_rings('unmasked_members')", kind="check", expect=at_least(1),
             claim="without the hub mask output rings mix planted rings"),
        Step("unmasked largest ring",
             f"SELECT max(accounts) / @@largest_masked@@::float8 FROM ({RINGS.format(table='unmasked_members')}) r",
             kind="check", expect=at_least(5),
             claim="without the hub mask the largest ring grows at least 5×"),
        Step("unmasked convergence", "SELECT iterations FROM convergence WHERE hub_degree = 1e9", kind="measure",
             key="unmasked_iterations"),
    )
    return UseCase("a1", "A.1", s, steps)
