//! `warren.neighbours`, `warren.degree` and `warren.has_edge`: one node's
//! adjacency, read straight from the CSR (DESIGN.md §9.3). They record no
//! stats and read no vector, so they are `PARALLEL SAFE` (§14 item 33): a
//! parallel scan's workers call them on their own rows, each mapping the
//! shared version, or loading its own copy where the shared cache is
//! unavailable.

use std::borrow::Borrow;
use std::sync::Arc;

use pgrx::prelude::*;
use pgrx::AnyElement;
use warren_core::graph::DenseId;
use warren_core::graph::Direction;
use warren_core::traversal::{Adjacency, TraversalError};

use super::{entry, TraversalSqlError};
use crate::cache::{cached, resolve, CachedIds, LoadedVersion, Resolved};
use crate::extension::added_flag;
use crate::external_ids::{element, element_widened, external};
use crate::selection::{adjacency, direction};
use crate::source::with_id_type;

/// `warren.neighbours`: the distinct nodes one edge from `node` in
/// `direction` along `rels` (all if NULL), ascending by dense id. None if
/// `node` is NULL or not a node of the version. The neighbours are
/// collected as dense ids, and each row's external id is made when the row
/// is returned (DESIGN.md §12).
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.neighbours(graph text, node anyelement, direction text DEFAULT 'out',
                                  rels text[] DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS SETOF anyelement
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'neighbours_wrapper';
"#
)]
fn neighbours(
    graph: Option<&str>,
    node: Option<AnyElement>,
    direction: Option<&str>,
    rels: Option<Vec<Option<String>>>,
    version: Option<i64>,
) -> SetOfIterator<'static, AnyElement> {
    entry(|| {
        let query = NodeQuery::new(graph, node, direction, rels, version)?;
        let rows: Box<dyn Iterator<Item = AnyElement>> = with_id_type!(query.resolved.graph.id_type, E => {
            let rows = query.adjacent::<E, _>(|loaded, adjacency, node| {
                Ok::<_, TraversalError>(neighbour_rows(Arc::clone(loaded), adjacency.collect_neighbours(node)?))
            })?;
            Box::new(rows.transpose()?.into_iter().flatten())
        });
        Ok(SetOfIterator::new(rows))
    })
}

/// `warren.degree`: the number of `warren.neighbours` of `node`, or with
/// `per_relation` of (relation, neighbour) pairs: a neighbour once in each
/// relation linking it (DESIGN.md §5.3); 0 if it is not a node of the
/// version, NULL if it is NULL.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.degree(graph text, node anyelement, direction text DEFAULT 'out',
                              rels text[] DEFAULT NULL, version bigint DEFAULT NULL,
                              per_relation boolean DEFAULT false)
RETURNS bigint
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'degree_wrapper';
"#
)]
fn degree(
    graph: Option<&str>,
    node: Option<AnyElement>,
    direction: Option<&str>,
    rels: Option<Vec<Option<String>>>,
    version: Option<i64>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<i64> {
    let per_relation = added_flag(fcinfo, 5);
    entry(|| {
        let per_relation = per_relation.ok_or(TraversalSqlError::NullArgument("per_relation"))?;
        let query =
            NodeQuery { widen: true, ..NodeQuery::new(graph, node, direction, rels, version)? };
        let degree = with_id_type!(query.resolved.graph.id_type, E => {
            query.adjacent::<E, _>(|_, adjacency, node| {
                if per_relation { adjacency.degree_per_relation(node) } else { adjacency.degree(node) }
            })?
        });
        Ok(node.map(|_| i64::try_from(degree.unwrap_or(0)).unwrap_or(i64::MAX)))
    })
}

/// `warren.has_edge`: whether there is an edge `src → dst` in relation
/// `rel` (any relation if NULL). False if either end is not a node of the
/// version or the version has no relation `rel`; NULL if an end is NULL.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.has_edge(graph text, src anyelement, dst anyelement,
                                rel text DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS boolean
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'has_edge_wrapper';
"#
)]
fn has_edge(
    graph: Option<&str>,
    src: Option<AnyElement>,
    dst: Option<AnyElement>,
    rel: Option<&str>,
    version: Option<i64>,
) -> Option<bool> {
    entry(|| {
        let graph = graph.ok_or(TraversalSqlError::NullArgument("graph"))?;
        let resolved = resolve(graph, version, "read")?;
        let (Some(src), Some(dst)) = (src, dst) else {
            return Ok(None);
        };
        with_id_type!(resolved.graph.id_type, E => {
            let loaded = cached::<E>(&resolved)?;
            let src = element_widened::<E>(graph, "src", src)?;
            let dst = element_widened::<E>(graph, "dst", dst)?;
            let (Some(src), Some(dst)) = (loaded.id_map.dense_id(&src), loaded.id_map.dense_id(&dst))
            else {
                return Ok(Some(false));
            };
            let found = match rel {
                None => loaded.graph.all_relations().forward().has_edge(src, dst),
                Some(name) => loaded
                    .relations
                    .get(name)
                    .and_then(|r| loaded.graph.relation(r))
                    .is_some_and(|csr| csr.forward().has_edge(src, dst)),
            };
            Ok(Some(found))
        })
    })
}

/// The rows of a node's `neighbours`, in the extension's buffer, each
/// external id made when its row is returned, in the context Postgres
/// resets between rows. The rows hold the loaded version, which keeps it
/// mapped until the call ends, however it ends.
fn neighbour_rows<E: CachedIds>(
    loaded: Arc<LoadedVersion<E>>,
    neighbours: Vec<DenseId>,
) -> impl Iterator<Item = AnyElement> {
    neighbours.into_iter().filter_map(move |neighbour| external(&loaded.id_map, neighbour))
}

/// A call about one node.
struct NodeQuery<'a> {
    graph: &'a str,
    resolved: Resolved,
    node: Option<AnyElement>,
    direction: Direction,
    rels: Option<Vec<Option<String>>>,
    /// Whether a narrower integer node id is widened (for results that
    /// hold no ids).
    widen: bool,
}

impl<'a> NodeQuery<'a> {
    fn new(
        graph: Option<&'a str>,
        node: Option<AnyElement>,
        direction_text: Option<&str>,
        rels: Option<Vec<Option<String>>>,
        version: Option<i64>,
    ) -> Result<Self, TraversalSqlError> {
        let graph = graph.ok_or(TraversalSqlError::NullArgument("graph"))?;
        let resolved = resolve(graph, version, "read")?;
        Ok(Self {
            graph,
            resolved,
            node,
            direction: direction(direction_text)?,
            rels,
            widen: false,
        })
    }

    /// Runs `f` on the loaded version, the adjacency the query selects and
    /// the node's dense id; `None` if the node is NULL or not a node of the
    /// version.
    fn adjacent<E: CachedIds, T>(
        &self,
        f: impl FnOnce(&Arc<LoadedVersion<E>>, &Adjacency<'_>, DenseId) -> T,
    ) -> Result<Option<T>, TraversalSqlError> {
        let Some(node) = self.node else {
            return Ok(None);
        };
        let node = match self.widen {
            true => element_widened::<E>(self.graph, "node", node)?,
            false => element::<E>(self.graph, "node", node)?,
        };
        let loaded = cached::<E>(&self.resolved)?;
        let adjacency =
            adjacency(&loaded.graph, &loaded.relations, self.direction, self.rels.as_deref())?;
        Ok(loaded.id_map.dense_id(node.borrow()).map(|node| f(&loaded, &adjacency, node)))
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{held_after_first_row, iri_star, rows, small};

    fn neighbours(args: &str) -> Result<Option<String>, Error> {
        rows(&format!("SELECT n::text FROM warren.neighbours({args}) n"))
    }

    #[pg_test]
    fn neighbours_are_distinct_and_follow_direction_and_relations() -> Result<(), Error> {
        small("t")?;
        assert_eq!(neighbours("'t', 1::bigint")?.as_deref(), Some("2 4"));
        assert_eq!(neighbours("'t', 1::bigint, 'in'")?.as_deref(), Some("4"));
        assert_eq!(neighbours("'t', 4::bigint, 'both'")?.as_deref(), Some("1 3"));
        assert_eq!(neighbours("'t', 1::bigint, rels := ARRAY['b']")?.as_deref(), Some("4"));
        assert_eq!(neighbours("'t', 5::bigint")?.as_deref(), Some("5"));
        assert_eq!(neighbours("'t', 99::bigint")?.as_deref(), Some(""));
        assert_eq!(neighbours("'t', NULL::bigint")?.as_deref(), Some(""));
        Ok(())
    }

    /// Each neighbour's text id is made when its row is returned, so a
    /// hub's call holds no more Postgres memory for 50,000 rows to come
    /// than for 1,000.
    #[pg_test]
    fn neighbours_make_each_rows_id_as_it_is_returned() -> Result<(), Error> {
        iri_star("few", 1_000)?;
        iri_star("many", 50_000)?;
        let call = |graph: &str| {
            format!("warren.neighbours('{graph}', 'https://example.org/resource/0'::text)")
        };
        let few = held_after_first_row(&call("few"))?;
        let many = held_after_first_row(&call("many"))?;
        assert!(many < few + (256 << 10), "{many} bytes held for 50,000 rows, {few} for 1,000");
        Ok(())
    }

    #[pg_test]
    fn degree_counts_distinct_neighbours() -> Result<(), Error> {
        small("t")?;
        let degrees = Spi::get_one::<String>(
            "SELECT concat_ws(' ', warren.degree('t', 1::bigint), warren.degree('t', 4::bigint, 'both'),
                              warren.degree('t', 1::bigint, rels := ARRAY['a']),
                              warren.degree('t', 99::bigint),
                              warren.degree('t', NULL::bigint) IS NULL)",
        )?;
        assert_eq!(degrees.as_deref(), Some("2 2 1 0 t"));
        Ok(())
    }

    #[pg_test]
    fn has_edge_looks_up_one_edge() -> Result<(), Error> {
        small("t")?;
        let found = Spi::get_one::<String>(
            "SELECT concat_ws(' ', warren.has_edge('t', 1::bigint, 2::bigint),
                              warren.has_edge('t', 2::bigint, 1::bigint),
                              warren.has_edge('t', 1::bigint, 4::bigint, 'b'),
                              warren.has_edge('t', 1::bigint, 4::bigint, 'a'),
                              warren.has_edge('t', 1::bigint, 4::bigint, 'nope'),
                              warren.has_edge('t', 1::bigint, 99::bigint),
                              warren.has_edge('t', NULL::bigint, 1::bigint) IS NULL)",
        )?;
        assert_eq!(found.as_deref(), Some("t f t f f f t"));
        Ok(())
    }

    #[pg_test]
    fn lookups_returning_no_ids_widen_smaller_integer_ids() -> Result<(), Error> {
        small("t")?;
        let found = Spi::get_one::<String>(
            "SELECT concat_ws(' ', warren.degree('t', 1), warren.degree('t', 1::int2),
                              warren.has_edge('t', 1, 2), warren.reachable('t', 1, 3))",
        )?;
        assert_eq!(found.as_deref(), Some("2 2 t t"));
        Ok(())
    }

    #[pg_test]
    fn traversal_functions_are_stable_and_parallel_safe() -> Result<(), Error> {
        let labels = Spi::get_one::<String>(
            "SELECT string_agg(proname || ':' || provolatile::text || proparallel::text, ' '
                              ORDER BY proname)
             FROM pg_proc WHERE pronamespace = 'warren'::regnamespace
               AND proname IN ('neighbours', 'degree', 'has_edge', 'bfs', 'reachable',
                               'reachable_set', 'shortest_path', 'dijkstra', 'components',
                               'last_stats')",
        )?;
        assert_eq!(
            labels.as_deref(),
            Some(
                "bfs:ss components:ss degree:ss dijkstra:ss has_edge:ss last_stats:vr \
                 neighbours:ss reachable:ss reachable_set:ss shortest_path:ss"
            )
        );
        Ok(())
    }
}
