//! `warren.reachable`, `warren.shortest_path` and `warren.dijkstra`:
//! questions about a start and a target (DESIGN.md §9.3). A start or
//! target that is not a node of the version reaches nothing, not even
//! itself; a NULL one gives a NULL answer (no row for `dijkstra`).

use pgrx::prelude::*;
use pgrx::{AnyArray, AnyElement};
use warren_core::graph::DenseId;
use warren_core::traversal::{
    dijkstra as cheapest_path, reachable as reaches, shortest_path as shortest, Adjacency,
    Direction, RelationSelection, TraversalStats,
};

use super::args::{bound, direction, relations};
use super::ids::{element, external_array};
use super::interrupts::PostgresInterrupts;
use super::stats::Recorder;
use super::{entry, TraversalSqlError};
use crate::cache::{cached, resolve, LoadedVersion, Resolved};
use crate::source::{with_id_type, IdDatum, IdType};

/// `warren.reachable`: whether `target` is within `max_depth` edges of
/// `start` (unbounded if NULL) in `direction` along `rels`.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.reachable(graph text, start anyelement, target anyelement,
                                 max_depth int DEFAULT NULL, rels text[] DEFAULT NULL,
                                 direction text DEFAULT 'out', version bigint DEFAULT NULL)
RETURNS boolean
STABLE PARALLEL RESTRICTED LANGUAGE c AS 'MODULE_PATHNAME', 'reachable_wrapper';
"#
)]
fn reachable(
    graph: Option<&str>,
    start: Option<AnyElement>,
    target: Option<AnyElement>,
    max_depth: Option<i32>,
    rels: Option<Vec<Option<String>>>,
    direction: Option<&str>,
    version: Option<i64>,
) -> Option<bool> {
    let call = Endpoints { function: "reachable", graph, start, target, rels, direction, version };
    entry(|| {
        let max_depth = bound("max_depth", max_depth.map(i64::from))?;
        let opened = call.open()?;
        let found = with_id_type!(opened.id_type(), E => opened.run::<E, _>(|_, adjacency, start, target| {
            let found = reaches(adjacency, start, target, max_depth, &mut PostgresInterrupts)?;
            Ok((found.result, found.stats))
        })?);
        Ok(found.map(|found| found.unwrap_or(false)))
    })
}

/// `warren.shortest_path`: the nodes along a shortest path of at most
/// `max_depth` edges (unbounded if NULL) from `start` to `target`, both
/// included; NULL if there is none. Bidirectional BFS; ties go to the
/// smallest dense id.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.shortest_path(graph text, start anyelement, target anyelement,
                                     max_depth int DEFAULT NULL, rels text[] DEFAULT NULL,
                                     direction text DEFAULT 'out', version bigint DEFAULT NULL)
RETURNS anyarray
STABLE PARALLEL RESTRICTED LANGUAGE c AS 'MODULE_PATHNAME', 'shortest_path_wrapper';
"#
)]
fn shortest_path(
    graph: Option<&str>,
    start: Option<AnyElement>,
    target: Option<AnyElement>,
    max_depth: Option<i32>,
    rels: Option<Vec<Option<String>>>,
    direction: Option<&str>,
    version: Option<i64>,
) -> Option<AnyArray> {
    let call =
        Endpoints { function: "shortest_path", graph, start, target, rels, direction, version };
    entry(|| {
        let max_depth = bound("max_depth", max_depth.map(i64::from))?;
        let opened = call.open()?;
        let path = with_id_type!(opened.id_type(), E => opened.run::<E, _>(|loaded, adjacency, start, target| {
            let found = shortest(adjacency, start, target, max_depth, &mut PostgresInterrupts)?;
            let path = found.result.and_then(|path| external_array(&loaded.id_map, &path));
            Ok((path, found.stats))
        })?);
        Ok(path.flatten().flatten())
    })
}

/// `warren.dijkstra`: a cheapest path from `start` to `target` and its
/// cost (edge weights; 1 per edge if the graph is unweighted); no row if
/// there is none. An edge in several selected relations costs its cheapest
/// weight.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.dijkstra(graph text, start anyelement, target anyelement,
                                rels text[] DEFAULT NULL, direction text DEFAULT 'out',
                                version bigint DEFAULT NULL)
RETURNS TABLE (path anyarray, cost float8)
STABLE PARALLEL RESTRICTED LANGUAGE c AS 'MODULE_PATHNAME', 'dijkstra_wrapper';
"#
)]
fn dijkstra(
    graph: Option<&str>,
    start: Option<AnyElement>,
    target: Option<AnyElement>,
    rels: Option<Vec<Option<String>>>,
    direction: Option<&str>,
    version: Option<i64>,
) -> TableIterator<'static, (name!(path, AnyArray), name!(cost, f64))> {
    let call = Endpoints { function: "dijkstra", graph, start, target, rels, direction, version };
    entry(|| {
        let opened = call.open()?;
        let row = with_id_type!(opened.id_type(), E => opened.run::<E, _>(|loaded, adjacency, start, target| {
            let found = cheapest_path(adjacency, start, target, &mut PostgresInterrupts)?;
            let row = found.result.and_then(|weighted| {
                Some((external_array(&loaded.id_map, &weighted.path)?, weighted.cost))
            });
            Ok((row, found.stats))
        })?);
        Ok(TableIterator::new(row.flatten().flatten()))
    })
}

/// A call about a start and a target.
struct Endpoints<'a> {
    function: &'static str,
    graph: Option<&'a str>,
    start: Option<AnyElement>,
    target: Option<AnyElement>,
    rels: Option<Vec<Option<String>>>,
    direction: Option<&'a str>,
    version: Option<i64>,
}

/// What a search over two endpoints returns, with the work it did.
type Searched<T> = Result<(T, TraversalStats), TraversalSqlError>;

impl<'a> Endpoints<'a> {
    /// Resolves the graph and version, checking that the caller may read
    /// it, and the direction.
    fn open(self) -> Result<Opened<'a>, TraversalSqlError> {
        let graph = self.graph.ok_or(TraversalSqlError::NullArgument("graph"))?;
        let resolved = resolve(graph, self.version, "read")?;
        let direction = direction(self.direction)?;
        Ok(Opened { graph, resolved, direction, call: self })
    }
}

/// An [`Endpoints`] call with its graph and version resolved.
struct Opened<'a> {
    graph: &'a str,
    resolved: Resolved,
    direction: Direction,
    call: Endpoints<'a>,
}

impl Opened<'_> {
    fn id_type(&self) -> IdType {
        self.resolved.graph.id_type
    }

    /// Records stats under the function's name and runs `search` on the
    /// selected adjacency from start to target. `None` if an endpoint is
    /// NULL; `Some(None)` if one is not a node of the version.
    fn run<E: IdDatum, T>(
        &self,
        search: impl FnOnce(&LoadedVersion<E>, &Adjacency<'_>, DenseId, DenseId) -> Searched<T>,
    ) -> Result<Option<Option<T>>, TraversalSqlError> {
        let mut recorder = Recorder::start(self.call.function, self.graph, self.resolved.version);
        let (Some(start), Some(target)) = (self.call.start, self.call.target) else {
            return Ok(None);
        };
        let start = element::<E>(self.graph, "start", start)?;
        let target = element::<E>(self.graph, "target", target)?;
        let loaded = cached::<E>(self.resolved.graph.graph_id, self.resolved.version)?;
        let (Some(start), Some(target)) =
            (loaded.id_map.dense_id(&start), loaded.id_map.dense_id(&target))
        else {
            return Ok(Some(None));
        };
        let selected = relations(&loaded.relations, self.call.rels.as_deref());
        let selection = selected.as_deref().map_or(RelationSelection::All, RelationSelection::Only);
        let adjacency = Adjacency::new(&loaded.graph, self.direction, selection)?;
        let (result, stats) = search(&loaded, &adjacency, start, target)?;
        recorder.record(stats);
        Ok(Some(Some(result)))
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::small;

    fn one(query: &str) -> Result<Option<String>, Error> {
        Spi::get_one::<String>(query)
    }

    #[pg_test]
    fn reachable_answers_within_a_depth() -> Result<(), Error> {
        small("t")?;
        let found = one("SELECT concat_ws(' ', warren.reachable('t', 1::bigint, 3::bigint),
                              warren.reachable('t', 1::bigint, 3::bigint, 1),
                              warren.reachable('t', 3::bigint, 2::bigint, direction := 'in'),
                              warren.reachable('t', 1::bigint, 3::bigint, rels := ARRAY['b']),
                              warren.reachable('t', 6::bigint, 6::bigint),
                              warren.reachable('t', 99::bigint, 99::bigint),
                              warren.reachable('t', 1::bigint, NULL) IS NULL)")?;
        assert_eq!(found.as_deref(), Some("t f t f t f t"));
        Ok(())
    }

    #[pg_test]
    fn shortest_path_returns_the_external_ids_along_it() -> Result<(), Error> {
        small("t")?;
        let found = one("SELECT concat_ws(' ', warren.shortest_path('t', 2::bigint, 1::bigint),
                              warren.shortest_path('t', 1::bigint, 3::bigint, 1) IS NULL,
                              warren.shortest_path('t', 6::bigint, 6::bigint),
                              warren.shortest_path('t', 1::bigint, 99::bigint) IS NULL,
                              pg_typeof(warren.shortest_path('t', 1::bigint, 2::bigint)))")?;
        assert_eq!(found.as_deref(), Some("{2,3,4,1} t {6} t bigint[]"));
        assert_eq!(
            one("SELECT warren.last_stats() ->> 'function'")?.as_deref(),
            Some("shortest_path")
        );
        Ok(())
    }

    #[pg_test]
    fn dijkstra_returns_the_cheapest_path_and_its_cost() -> Result<(), Error> {
        small("t")?;
        // 1 → 4 directly costs 5; 1 → 2 → 3 → 4 costs 3.
        let found = one("SELECT string_agg(path::text || ':' || cost, ' ')
             FROM warren.dijkstra('t', 1::bigint, 4::bigint)")?;
        assert_eq!(found.as_deref(), Some("{1,2,3,4}:3"));
        let found = one("SELECT string_agg(path::text || ':' || cost, ' ')
             FROM warren.dijkstra('t', 1::bigint, 4::bigint, rels := ARRAY['b'])")?;
        assert_eq!(found.as_deref(), Some("{1,4}:5"));
        let none =
            Spi::get_one::<i64>("SELECT count(*) FROM warren.dijkstra('t', 1::bigint, 6::bigint)")?;
        assert_eq!(none, Some(0));
        Ok(())
    }

    #[pg_test(
        error = "graph \"t\" has node ids of type bigint, but start is of type integer; cast it to bigint"
    )]
    fn endpoints_must_have_the_graphs_id_type() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT warren.shortest_path('t', 1, 2)")
    }
}
