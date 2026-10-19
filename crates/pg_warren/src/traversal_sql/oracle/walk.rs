//! The oracle of `warren.walk` (DESIGN.md §9.2, §12): an equivalent chain
//! of recursive CTEs, one per step, each with a depth column (walks, not
//! shortest distances); walk counts by level-by-level sums in SQL; and,
//! when there are few enough, every walk enumerated in SQL with its path
//! and relations. Every mode is checked forwards and in reverse.

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
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

    /// Every walk along `chain` from every node of `origins`, as (origin,
    /// end, path, rels): one recursive CTE per step carrying the walk so far,
    /// with a depth column. A walk that splits the same hops differently
    /// between steps is a different walk, so rows may repeat.
    fn enumerated(g: &str, t: &str, chain: &[S], origins: &str) -> String {
        let mut ctes = vec![format!(
            "w0(o, n, path, rels) AS (
                 SELECT DISTINCT id, id, ARRAY[id]::{t}[], ARRAY[]::text[] FROM ({origins}) x)"
        )];
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
                "s{k}(o, n, d, path, rels) AS (
                     SELECT o, n, 0, path, rels FROM w{prev}
                     UNION ALL
                     SELECT x.o, h.b, x.d + 1, x.path || h.b, x.rels || h.rel
                     FROM s{k} x JOIN {g}_hop{k} h ON h.a = x.n
                     WHERE x.d < {max} {enter}),
                 w{k}(o, n, path, rels) AS (
                     SELECT o, n, path, rels FROM s{k} WHERE d >= {min} {hand_on})",
                max = s.max,
                min = s.min,
            ));
        }
        format!("WITH RECURSIVE {} SELECT o, n, path, rels FROM w{}", ctes.join(", "), chain.len())
    }

    /// Counts the walks along `chain` from every node of `origins` by
    /// summing walk counts level by level; leaves (origin, end, walks) in
    /// `<g>_walks`.
    fn count_walks(g: &str, chain: &[S], origins: &str) -> Result<(), Error> {
        let mut sql = format!(
            "DROP TABLE IF EXISTS {g}_walks;
             CREATE TEMP TABLE {g}_walks AS
                 SELECT DISTINCT id AS o, id AS n, 1::numeric AS c FROM ({origins}) x;"
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
                         SELECT c.o, h.b AS n, sum(c.c) AS c FROM {g}_cur c
                         JOIN {g}_hop{k} h ON h.a = c.n {enter} GROUP BY c.o, h.b;
                     DROP TABLE {g}_cur; ALTER TABLE {g}_next RENAME TO {g}_cur;"
                );
                if depth >= s.min {
                    sql += &format!("INSERT INTO {g}_acc SELECT * FROM {g}_cur;");
                }
            }
            sql += &format!(
                "DROP TABLE {g}_walks;
                 CREATE TEMP TABLE {g}_walks AS
                     SELECT o, n, sum(c) AS c FROM {g}_acc GROUP BY o, n;"
            );
        }
        Spi::run(&sql)
    }

    fn walk(g: &str, steps: &str, rest: &str) -> String {
        format!("warren.walk('{g}', {starts}, {steps}{rest})", starts = starts(g))
    }

    /// The most walks a per-walk check enumerates in SQL (the walk's own
    /// default row limit).
    const ENUMERABLE: i64 = 100_000;

    /// One direction of execution of a chain under test.
    struct Run<'a> {
        what: String,
        g: &'a str,
        steps: String,
        /// The walk's extra arguments: `, reverse := true` or nothing.
        reverse: &'static str,
        /// Distinct (reported node) rows of the oracle.
        reported: String,
        /// (reported node, walks) of the oracle.
        counts: String,
        /// (reported node, path, rels) of every walk, when enumerable.
        walks: Option<String>,
        total: i64,
    }

    impl Run<'_> {
        fn call(&self, rest: &str) -> String {
            walk(self.g, &self.steps, &format!("{rest}{}", self.reverse))
        }

        /// Every mode against the oracle.
        fn check(&self) -> Result<(), Error> {
            let what = &self.what;
            let reported = &self.reported;
            same(what, &format!("SELECT end_id FROM {}", self.call("")), reported)?;
            same(
                &format!("{what} count"),
                &format!("SELECT count FROM {}", self.call(", 'count'")),
                &format!("SELECT count(*) FROM ({reported}) e"),
            )?;
            same(
                &format!("{what} count of walks"),
                &format!("SELECT count FROM {}", self.call(", 'count', distinct_ends := false")),
                &format!("SELECT {}::int8", self.total),
            )?;
            if self.total > ENUMERABLE {
                return Ok(());
            }
            same(
                &format!("{what} ends per walk"),
                &format!(
                    "SELECT end_id, count(*)::numeric FROM {} GROUP BY end_id",
                    self.call(", distinct_ends := false")
                ),
                &self.counts,
            )?;
            let Some(walks) = &self.walks else {
                return Ok(());
            };
            same(
                &format!("{what} every walk"),
                &format!(
                    "SELECT end_id, path, rels FROM {}",
                    self.call(", 'paths', distinct_ends := false")
                ),
                walks,
            )?;
            same(
                &format!("{what} one path per end"),
                &format!("SELECT end_id FROM {}", self.call(", 'paths'")),
                reported,
            )?;
            let strays = Spi::get_one::<i64>(&format!(
                "SELECT count(*) FROM {} p
                 WHERE NOT EXISTS (SELECT 1 FROM ({walks}) w
                                   WHERE w.r = p.end_id AND w.path = p.path AND w.rels = p.rels)",
                self.call(", 'paths'")
            ))?;
            assert_eq!(strays, Some(0), "{what}: a path per end that is not a walk");
            Ok(())
        }
    }

    /// Checks one chain in every mode, forwards and in reverse, and the
    /// jsonb form of its steps.
    fn check_chain(shape: &Shape, chain: &[S]) -> Result<(), Error> {
        let g = shape.name;
        let t = shape.id_type;
        let what = format!("{g} walk of {} steps", chain.len());
        hops(g, chain)?;
        let steps = steps_sql(g, chain);
        let forward = ends_by_origin(g, chain, &live_starts(g));
        same(
            &format!("{what} (jsonb)"),
            &format!("SELECT end_id FROM {}", walk(g, &steps_jsonb(g, chain), "")),
            &format!("SELECT DISTINCT n FROM ({forward}) f"),
        )?;

        let total = |filter: &str| -> Result<i64, Error> {
            Ok(Spi::get_one::<i64>(&format!(
                "SELECT coalesce(sum(c), 0)::int8 FROM {g}_walks {filter}"
            ))?
            .unwrap_or(0))
        };
        count_walks(g, chain, &live_starts(g))?;
        let forward_total = total("")?;
        let forward_walks = (forward_total <= ENUMERABLE).then(|| {
            format!(
                "SELECT n AS r, path, rels FROM ({}) w",
                enumerated(g, t, chain, &live_starts(g))
            )
        });
        if let Some(walks) = &forward_walks {
            Spi::run(&format!(
                "DROP TABLE IF EXISTS {g}_fwd; CREATE TEMP TABLE {g}_fwd AS {walks}"
            ))?;
        }
        Run {
            what: what.clone(),
            g,
            steps: steps.clone(),
            reverse: "",
            reported: format!("SELECT DISTINCT n FROM ({forward}) f"),
            counts: format!("SELECT n, sum(c) FROM {g}_walks GROUP BY n"),
            walks: forward_walks.map(|_| format!("SELECT r, path, rels FROM {g}_fwd")),
            total: forward_total,
        }
        .check()?;
        if !shape.reverse {
            // Reversing a step reads the opposite CSR, which is missing.
            return Ok(());
        }

        // In reverse the starts are the allowed ends and the walk reports
        // the nodes walks start from: the oracle runs from every node.
        let every = format!("SELECT id FROM {g}_nodes WHERE id IS NOT NULL");
        let allowed = format!("n IN (SELECT id FROM {g}_starts)");
        count_walks(g, chain, &every)?;
        let reverse_total = total(&format!("WHERE {allowed}"))?;
        let reverse_walks = (reverse_total <= ENUMERABLE).then(|| {
            format!(
                "SELECT o AS r, path, rels FROM ({}) w WHERE {allowed}",
                enumerated(g, t, chain, &every)
            )
        });
        if let Some(walks) = &reverse_walks {
            Spi::run(&format!(
                "DROP TABLE IF EXISTS {g}_rev; CREATE TEMP TABLE {g}_rev AS {walks}"
            ))?;
        }
        let all = ends_by_origin(g, chain, &every);
        Run {
            what: format!("{what} reverse"),
            g,
            steps,
            reverse: ", reverse := true",
            reported: format!("SELECT DISTINCT o FROM ({all}) f WHERE {allowed}"),
            counts: format!("SELECT o, sum(c) FROM {g}_walks WHERE {allowed} GROUP BY o"),
            walks: reverse_walks.map(|_| format!("SELECT r, path, rels FROM {g}_rev")),
            total: reverse_total,
        }
        .check()
    }

    fn chains(relations: bool, reverse: bool) -> Vec<Vec<S>> {
        let (some, other) =
            if relations { ("ARRAY['a', 'c']", "ARRAY['b']") } else { ("NULL", "ARRAY['']") };
        let chains = vec![
            vec![
                S { rels: some, direction: "out", filter: false, min: 1, max: 1 },
                S { rels: "NULL", direction: "out", filter: true, min: 1, max: 2 },
            ],
            vec![
                S { rels: "NULL", direction: "in", filter: false, min: 0, max: 2 },
                S { rels: other, direction: "both", filter: true, min: 2, max: 2 },
            ],
            vec![S { rels: "NULL", direction: "out", filter: true, min: 1, max: 3 }],
            vec![S { rels: other, direction: "out", filter: false, min: 0, max: 0 }],
            vec![],
        ];
        chains
            .into_iter()
            .filter(|chain| reverse || chain.iter().all(|s| s.direction == "out"))
            .collect()
    }

    fn check_walks(shape: Shape, tombstones: bool) -> Result<(), Error> {
        generate(&shape, tombstones)?;
        for chain in chains(shape.relations, shape.reverse) {
            check_chain(&shape, &chain)?;
        }
        Ok(())
    }

    fn shape(name: &'static str, id_type: &'static str) -> Shape {
        Shape {
            name,
            id_type,
            directed: true,
            weighted: false,
            relations: true,
            reverse: true,
            nodes: 300,
            edges: 700,
        }
    }

    #[pg_test]
    fn walk_oracle_int8_with_relations() -> Result<(), Error> {
        check_walks(shape("w_a", "int8"), false)
    }

    #[pg_test]
    fn walk_oracle_uuid_undirected() -> Result<(), Error> {
        let undirected = Shape {
            directed: false,
            weighted: true,
            relations: false,
            edges: 300,
            ..shape("w_b", "uuid")
        };
        check_walks(undirected, false)
    }

    #[pg_test]
    fn walk_oracle_int4_with_tombstoned_starts() -> Result<(), Error> {
        let tombstoned = shape("w_d", "int4");
        check_walks(tombstoned, true)?;
        let start_tombstoned = Spi::get_one::<bool>(
            "SELECT EXISTS (SELECT 1 FROM w_d_starts s JOIN w_d_ix x ON x.id = s.id AND x.i = 17
                            WHERE NOT EXISTS (SELECT 1 FROM w_d_nodes n WHERE n.id = s.id))",
        )?;
        assert_eq!(start_tombstoned, Some(true), "a start is tombstoned");
        Ok(())
    }

    #[pg_test]
    fn walk_oracle_without_a_reverse_csr() -> Result<(), Error> {
        let forward_only = Shape { reverse: false, ..shape("w_e", "int8") };
        check_walks(forward_only, false)?;
        let refused = crate::fixture::failure(&format!(
            "(SELECT count(*) FROM {})",
            walk("w_e", "ARRAY[warren.s(NULL, 'in')]", "")
        ))?;
        let reversed = crate::fixture::failure(&format!(
            "(SELECT count(*) FROM {})",
            walk("w_e", "ARRAY[warren.s(NULL)]", ", reverse := true")
        ))?;
        for error in [refused, reversed] {
            assert_eq!(
                error.as_deref(),
                Some("55000: direction 'in' or 'both' needs the reverse CSR, and this graph has none")
            );
        }
        Ok(())
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
