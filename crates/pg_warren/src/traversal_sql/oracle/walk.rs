//! The oracle of `warren.walk` (DESIGN.md §9.2, §12): an equivalent chain
//! of recursive CTEs, one per step, each with a depth column (walks, not
//! shortest distances), and walk counts by level-by-level sums in SQL.

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::super::{generate, same, starts, Shape};

    /// One step of a chain under test.
    #[derive(Clone, Copy)]
    struct S {
        rels: &'static str,
        direction: &'static str,
        filter: bool,
        min: u32,
        max: u32,
    }

    /// The live starts, once each.
    fn live_starts(g: &str) -> String {
        format!("SELECT DISTINCT id FROM {g}_starts WHERE id IN (SELECT id FROM {g}_nodes)")
    }

    fn filter_ids(g: &str) -> String {
        format!("(SELECT array_agg(id) FROM {g}_filter)")
    }

    /// `chain` as a `warren.step[]` built with `warren.s`.
    fn steps_sql(g: &str, chain: &[S]) -> String {
        let steps: Vec<String> = chain
            .iter()
            .map(|s| {
                let filter = if s.filter { filter_ids(g) } else { "NULL".to_owned() };
                format!("warren.s({}, '{}', {filter}, {}, {})", s.rels, s.direction, s.min, s.max)
            })
            .collect();
        format!("ARRAY[{}]::warren.step[]", steps.join(", "))
    }

    /// `chain` as `jsonb` cast to `warren.step[]`.
    fn steps_jsonb(g: &str, chain: &[S]) -> String {
        let steps: Vec<String> = chain
            .iter()
            .map(|s| {
                let filter = match s.filter {
                    true => format!("(SELECT to_jsonb(array_agg(id::text)) FROM {g}_filter)"),
                    false => "NULL::jsonb".to_owned(),
                };
                format!(
                    "jsonb_build_object('rels', to_jsonb({}::text[]), 'direction', '{}',
                                        'filter', {filter}, 'min_hops', {}, 'max_hops', {})",
                    s.rels, s.direction, s.min, s.max
                )
            })
            .collect();
        format!("jsonb_build_array({})::warren.step[]", steps.join(", "))
    }

    /// Materialises each step's hops as `<g>_hop<k> (a, b, rel)`: one row
    /// per (relation, neighbour) pair, as walk identity counts them.
    fn hops(g: &str, chain: &[S]) -> Result<(), Error> {
        for (k, s) in (1..).zip(chain) {
            let rels = s.rels;
            let arcs = format!(
                "SELECT src, dst, rel FROM {g}_arc WHERE {rels} IS NULL OR rel = ANY ({rels})"
            );
            let pairs = match s.direction {
                "out" => format!("SELECT src AS a, dst AS b, rel FROM ({arcs}) x"),
                "in" => format!("SELECT dst AS a, src AS b, rel FROM ({arcs}) x"),
                _ => format!(
                    "SELECT src AS a, dst AS b, rel FROM ({arcs}) x
                     UNION SELECT dst, src, rel FROM ({arcs}) y"
                ),
            };
            Spi::run(&format!(
                "DROP TABLE IF EXISTS {g}_hop{k};
                 CREATE TEMP TABLE {g}_hop{k} AS SELECT DISTINCT * FROM ({pairs}) p;
                 CREATE INDEX ON {g}_hop{k} (a);
                 ANALYZE {g}_hop{k};"
            ))?;
        }
        Ok(())
    }

    /// (origin, end) for every walk along `chain` from every node of
    /// `origins`: one recursive CTE per step, with a depth column.
    fn ends_by_origin(g: &str, chain: &[S], origins: &str) -> String {
        let mut ctes = vec![format!("e0(o, n) AS (SELECT DISTINCT id, id FROM ({origins}) x)")];
        for (k, s) in (1..).zip(chain) {
            let prev = k - 1;
            let (enter, hand_on) = match s.filter {
                true => (
                    format!("AND h.b IN (SELECT id FROM {g}_filter)"),
                    format!("AND n IN (SELECT id FROM {g}_filter)"),
                ),
                false => (String::new(), String::new()),
            };
            ctes.push(format!(
                "s{k}(o, n, d) AS (
                     SELECT o, n, 0 FROM e{prev}
                     UNION
                     SELECT s{k}.o, h.b, s{k}.d + 1 FROM s{k}
                     JOIN (SELECT DISTINCT a, b FROM {g}_hop{k}) h ON h.a = s{k}.n
                     WHERE s{k}.d < {max} {enter}),
                 e{k}(o, n) AS (SELECT DISTINCT o, n FROM s{k} WHERE d >= {min} {hand_on})",
                max = s.max,
                min = s.min,
            ));
        }
        format!("WITH RECURSIVE {} SELECT o, n FROM e{}", ctes.join(", "), chain.len())
    }

    /// Counts the walks along `chain` from the live starts, by summing walk
    /// counts level by level; leaves (end, walks) in `<g>_walks`.
    fn count_walks(g: &str, chain: &[S]) -> Result<(), Error> {
        let mut sql = format!(
            "DROP TABLE IF EXISTS {g}_walks;
             CREATE TEMP TABLE {g}_walks AS SELECT id AS n, 1::numeric AS c FROM ({starts}) x;",
            starts = live_starts(g)
        );
        for (k, s) in (1..).zip(chain) {
            let enter = match s.filter {
                true => format!("WHERE h.b IN (SELECT id FROM {g}_filter)"),
                false => String::new(),
            };
            let hand_on = match s.filter {
                true => format!("WHERE n IN (SELECT id FROM {g}_filter)"),
                false => String::new(),
            };
            sql += &format!(
                "DROP TABLE IF EXISTS {g}_cur; DROP TABLE IF EXISTS {g}_acc;
                 CREATE TEMP TABLE {g}_cur AS SELECT * FROM {g}_walks;
                 CREATE TEMP TABLE {g}_acc AS SELECT * FROM {g}_walks WHERE false;"
            );
            if s.min == 0 {
                sql += &format!("INSERT INTO {g}_acc SELECT * FROM {g}_cur {hand_on};");
            }
            for depth in 1..=s.max {
                sql += &format!(
                    "CREATE TEMP TABLE {g}_next AS
                         SELECT h.b AS n, sum(c.c) AS c FROM {g}_cur c
                         JOIN {g}_hop{k} h ON h.a = c.n {enter} GROUP BY h.b;
                     DROP TABLE {g}_cur; ALTER TABLE {g}_next RENAME TO {g}_cur;"
                );
                if depth >= s.min {
                    sql += &format!("INSERT INTO {g}_acc SELECT * FROM {g}_cur;");
                }
            }
            sql += &format!(
                "DROP TABLE {g}_walks;
                 CREATE TEMP TABLE {g}_walks AS SELECT n, sum(c) AS c FROM {g}_acc GROUP BY n;"
            );
        }
        Spi::run(&sql)
    }

    fn walk(g: &str, steps: &str, rest: &str) -> String {
        format!("warren.walk('{g}', {starts}, {steps}{rest})", starts = starts(g))
    }

    /// Checks one chain in every mode, forwards and in reverse.
    fn check_chain(shape: &Shape, chain: &[S]) -> Result<(), Error> {
        let g = shape.name;
        let what = format!("{g} walk of {} steps", chain.len());
        hops(g, chain)?;
        let steps = steps_sql(g, chain);
        let forward = ends_by_origin(g, chain, &live_starts(g));
        let ends = format!("SELECT DISTINCT n FROM ({forward}) f");

        same(&what, &format!("SELECT end_id FROM {}", walk(g, &steps, "")), &ends)?;
        let jsonb = steps_jsonb(g, chain);
        same(
            &format!("{what} (jsonb)"),
            &format!("SELECT end_id FROM {}", walk(g, &jsonb, "")),
            &ends,
        )?;
        same(
            &format!("{what} count"),
            &format!("SELECT count FROM {}", walk(g, &steps, ", 'count'")),
            &format!("SELECT count(*) FROM ({ends}) e"),
        )?;

        count_walks(g, chain)?;
        let total =
            Spi::get_one::<i64>(&format!("SELECT coalesce(sum(c), 0)::int8 FROM {g}_walks"))?
                .unwrap_or(0);
        same(
            &format!("{what} count of walks"),
            &format!("SELECT count FROM {}", walk(g, &steps, ", 'count', distinct_ends := false")),
            &format!("SELECT {total}::int8"),
        )?;
        if total <= 100_000 {
            same(
                &format!("{what} ends per walk"),
                &format!(
                    "SELECT end_id, count(*)::numeric FROM {} GROUP BY end_id",
                    walk(g, &steps, ", distinct_ends := false")
                ),
                &format!("SELECT n, c FROM {g}_walks"),
            )?;
        }
        check_paths(shape, chain, &steps, &ends, total)?;

        // In reverse, the starts are the allowed ends and the result is the
        // nodes walks start from: oracle from every node.
        let every = format!("SELECT id FROM {g}_nodes WHERE id IS NOT NULL");
        let all = ends_by_origin(g, chain, &every);
        same(
            &format!("{what} reverse"),
            &format!("SELECT end_id FROM {}", walk(g, &steps, ", reverse := true")),
            &format!("SELECT DISTINCT o FROM ({all}) f WHERE n IN (SELECT id FROM {g}_starts)"),
        )?;
        let bad = Spi::get_one::<i64>(&format!(
            "SELECT count(*) FROM {}
             WHERE path[1] <> end_id OR NOT path[array_length(path, 1)] IN (SELECT id FROM {g}_starts)",
            walk(g, &steps, ", 'paths', reverse := true")
        ))?;
        assert_eq!(bad, Some(0), "{what} reverse paths");
        Ok(())
    }

    /// Paths: one per distinct end, or every walk, each a valid walk.
    fn check_paths(
        shape: &Shape,
        chain: &[S],
        steps: &str,
        ends: &str,
        total: i64,
    ) -> Result<(), Error> {
        let g = shape.name;
        let what = format!("{g} walk paths of {} steps", chain.len());
        same(&what, &format!("SELECT end_id FROM {}", walk(g, steps, ", 'paths'")), ends)?;
        let min: u32 = chain.iter().map(|s| s.min).sum();
        let max: u32 = chain.iter().map(|s| s.max).sum();
        let hops = match chain.len() {
            0 => format!(
                "SELECT NULL::{t} AS a, NULL::{t} AS b, NULL::text AS rel WHERE false",
                t = shape.id_type
            ),
            n => (1..=n)
                .map(|k| format!("SELECT * FROM {g}_hop{k}"))
                .collect::<Vec<_>>()
                .join(" UNION "),
        };
        for rest in [", 'paths'", ", 'paths', distinct_ends := false"] {
            let bad = Spi::get_one::<i64>(&format!(
                "WITH p AS (SELECT * FROM {walk}), hop AS ({hops})
                 SELECT count(*) FROM p
                 WHERE NOT path[1] IN ({starts})
                    OR path[array_length(path, 1)] <> end_id
                    OR array_length(path, 1) - 1 NOT BETWEEN {min} AND {max}
                    OR coalesce(array_length(rels, 1), 0) <> array_length(path, 1) - 1
                    OR EXISTS (SELECT 1 FROM generate_subscripts(rels, 1) k
                               WHERE NOT EXISTS (SELECT 1 FROM hop
                                                 WHERE hop.a = path[k] AND hop.b = path[k + 1]
                                                   AND hop.rel = rels[k]))",
                walk = walk(g, steps, rest),
                starts = live_starts(g),
            ))?;
            assert_eq!(bad, Some(0), "{what}{rest}: invalid paths");
        }
        if total <= 100_000 {
            let rows = Spi::get_one::<String>(&format!(
                "SELECT count(*) || ' ' || count(DISTINCT (path, rels)) FROM {}",
                walk(g, steps, ", 'paths', distinct_ends := false")
            ))?;
            assert_eq!(rows, Some(format!("{total} {total}")), "{what}: every walk once");
        }
        Ok(())
    }

    fn chains(relations: bool) -> Vec<Vec<S>> {
        let (some, other) =
            if relations { ("ARRAY['a', 'c']", "ARRAY['b']") } else { ("NULL", "ARRAY['']") };
        vec![
            vec![
                S { rels: some, direction: "out", filter: false, min: 1, max: 1 },
                S { rels: "NULL", direction: "out", filter: true, min: 1, max: 2 },
            ],
            vec![
                S { rels: "NULL", direction: "in", filter: false, min: 0, max: 2 },
                S { rels: other, direction: "both", filter: true, min: 2, max: 2 },
            ],
            vec![S { rels: "NULL", direction: "out", filter: true, min: 1, max: 3 }],
            vec![],
        ]
    }

    fn check_walks(shape: Shape) -> Result<(), Error> {
        generate(&shape, false)?;
        for chain in chains(shape.relations) {
            check_chain(&shape, &chain)?;
        }
        Ok(())
    }

    #[pg_test]
    fn walk_oracle_int8_with_relations() -> Result<(), Error> {
        check_walks(Shape {
            name: "w_a",
            id_type: "int8",
            directed: true,
            weighted: false,
            relations: true,
            reverse: true,
            nodes: 300,
            edges: 700,
        })
    }

    #[pg_test]
    fn walk_oracle_uuid_undirected() -> Result<(), Error> {
        check_walks(Shape {
            name: "w_b",
            id_type: "uuid",
            directed: false,
            weighted: true,
            relations: false,
            reverse: true,
            nodes: 250,
            edges: 300,
        })
    }

    /// Appendix A.4: a step filter prunes the walk rather than its output,
    /// so the walk visits fewer nodes (`last_stats()`).
    #[pg_test]
    fn a_step_filter_reduces_the_nodes_visited() -> Result<(), Error> {
        let shape = Shape {
            name: "w_c",
            id_type: "int8",
            directed: true,
            weighted: false,
            relations: true,
            reverse: true,
            nodes: 300,
            edges: 700,
        };
        generate(&shape, false)?;
        let visited = |filter: bool| -> Result<Option<i64>, Error> {
            let chain = [S { rels: "NULL", direction: "out", filter, min: 1, max: 3 }];
            Spi::run(&format!(
                "SELECT count(*) FROM {}",
                walk("w_c", &steps_sql("w_c", &chain), "")
            ))?;
            Spi::get_one::<i64>("SELECT (warren.last_stats() ->> 'nodes_visited')::int8")
        };
        let (filtered, unfiltered) = (visited(true)?, visited(false)?);
        assert!(
            filtered < unfiltered,
            "{filtered:?} nodes visited with the filter, {unfiltered:?} without"
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT warren.last_stats() ->> 'function'")?.as_deref(),
            Some("walk")
        );
        Ok(())
    }
}
