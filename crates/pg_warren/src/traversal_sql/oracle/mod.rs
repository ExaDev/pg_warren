//! Oracle tests (DESIGN.md §12): every traversal function against an
//! equivalent recursive CTE or plain SQL over the source tables, on random
//! graphs generated in SQL, across id types, directed and undirected,
//! weighted and unweighted, with and without relations and reverse CSRs,
//! with node filters, tombstones, unknown and NULL ids.
//!
//! Each graph `<g>` has tables `<g>_ix (i, id)` (every id, including some
//! that are not nodes), `<g>_nodes (id)` and `<g>_edges (src, dst, rel, w)`,
//! and a table `<g>_arc (src, dst, rel, w)`: the arcs the build stores,
//! derived in SQL from the build's rules (§5.4: edges with an unknown or
//! NULL endpoint, or a NULL relation or weight the graph needs, dropped;
//! undirected edges both ways, a self-loop once; parallel edges per
//! relation folded with weights summed). The oracles read only these.

use pgrx::prelude::*;
use pgrx::spi::Error;

mod traversal;
mod walk;

/// A random graph's shape.
#[derive(Clone, Copy)]
struct Shape {
    name: &'static str,
    id_type: &'static str,
    directed: bool,
    weighted: bool,
    relations: bool,
    reverse: bool,
    nodes: i64,
    edges: i64,
}

impl Shape {
    /// The SQL that maps index `i` (an integer) to an id.
    fn id_of(&self, i: &str) -> String {
        match self.id_type {
            "uuid" => format!("md5('n' || ({i})::text)::uuid"),
            "int4" => format!("(({i}) * 7919 % 1000003 - 500000)::int4"),
            _ => format!("(({i})::int8 * 104729 % 1000000007 - 500000000)::int8"),
        }
    }

    /// The relation selections worth checking.
    fn selections(&self) -> Vec<&'static str> {
        match self.relations {
            true => vec!["NULL", "ARRAY['a', 'c']", "ARRAY['b', 'nope']"],
            false => vec!["NULL", "ARRAY['']"],
        }
    }

    /// The directions the graph can be read in.
    fn directions(&self) -> Vec<&'static str> {
        match self.reverse {
            true => vec!["out", "in", "both"],
            false => vec!["out"],
        }
    }
}

/// Creates the tables, graph and arc view of `shape`, builds it, and
/// (if `tombstones`) removes some nodes and builds it again.
fn generate(shape: &Shape, tombstones: bool) -> Result<(), Error> {
    let Shape { name: g, id_type: t, nodes: n, edges: m, .. } = *shape;
    let id_i = shape.id_of("i");
    let id_x = shape.id_of("x");
    let id_y = shape.id_of("y");
    Spi::run(&format!(
        "SELECT setseed(0.42);
         CREATE TABLE {g}_ix AS SELECT i, {id_i} AS id FROM generate_series(1, {n}) i;
         -- Ids with i % 50 = 0 are not nodes: edges to them are dropped.
         CREATE TABLE {g}_nodes AS SELECT id FROM {g}_ix WHERE i % 50 <> 0;
         INSERT INTO {g}_nodes VALUES (NULL);
         CREATE TABLE {g}_edges (src {t}, dst {t}, rel text, w float8);
         INSERT INTO {g}_edges
             SELECT {id_x}, {id_y}, (ARRAY['a', 'b', 'c'])[1 + floor(random() * 3)::int],
                    round((0.1 + random() * 9)::numeric, 2)::float8
             FROM (SELECT 1 + floor(random() * {n})::int AS x,
                          1 + floor(random() * {n})::int AS y
                   FROM generate_series(1, {m})) r;
         -- Parallel edges, self-loops, NULL fields.
         INSERT INTO {g}_edges SELECT * FROM {g}_edges ORDER BY src, dst LIMIT {m} / 20;
         INSERT INTO {g}_edges SELECT src, src, rel, w FROM {g}_edges ORDER BY dst LIMIT 10;
         INSERT INTO {g}_edges SELECT src, NULL, rel, w FROM {g}_edges LIMIT 3;
         INSERT INTO {g}_edges SELECT src, dst, NULL, w FROM {g}_edges ORDER BY w LIMIT 3;
         INSERT INTO {g}_edges SELECT src, dst, rel, NULL FROM {g}_edges ORDER BY rel LIMIT 3;
         SELECT warren.create_graph('{g}', 'SELECT id FROM {g}_nodes',
             'SELECT src, dst{rel}{w} FROM {g}_edges', directed := {directed},
             weighted := {weighted}, has_relations := {relations},
             build_reverse := {reverse});
         SELECT warren.refresh('{g}');",
        rel = if shape.relations { ", rel" } else { "" },
        w = if shape.weighted { ", w" } else { "" },
        directed = shape.directed,
        weighted = shape.weighted,
        relations = shape.relations,
        reverse = shape.reverse,
    ))?;
    if tombstones {
        Spi::run(&format!(
            "DELETE FROM {g}_nodes WHERE id IN (SELECT id FROM {g}_ix WHERE i % 17 = 0);
             SELECT warren.refresh('{g}');"
        ))?;
    }
    let valid = format!(
        "SELECT src, dst, {rel} AS rel, {w} AS w FROM {g}_edges
         WHERE src IN (SELECT id FROM {g}_nodes) AND dst IN (SELECT id FROM {g}_nodes)
           {rel_check} {w_check}",
        rel = if shape.relations { "rel" } else { "''" },
        w = if shape.weighted { "w" } else { "1::float8" },
        rel_check = if shape.relations { "AND rel IS NOT NULL" } else { "" },
        w_check = if shape.weighted { "AND w IS NOT NULL" } else { "" },
    );
    let both_ways = match shape.directed {
        true => "SELECT * FROM valid".to_owned(),
        false => "SELECT * FROM valid UNION ALL
                  SELECT dst, src, rel, w FROM valid WHERE src <> dst"
            .to_owned(),
    };
    // Unweighted parallel edges are dropped, weighted ones summed.
    let sum = if shape.weighted { "sum(w)" } else { "1::float8" };
    Spi::run(&format!(
        "CREATE TABLE {g}_arc AS
             WITH valid AS ({valid}), stored AS ({both_ways})
             SELECT src, dst, rel, {sum} AS w FROM stored GROUP BY src, dst, rel;
         CREATE INDEX ON {g}_arc (src, dst);
         ANALYZE {g}_arc;
         -- Starts: three nodes, an id that is not a node, NULL, a repeat.
         CREATE TABLE {g}_starts AS
             (SELECT id FROM {g}_nodes WHERE id IS NOT NULL ORDER BY md5(id::text) LIMIT 3)
             UNION ALL (SELECT id FROM {g}_ix WHERE i = 50)
             UNION ALL (SELECT NULL)
             UNION ALL (SELECT id FROM {g}_nodes WHERE id IS NOT NULL
                        ORDER BY md5(id::text) LIMIT 1);
         CREATE TABLE {g}_filter AS
             SELECT id FROM {g}_ix WHERE i % 5 <> 0 OR i % 50 = 0;
         CREATE TABLE {g}_sample AS
             SELECT id FROM {g}_ix ORDER BY md5(id::text || 's') LIMIT 60;"
    ))
}

/// Materialises the (a, b) pairs one step apart in `direction` along
/// `rels` as table `<g>_adj`, and returns a query over it.
fn adjacency(g: &str, direction: &str, rels: &str) -> Result<String, Error> {
    let arcs = format!("SELECT src, dst FROM {g}_arc WHERE {rels} IS NULL OR rel = ANY ({rels})");
    let pairs = match direction {
        "out" => format!("SELECT DISTINCT src AS a, dst AS b FROM ({arcs}) x"),
        "in" => format!("SELECT DISTINCT dst AS a, src AS b FROM ({arcs}) x"),
        _ => format!(
            "SELECT src AS a, dst AS b FROM ({arcs}) x
             UNION SELECT dst, src FROM ({arcs}) y"
        ),
    };
    Spi::run(&format!(
        "DROP TABLE IF EXISTS {g}_adj;
         CREATE TEMP TABLE {g}_adj AS {pairs};
         CREATE INDEX ON {g}_adj (a, b);
         ANALYZE {g}_adj;"
    ))?;
    Ok(format!("SELECT a, b FROM {g}_adj"))
}

fn starts(g: &str) -> String {
    format!("(SELECT array_agg(id) FROM {g}_starts)")
}

/// Asserts that queries `a` and `b` return the same rows, duplicates
/// included.
fn same(what: &str, a: &str, b: &str) -> Result<(), Error> {
    let diff = Spi::get_one::<String>(&format!(
        "SELECT format('%s rows vs %s; only first: %s; only second: %s',
                       (SELECT count(*) FROM ({a}) x), (SELECT count(*) FROM ({b}) y),
                       (SELECT string_agg(z::text, ' ') FROM
                           (SELECT * FROM (({a}) EXCEPT ALL ({b})) d LIMIT 5) z),
                       (SELECT string_agg(z::text, ' ') FROM
                           (SELECT * FROM (({b}) EXCEPT ALL ({a})) d LIMIT 5) z))"
    ))?
    .unwrap_or_default();
    let rows = Spi::get_one::<i64>(&format!(
        "SELECT count(*) FROM ((({a}) EXCEPT ALL ({b})) UNION ALL (({b}) EXCEPT ALL ({a}))) d"
    ))?;
    assert_eq!(rows, Some(0), "{what}: {diff}");
    Ok(())
}
