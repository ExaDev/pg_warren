//! The oracles of the §9.3 traversal functions.

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::super::{adjacency, generate, same, starts, Shape};

    /// `warren.bfs` (node, depth) against a depth-bounded recursive CTE,
    /// with and without the node filter; every `via` is checked to be a
    /// node one level up with an edge to the node.
    fn check_bfs(shape: &Shape) -> Result<(), Error> {
        let g = shape.name;
        for direction in shape.directions() {
            for rels in shape.selections() {
                for (filter, max_depth) in [(false, 3), (true, 4), (false, 0)] {
                    let adjacency = adjacency(g, direction, rels)?;
                    let (sql_filter, call_filter) = match filter {
                        true => (
                            format!("AND adj.b IN (SELECT id FROM {g}_filter)"),
                            format!(", node_filter := (SELECT array_agg(id) FROM {g}_filter)"),
                        ),
                        false => (String::new(), String::new()),
                    };
                    let oracle = format!(
                        "WITH RECURSIVE adj AS ({adjacency}),
                         r(n, d) AS (
                             SELECT DISTINCT id, 0 FROM {g}_starts
                             WHERE id IN (SELECT id FROM {g}_nodes)
                             UNION
                             SELECT adj.b, r.d + 1 FROM r JOIN adj ON adj.a = r.n
                             WHERE r.d < {max_depth} {sql_filter})
                         SELECT n, min(d) FROM r GROUP BY n"
                    );
                    let call = format!(
                        "warren.bfs('{g}', {starts}, {max_depth}, '{direction}', {rels}{call_filter})",
                        starts = starts(g)
                    );
                    let what =
                        format!("{g} bfs {direction} {rels} filter={filter} depth={max_depth}");
                    same(&what, &format!("SELECT node_id, depth FROM {call}"), &oracle)?;
                    let bad_via = Spi::get_one::<i64>(&format!(
                        "WITH b AS (SELECT * FROM {call}), adj AS ({adjacency})
                         SELECT count(*) FROM b
                         WHERE (b.depth = 0) <> (b.via IS NULL)
                            OR (b.depth > 0 AND NOT EXISTS (
                                   SELECT 1 FROM b p JOIN adj ON adj.a = p.node_id
                                   WHERE p.node_id = b.via AND p.depth = b.depth - 1
                                     AND adj.b = b.node_id))"
                    ))?;
                    assert_eq!(bad_via, Some(0), "{what}: via");
                }
            }
        }
        Ok(())
    }

    /// `warren.reachable_set` and `warren.reachable` against an unbounded
    /// recursive CTE (UNION stops at nodes seen before).
    fn check_reachability(shape: &Shape) -> Result<(), Error> {
        let g = shape.name;
        for direction in shape.directions() {
            for rels in shape.selections() {
                let adjacency = adjacency(g, direction, rels)?;
                let reached = |from: &str| {
                    format!(
                        "WITH RECURSIVE adj AS ({adjacency}),
                         r(n) AS (SELECT id FROM {from} WHERE id IN (SELECT id FROM {g}_nodes)
                                  UNION SELECT adj.b FROM r JOIN adj ON adj.a = r.n)
                         SELECT n FROM r"
                    )
                };
                same(
                    &format!("{g} reachable_set {direction} {rels}"),
                    &format!(
                        "SELECT warren.reachable_set('{g}', {starts}, NULL, {rels}, '{direction}')",
                        starts = starts(g)
                    ),
                    &reached(&format!("{g}_starts")),
                )?;
                let start = format!(
                    "(SELECT id FROM {g}_starts WHERE id IS NOT NULL ORDER BY md5(id::text) LIMIT 1)"
                );
                same(
                    &format!("{g} reachable {direction} {rels}"),
                    &format!(
                        "SELECT s.id, warren.reachable('{g}', {start}, s.id, NULL, {rels},
                                                       '{direction}')
                         FROM {g}_sample s"
                    ),
                    &format!(
                        "SELECT s.id, s.id IN ({reached}) FROM {g}_sample s",
                        reached = reached(&format!("(SELECT {start} AS id) one"))
                    ),
                )?;
            }
        }
        Ok(())
    }

    /// `warren.neighbours`, `warren.degree` and `warren.has_edge` against
    /// the arcs.
    fn check_neighbourhood(shape: &Shape) -> Result<(), Error> {
        let g = shape.name;
        for direction in shape.directions() {
            for rels in shape.selections() {
                let adjacency = adjacency(g, direction, rels)?;
                let what = format!("{g} neighbours {direction} {rels}");
                same(
                    &what,
                    &format!(
                        "SELECT s.id, n FROM {g}_sample s,
                             warren.neighbours('{g}', s.id, '{direction}', {rels}) n"
                    ),
                    &format!(
                        "SELECT a, b FROM ({adjacency}) adj WHERE a IN (SELECT id FROM {g}_sample)"
                    ),
                )?;
                same(
                    &format!("{g} degree {direction} {rels}"),
                    &format!(
                        "SELECT s.id, warren.degree('{g}', s.id, '{direction}', {rels})
                         FROM {g}_sample s"
                    ),
                    &format!(
                        "SELECT s.id, (SELECT count(*) FROM ({adjacency}) adj WHERE adj.a = s.id)
                         FROM {g}_sample s"
                    ),
                )?;
            }
        }
        let rels = if shape.relations { vec!["NULL", "'a'", "'nope'"] } else { vec!["NULL"] };
        for rel in rels {
            same(
                &format!("{g} has_edge {rel}"),
                &format!(
                    "SELECT a.id, b.id, warren.has_edge('{g}', a.id, b.id, {rel})
                     FROM {g}_sample a, {g}_sample b"
                ),
                &format!(
                    "SELECT a.id, b.id, EXISTS (SELECT 1 FROM {g}_arc x
                                               WHERE x.src = a.id AND x.dst = b.id
                                                 AND ({rel} IS NULL OR x.rel = {rel}))
                     FROM {g}_sample a, {g}_sample b"
                ),
            )?;
        }
        Ok(())
    }

    /// `warren.shortest_path`: its length against a depth-bounded CTE, and
    /// every step an arc.
    fn check_shortest_paths(shape: &Shape) -> Result<(), Error> {
        let g = shape.name;
        let start = format!(
            "(SELECT id FROM {g}_starts WHERE id IS NOT NULL ORDER BY md5(id::text) LIMIT 1)"
        );
        for direction in shape.directions() {
            for rels in shape.selections() {
                let adjacency = adjacency(g, direction, rels)?;
                let paths = format!(
                    "SELECT s.id AS target,
                            warren.shortest_path('{g}', {start}, s.id, 6, {rels}, '{direction}') AS p
                     FROM {g}_sample s"
                );
                same(
                    &format!("{g} shortest_path {direction} {rels}"),
                    &format!("SELECT target, array_length(p, 1) - 1 FROM ({paths}) x"),
                    &format!(
                        "WITH RECURSIVE adj AS ({adjacency}),
                         r(n, d) AS (SELECT id, 0 FROM {g}_nodes WHERE id = {start}
                                     UNION SELECT adj.b, r.d + 1 FROM r JOIN adj ON adj.a = r.n
                                     WHERE r.d < 6),
                         depth AS (SELECT n, min(d) AS d FROM r GROUP BY n)
                         SELECT s.id, depth.d::int FROM {g}_sample s LEFT JOIN depth ON depth.n = s.id"
                    ),
                )?;
                let bad_steps = Spi::get_one::<i64>(&format!(
                    "WITH adj AS ({adjacency}), x AS ({paths})
                     SELECT count(*) FROM x, generate_subscripts(x.p, 1) k
                     WHERE (k = 1 AND x.p[1] <> {start})
                        OR (k = array_length(x.p, 1) AND x.p[k] <> x.target)
                        OR (k > 1 AND NOT EXISTS (SELECT 1 FROM adj
                                                  WHERE adj.a = x.p[k - 1] AND adj.b = x.p[k]))"
                ))?;
                assert_eq!(bad_steps, Some(0), "{g} shortest_path {direction} {rels}: steps");
            }
        }
        Ok(())
    }

    /// `warren.dijkstra` costs against Bellman–Ford in PL/pgSQL over the
    /// cheapest arc between each pair, and every path's cost its arcs' sum.
    fn check_dijkstra(shape: &Shape) -> Result<(), Error> {
        let g = shape.name;
        let t = shape.id_type;
        let start = format!(
            "(SELECT id FROM {g}_starts WHERE id IS NOT NULL ORDER BY md5(id::text) LIMIT 1)"
        );
        for direction in shape.directions() {
            for rels in shape.selections() {
                let forward = format!("SELECT src AS a, dst AS b, w, rel FROM {g}_arc");
                let backward = format!("SELECT dst AS a, src AS b, w, rel FROM {g}_arc");
                let directed = match direction {
                    "out" => forward,
                    "in" => backward,
                    _ => format!("{forward} UNION ALL {backward}"),
                };
                // The cheapest arc between each pair over the selected relations.
                Spi::run(&format!(
                    "DROP TABLE IF EXISTS {g}_warc;
                     CREATE TEMP TABLE {g}_warc AS
                         SELECT a, b, min(w) AS w FROM ({directed}) x
                         WHERE {rels} IS NULL OR rel = ANY ({rels}) GROUP BY a, b;
                     CREATE INDEX ON {g}_warc (a, b);
                     ANALYZE {g}_warc;"
                ))?;
                let arcs = format!("SELECT a, b, w FROM {g}_warc");
                Spi::run(&format!(
                    "DROP TABLE IF EXISTS {g}_dist;
                     CREATE TEMP TABLE {g}_dist (n {t} PRIMARY KEY, c float8);
                     INSERT INTO {g}_dist VALUES ({start}, 0);
                     DO $$
                     DECLARE changed bigint;
                     BEGIN
                         LOOP
                             INSERT INTO {g}_dist
                                 SELECT arcs.b, min(d.c + arcs.w) FROM {g}_dist d
                                 JOIN ({arcs}) arcs ON arcs.a = d.n GROUP BY arcs.b
                             ON CONFLICT (n) DO UPDATE SET c = excluded.c
                                 WHERE excluded.c < {g}_dist.c;
                             GET DIAGNOSTICS changed = ROW_COUNT;
                             EXIT WHEN changed = 0;
                         END LOOP;
                     END $$;"
                ))?;
                let found = format!(
                    "SELECT s.id AS target, d.path, d.cost FROM {g}_sample s
                     LEFT JOIN LATERAL warren.dijkstra('{g}', {start}, s.id, {rels}, '{direction}') d
                         ON true"
                );
                let what = format!("{g} dijkstra {direction} {rels}");
                let wrong = Spi::get_one::<String>(&format!(
                    "SELECT string_agg(format('%s: %s vs %s', f.target, f.cost, o.c), ', ')
                     FROM ({found}) f LEFT JOIN {g}_dist o ON o.n = f.target
                     WHERE (f.cost IS NULL) <> (o.c IS NULL)
                        OR abs(f.cost - o.c) > 1e-9 * greatest(1, o.c)"
                ))?;
                assert_eq!(wrong, None, "{what}");
                let bad_paths = Spi::get_one::<i64>(&format!(
                    "WITH arcs AS ({arcs}), f AS ({found})
                     SELECT count(*) FROM f WHERE f.path IS NOT NULL AND abs(f.cost - (
                         SELECT coalesce(sum(arcs.w), 0) FROM generate_subscripts(f.path, 1) k
                         JOIN arcs ON arcs.a = f.path[k - 1] AND arcs.b = f.path[k]
                         WHERE k > 1)) > 1e-9 * greatest(1, f.cost)"
                ))?;
                assert_eq!(bad_paths, Some(0), "{what}: path costs");
            }
        }
        Ok(())
    }

    /// `warren.components` against weakly connected components labelled by
    /// iterated minimum (of the ids as text: any total order will do) in
    /// PL/pgSQL: the two partitions must coincide.
    fn check_components(shape: &Shape) -> Result<(), Error> {
        let g = shape.name;
        let t = shape.id_type;
        adjacency(g, "both", "NULL")?;
        Spi::run(&format!(
            "CREATE TEMP TABLE {g}_label AS
                 SELECT id AS n, id::text AS l FROM {g}_nodes WHERE id IS NOT NULL;
             DO $$
             DECLARE changed bigint;
             BEGIN
                 LOOP
                     UPDATE {g}_label x SET l = m.l FROM (
                         SELECT y.n, min(z.l) AS l FROM {g}_label y
                         JOIN {g}_adj e ON e.a = y.n
                         JOIN {g}_label z ON z.n = e.b GROUP BY y.n) m
                     WHERE m.n = x.n AND m.l < x.l;
                     GET DIAGNOSTICS changed = ROW_COUNT;
                     EXIT WHEN changed = 0;
                 END LOOP;
             END $$;"
        ))?;
        let partition = Spi::get_one::<String>(&format!(
            "WITH c AS (SELECT * FROM warren.components('{g}', NULL::{t}))
             SELECT format('%s %s %s %s', (SELECT count(*) FROM c),
                           (SELECT count(*) FROM {g}_label),
                           (SELECT count(DISTINCT (c.component_id, o.l)) FROM c
                            JOIN {g}_label o ON o.n = c.node_id),
                           (SELECT count(DISTINCT component_id) FROM c)
                           = (SELECT count(DISTINCT l) FROM {g}_label))"
        ))?
        .unwrap_or_default();
        let parts: Vec<&str> = partition.split(' ').collect();
        assert_eq!(parts[0], parts[1], "{g} components: node count {partition}");
        let distinct =
            Spi::get_one::<i64>(&format!("SELECT count(DISTINCT l) FROM {g}_label"))?.unwrap_or(0);
        assert_eq!(parts[2], distinct.to_string(), "{g} components: partition {partition}");
        assert_eq!(parts[3], "t", "{g} components: {partition}");
        Ok(())
    }

    fn check_all(shape: Shape, tombstones: bool) -> Result<(), Error> {
        generate(&shape, tombstones)?;
        check_bfs(&shape)?;
        check_reachability(&shape)?;
        check_neighbourhood(&shape)?;
        check_shortest_paths(&shape)?;
        check_dijkstra(&shape)?;
        check_components(&shape)
    }

    #[pg_test]
    fn oracle_int8_directed_unweighted_without_relations() -> Result<(), Error> {
        check_all(
            Shape {
                name: "o_a",
                id_type: "int8",
                directed: true,
                weighted: false,
                relations: false,
                reverse: true,
                nodes: 2000,
                edges: 5000,
            },
            false,
        )
    }

    #[pg_test]
    fn oracle_int4_undirected_weighted_with_relations() -> Result<(), Error> {
        check_all(
            Shape {
                name: "o_b",
                id_type: "int4",
                directed: false,
                weighted: true,
                relations: true,
                reverse: true,
                nodes: 1500,
                edges: 2500,
            },
            false,
        )
    }

    #[pg_test]
    fn oracle_uuid_directed_weighted_with_relations_without_reverse() -> Result<(), Error> {
        check_all(
            Shape {
                name: "o_c",
                id_type: "uuid",
                directed: true,
                weighted: true,
                relations: true,
                reverse: false,
                nodes: 1500,
                edges: 4000,
            },
            false,
        )
    }

    #[pg_test]
    fn oracle_int8_with_tombstones() -> Result<(), Error> {
        check_all(
            Shape {
                name: "o_d",
                id_type: "int8",
                directed: true,
                weighted: true,
                relations: true,
                reverse: true,
                nodes: 3000,
                edges: 9000,
            },
            true,
        )
    }

    #[pg_test]
    fn oracle_uuid_undirected_unweighted() -> Result<(), Error> {
        check_all(
            Shape {
                name: "o_e",
                id_type: "uuid",
                directed: false,
                weighted: false,
                relations: false,
                reverse: true,
                nodes: 1000,
                edges: 1500,
            },
            false,
        )
    }
}
