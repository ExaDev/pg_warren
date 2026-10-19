//! `warren.bfs` and `warren.reachable_set`: breadth-first search from a set
//! of starts, streamed one level at a time (DESIGN.md §9.3).
//!
//! Both are set-returning functions over a [`BfsCursor`] that holds an
//! `Arc` of the cached version: each fetch that finds no buffered row runs
//! the next level, so SQL `LIMIT` stops the scan within a level of where
//! it stopped asking, and `warren.evict` during the scan is safe.

use std::borrow::Borrow;
use std::ops::ControlFlow;
use std::sync::Arc;

use pgrx::prelude::*;
use pgrx::{AnyArray, AnyElement};
use warren_core::graph::{GraphCsr, GraphError, IdMap};
use warren_core::traversal::{BfsCursor, Limits, NodeSet, TraversalError, Visit};

use super::args::{bound, direction, relations};
use super::ids::{dense_ids, elements, external};
use super::stats::Recorder;
use super::{entry, TraversalSqlError};
use crate::cache::{cached, resolve, LoadedVersion};
use crate::interrupts::PostgresInterrupts;
use crate::source::{with_id_type, IdDatum};

/// A row of `warren.bfs`.
type BfsRow = (name!(node_id, AnyElement), name!(depth, i32), name!(via, Option<AnyElement>));

/// `warren.bfs`: every node within `max_depth` edges of `start` (unbounded
/// if NULL), once each at its shortest depth, with the node it was first
/// reached from (NULL for a start), in BFS order. Only nodes in
/// `node_filter`, when given, are admitted beyond the starts. At most
/// `max_results` rows; stopping there is truncation (`NOTICE`,
/// `last_stats()`).
#[allow(clippy::too_many_arguments)] // the SQL signature of DESIGN.md §9.3
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.bfs(graph text, start anyarray, max_depth int,
                           direction text DEFAULT 'out', rels text[] DEFAULT NULL,
                           node_filter "any" DEFAULT NULL, max_results bigint DEFAULT NULL,
                           version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, depth int, via anyelement)
STABLE PARALLEL RESTRICTED LANGUAGE c AS 'MODULE_PATHNAME', 'bfs_wrapper';
"#
)]
fn bfs(
    graph: Option<&str>,
    start: Option<AnyArray>,
    max_depth: Option<i32>,
    direction: Option<&str>,
    rels: Option<Vec<Option<String>>>,
    node_filter: Option<AnyArray>,
    max_results: Option<i64>,
    version: Option<i64>,
) -> TableIterator<
    'static,
    (name!(node_id, AnyElement), name!(depth, i32), name!(via, Option<AnyElement>)),
> {
    entry(|| {
        let request = Request {
            function: "bfs",
            graph: graph.ok_or(TraversalSqlError::NullArgument("graph"))?,
            start,
            max_depth,
            direction,
            rels,
            node_filter,
            max_results,
        };
        let resolved = resolve(request.graph, version, "read")?;
        let rows: Box<dyn Iterator<Item = BfsRow>> = with_id_type!(resolved.graph.id_type, E => {
            let loaded = cached::<E>(&resolved)?;
            Box::new(LevelRows::new(&request, resolved.version, loaded, bfs_row::<E>)?)
        });
        Ok(TableIterator::new(rows))
    })
}

/// `warren.reachable_set`: every node within `max_depth` edges of `start`
/// (unbounded if NULL), the starts included, once each, in BFS order.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.reachable_set(graph text, start anyarray, max_depth int DEFAULT NULL,
                                     rels text[] DEFAULT NULL, direction text DEFAULT 'out',
                                     version bigint DEFAULT NULL)
RETURNS SETOF anyelement
STABLE PARALLEL RESTRICTED LANGUAGE c AS 'MODULE_PATHNAME', 'reachable_set_wrapper';
"#
)]
fn reachable_set(
    graph: Option<&str>,
    start: Option<AnyArray>,
    max_depth: Option<i32>,
    rels: Option<Vec<Option<String>>>,
    direction: Option<&str>,
    version: Option<i64>,
) -> SetOfIterator<'static, AnyElement> {
    entry(|| {
        let request = Request {
            function: "reachable_set",
            graph: graph.ok_or(TraversalSqlError::NullArgument("graph"))?,
            start,
            max_depth,
            direction,
            rels,
            node_filter: None,
            max_results: None,
        };
        let resolved = resolve(request.graph, version, "read")?;
        let rows: Box<dyn Iterator<Item = AnyElement>> = with_id_type!(resolved.graph.id_type, E => {
            let loaded = cached::<E>(&resolved)?;
            Box::new(LevelRows::new(&request, resolved.version, loaded, node_row::<E>)?)
        });
        Ok(SetOfIterator::new(rows))
    })
}

/// The arguments of a BFS-shaped call.
struct Request<'a> {
    function: &'static str,
    graph: &'a str,
    start: Option<AnyArray>,
    max_depth: Option<i32>,
    direction: Option<&'a str>,
    rels: Option<Vec<Option<String>>>,
    node_filter: Option<AnyArray>,
    max_results: Option<i64>,
}

fn bfs_row<E: IdDatum>(id_map: &IdMap<E>, visit: Visit) -> Option<BfsRow> {
    let via = visit.via.and_then(|via| external(id_map, via));
    let depth = i32::try_from(visit.depth).unwrap_or(i32::MAX);
    Some((external(id_map, visit.node)?, depth, via))
}

fn node_row<E: IdDatum>(id_map: &IdMap<E>, visit: Visit) -> Option<AnyElement> {
    external(id_map, visit.node)
}

/// A cached version as the graph a cursor reads.
struct VersionGraph<E>(Arc<LoadedVersion<E>>);

impl<E> Borrow<GraphCsr> for VersionGraph<E> {
    fn borrow(&self) -> &GraphCsr {
        &self.0.graph
    }
}

/// The rows of a BFS, produced a level at a time.
struct LevelRows<E, R> {
    loaded: Arc<LoadedVersion<E>>,
    cursor: BfsCursor<VersionGraph<E>>,
    row: fn(&IdMap<E>, Visit) -> Option<R>,
    /// The visits of the current level not yet returned, in reverse.
    level: Vec<Visit>,
    more: bool,
    recorder: Recorder,
}

impl<E: IdDatum, R> LevelRows<E, R> {
    fn new(
        request: &Request<'_>,
        version: i64,
        loaded: Arc<LoadedVersion<E>>,
        row: fn(&IdMap<E>, Visit) -> Option<R>,
    ) -> Result<Self, TraversalSqlError> {
        let direction = direction(request.direction)?;
        let relations = relations(&loaded.relations, request.rels.as_deref());
        let limits = Limits {
            max_depth: bound("max_depth", request.max_depth.map(i64::from))?,
            max_results: bound("max_results", request.max_results)?,
        };
        let starts = match request.start {
            Some(start) => elements::<E>(request.graph, "start", start)?,
            None => Vec::new(),
        };
        let starts = dense_ids(&loaded.id_map, &starts);
        let filter = match request.node_filter {
            Some(filter) => {
                let members = elements::<E>(request.graph, "node_filter", filter)?;
                let id_space = loaded.id_map.id_space();
                Some(NodeSet::from_dense_ids(id_space, dense_ids(&loaded.id_map, &members))?)
            }
            None => None,
        };
        let graph = VersionGraph(Arc::clone(&loaded));
        let cursor = BfsCursor::new(graph, direction, relations, starts, filter, limits)?;
        let recorder = Recorder::start(request.function, request.graph, version);
        Ok(Self { loaded, cursor, row, level: Vec::new(), more: true, recorder })
    }

    /// Runs the next level into `self.level`.
    fn next_level(&mut self) -> Result<(), TraversalSqlError> {
        let mut visits = Vec::new();
        let mut out_of_memory = false;
        self.more = self.cursor.next_level(&mut PostgresInterrupts, |visit| {
            if visits.try_reserve(1).is_err() {
                out_of_memory = true;
                return ControlFlow::Break(());
            }
            visits.push(visit);
            ControlFlow::Continue(())
        })?;
        self.recorder.record(self.cursor.stats());
        if out_of_memory {
            let elements = visits.len() + 1;
            return Err(TraversalError::from(GraphError::OutOfMemory { elements }).into());
        }
        visits.reverse();
        self.level = visits;
        Ok(())
    }
}

impl<E: IdDatum, R> Iterator for LevelRows<E, R> {
    type Item = R;

    fn next(&mut self) -> Option<R> {
        loop {
            if let Some(visit) = self.level.pop() {
                return (self.row)(&self.loaded.id_map, visit);
            }
            if !self.more {
                return None;
            }
            entry(|| self.next_level());
        }
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{rows, small};

    fn bfs(args: &str) -> Result<Option<String>, Error> {
        rows(&format!(
            "SELECT node_id || ':' || depth || ':' || coalesce(via::text, '-')
             FROM warren.bfs({args})"
        ))
    }

    fn last_stats(key: &str) -> Result<Option<String>, Error> {
        Spi::get_one::<String>(&format!("SELECT warren.last_stats() ->> '{key}'"))
    }

    #[pg_test]
    fn bfs_reports_nodes_with_depth_and_via() -> Result<(), Error> {
        small("t")?;
        assert_eq!(bfs("'t', ARRAY[1::bigint], 3")?.as_deref(), Some("1:0:- 2:1:1 4:1:1 3:2:2"));
        assert_eq!(bfs("'t', ARRAY[1::bigint], 1")?.as_deref(), Some("1:0:- 2:1:1 4:1:1"));
        assert_eq!(bfs("'t', ARRAY[1::bigint], NULL")?.as_deref(), Some("1:0:- 2:1:1 4:1:1 3:2:2"));
        Ok(())
    }

    #[pg_test]
    fn bfs_follows_direction_and_relations() -> Result<(), Error> {
        small("t")?;
        assert_eq!(
            bfs("'t', ARRAY[1::bigint], 9, 'in'")?.as_deref(),
            Some("1:0:- 4:1:1 3:2:4 2:3:3")
        );
        assert_eq!(
            bfs("'t', ARRAY[1::bigint], 9, rels := ARRAY['a']")?.as_deref(),
            Some("1:0:- 2:1:1 3:2:2")
        );
        assert_eq!(
            bfs("'t', ARRAY[3::bigint], 9, 'both', ARRAY['b', 'nope']")?.as_deref(),
            Some("3:0:- 4:1:3 1:2:4")
        );
        Ok(())
    }

    #[pg_test]
    fn the_node_filter_prunes_the_search_but_not_the_starts() -> Result<(), Error> {
        small("t")?;
        let found = bfs("'t', ARRAY[1::bigint], 9, node_filter := ARRAY[2, 3]::bigint[]")?;
        assert_eq!(found.as_deref(), Some("1:0:- 2:1:1 3:2:2"));
        let found = bfs("'t', ARRAY[1::bigint], 9, node_filter := ARRAY[]::bigint[]")?;
        assert_eq!(found.as_deref(), Some("1:0:-"));
        Ok(())
    }

    #[pg_test]
    fn unknown_and_null_starts_are_skipped() -> Result<(), Error> {
        small("t")?;
        let found = bfs("'t', ARRAY[99, NULL, 6, 6]::bigint[], 2")?;
        assert_eq!(found.as_deref(), Some("6:0:-"));
        assert_eq!(bfs("'t', NULL::bigint[], 2")?.as_deref(), Some(""));
        Ok(())
    }

    #[pg_test]
    fn max_results_truncates_and_says_so() -> Result<(), Error> {
        small("t")?;
        assert_eq!(
            bfs("'t', ARRAY[1::bigint], 9, max_results := 2")?.as_deref(),
            Some("1:0:- 2:1:1")
        );
        assert_eq!(last_stats("truncated")?.as_deref(), Some("true"));
        assert_eq!(last_stats("function")?.as_deref(), Some("bfs"));
        assert_eq!(
            bfs("'t', ARRAY[1::bigint], 9, max_results := 4")?.as_deref().map(str::len),
            Some(23)
        );
        assert_eq!(last_stats("truncated")?.as_deref(), Some("false"));
        Ok(())
    }

    #[pg_test]
    fn sql_limit_in_the_select_list_stops_the_scan_within_a_level() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE chain_nodes AS SELECT generate_series(1, 1000)::bigint AS id;
             CREATE TABLE chain_edges AS SELECT id AS src, id + 1 AS dst FROM chain_nodes;
             SELECT warren.create_graph('chain', 'SELECT id FROM chain_nodes',
                                        'SELECT src, dst FROM chain_edges');
             SELECT warren.refresh('chain')",
        )?;
        // Postgres runs a set-returning function in FROM to completion into a
        // tuplestore; in the select list it fetches one row at a time.
        let found = rows(
            "SELECT n::text FROM (SELECT warren.reachable_set('chain', ARRAY[1::bigint]) LIMIT 3) s(n)",
        )?;
        assert_eq!(found.as_deref(), Some("1 2 3"));
        assert_eq!(last_stats("nodes_visited")?.as_deref(), Some("3"));
        let found = rows(
            "SELECT (b).node_id::text
             FROM (SELECT warren.bfs('chain', ARRAY[1::bigint], NULL) LIMIT 2) s(b)",
        )?;
        assert_eq!(found.as_deref(), Some("1 2"));
        assert_eq!(last_stats("nodes_visited")?.as_deref(), Some("2"));
        assert_eq!(last_stats("truncated")?.as_deref(), Some("false"));
        Ok(())
    }

    #[pg_test]
    fn reachable_set_includes_the_starts() -> Result<(), Error> {
        small("t")?;
        let found = rows("SELECT n::text FROM warren.reachable_set('t', ARRAY[2::bigint, 5]) n")?;
        assert_eq!(found.as_deref(), Some("2 5 3 4 1"));
        let found = rows("SELECT n::text FROM warren.reachable_set('t', ARRAY[2::bigint], 1) n")?;
        assert_eq!(found.as_deref(), Some("2 3"));
        assert_eq!(last_stats("function")?.as_deref(), Some("reachable_set"));
        Ok(())
    }

    #[pg_test]
    fn an_explicit_version_reads_that_version() -> Result<(), Error> {
        let first = small("t")?;
        Spi::run(
            "DELETE FROM t_edges WHERE src = 1; SELECT warren.refresh('t', promote := false)",
        )?;
        assert_eq!(bfs("'t', ARRAY[1::bigint], 1")?.as_deref(), Some("1:0:- 2:1:1 4:1:1"));
        let staged =
            Spi::get_one::<i64>("SELECT max(version_id) FROM warren.versions")?.unwrap_or(0);
        assert_eq!(
            bfs(&format!("'t', ARRAY[1::bigint], 1, version := {staged}"))?.as_deref(),
            Some("1:0:-")
        );
        assert_eq!(last_stats("version")?, Some(staged.to_string()));
        assert_ne!(first, staged);
        Ok(())
    }

    #[pg_test]
    fn the_next_call_sees_a_promoted_version_and_survives_eviction() -> Result<(), Error> {
        small("t")?;
        assert_eq!(bfs("'t', ARRAY[5::bigint], 1")?.as_deref(), Some("5:0:-"));
        Spi::run("INSERT INTO t_edges VALUES (5, 6, 'a', 1); SELECT warren.refresh('t')")?;
        assert_eq!(bfs("'t', ARRAY[5::bigint], 1")?.as_deref(), Some("5:0:- 6:1:5"));
        // Evicting while a scan holds the version is safe: it keeps its copy.
        let found = rows(
            "SELECT n::text FROM (SELECT warren.reachable_set('t', ARRAY[5::bigint]) n,
                                         warren.evict('t') e) s",
        )?;
        assert_eq!(found.as_deref(), Some("5 6"));
        Ok(())
    }

    #[pg_test(
        error = "graph \"t\" has node ids of type bigint, but start is of type integer[]; cast it to bigint[]"
    )]
    fn start_ids_must_have_the_graphs_id_type() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT * FROM warren.bfs('t', ARRAY[1], 2)")
    }

    #[pg_test(
        error = "graph \"t\" has node ids of type bigint, but node_filter is of type integer[]; cast it to bigint[]"
    )]
    fn the_node_filter_must_have_the_graphs_id_type() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT * FROM warren.bfs('t', ARRAY[1::bigint], 2, node_filter := ARRAY[1])")
    }

    #[pg_test(error = "node_filter must be an array (e.g. ARRAY[...]::bigint[]), not bigint")]
    fn a_node_filter_that_is_not_an_array_is_an_error() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT * FROM warren.bfs('t', ARRAY[1::bigint], 2, node_filter := 5::bigint)")
    }

    #[pg_test(error = "direction must be 'out', 'in' or 'both', not 'up'")]
    fn an_unknown_direction_is_an_error() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT * FROM warren.bfs('t', ARRAY[1::bigint], 2, 'up')")
    }

    #[pg_test(error = "max_depth must not be negative")]
    fn a_negative_depth_is_an_error() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT * FROM warren.bfs('t', ARRAY[1::bigint], -1)")
    }

    #[pg_test(
        error = "must be a member of role \"t_owner\" or of a reader role to read graph \"t\""
    )]
    fn only_readers_may_traverse() -> Result<(), Error> {
        small("t")?;
        Spi::run("CREATE ROLE stranger; SET ROLE stranger; SELECT * FROM warren.bfs('t', ARRAY[1::bigint], 2)")
    }

    #[pg_test]
    fn a_reader_may_traverse() -> Result<(), Error> {
        small("t")?;
        Spi::run("CREATE ROLE friend; SELECT warren.grant('t', 'friend'); SET ROLE friend")?;
        let found = bfs("'t', ARRAY[5::bigint], 2");
        Spi::run("RESET ROLE")?;
        assert_eq!(found?.as_deref(), Some("5:0:-"));
        Ok(())
    }

    #[pg_test(error = "direction 'in' or 'both' needs the reverse CSR, and this graph has none")]
    fn direction_in_needs_a_reverse_csr() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE fwd_nodes AS SELECT 1::bigint AS id;
             CREATE TABLE fwd_edges (src bigint, dst bigint);
             SELECT warren.create_graph('fwd', 'SELECT id FROM fwd_nodes',
                                        'SELECT src, dst FROM fwd_edges', build_reverse := false);
             SELECT warren.refresh('fwd');
             SELECT * FROM warren.bfs('fwd', ARRAY[1::bigint], 2, 'in')",
        )
    }
}
