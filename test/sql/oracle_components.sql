-- Self-check of the NetworkX oracle (§12 oracle tests): its components
-- equal those of a recursive CTE over the same sparse random graph; then
-- warren.components must equal the oracle (§12).
-- fixture: random --nodes 300 --edges 220 --seed 2
-- oracle: components
CREATE TABLE cte_components AS
WITH RECURSIVE undirected AS (SELECT src AS a, dst AS b FROM edges UNION ALL SELECT dst, src FROM edges),
reach (root, id) AS (
  SELECT id, id FROM nodes
  UNION
  SELECT r.root, u.b FROM reach r JOIN undirected u ON u.a = r.id)
SELECT id AS node_id, min(root) AS component FROM reach GROUP BY id;

SELECT assert.equal((SELECT count(*) FROM oracle_components), 300::bigint, 'the oracle labels every node');
SELECT assert.that((SELECT count(DISTINCT component) FROM oracle_components) BETWEEN 2 AND 299,
                   'the fixture has several non-trivial components');
SELECT assert.that((SELECT max(c) FROM (SELECT count(*) AS c FROM oracle_components GROUP BY component) s) > 10,
                   'the fixture has a large component');
SELECT assert.equal((SELECT count(*) FROM ((TABLE cte_components EXCEPT TABLE oracle_components)
                                           UNION ALL (TABLE oracle_components EXCEPT TABLE cte_components)) d),
                    0::bigint, 'the oracle and the recursive CTE agree');

-- warren.components labels a component by its smallest dense id; relabel
-- by the smallest external id to compare partitions with the oracle.
SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges', directed := false);
SELECT warren.refresh('g');
CREATE TABLE warren_components AS
SELECT node_id, min(node_id) OVER (PARTITION BY component_id) AS component
FROM warren.components('g', NULL::bigint);
SELECT assert.equal((SELECT count(*) FROM ((TABLE warren_components EXCEPT TABLE oracle_components)
                                           UNION ALL (TABLE oracle_components EXCEPT TABLE warren_components)) d),
                    0::bigint, 'warren.components agrees with the oracle');
SELECT assert.equal((SELECT count(*) FROM warren_components), 300::bigint, 'warren.components labels every node');
