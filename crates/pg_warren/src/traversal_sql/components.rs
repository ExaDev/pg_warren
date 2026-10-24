//! `warren.components`: weakly connected components (DESIGN.md §9.3).

use std::sync::Arc;

use pgrx::prelude::*;
use pgrx::AnyElement;
use warren_core::graph::DenseId;
use warren_core::traversal::components as label_components;

use super::stats::Recorder;
use super::{entry, TraversalSqlError};
use crate::cache::{cached, resolve, CachedIds, LoadedVersion};
use crate::external_ids::{check_type, external};
use crate::interrupts::PostgresInterrupts;
use crate::source::with_id_type;

/// `warren.components`: every live node with its weakly connected
/// component, labelled by the smallest dense id in it, so labels are
/// deterministic and comparable across versions in one id space.
/// `id_type` only fixes the result type, as the first argument of
/// `json_populate_record` does: pass `NULL::<the graph's id type>`; its
/// value is ignored.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.components(graph text, id_type anyelement, version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, component_id bigint)
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'components_wrapper';
"#
)]
fn components(
    graph: Option<&str>,
    _id_type: Option<AnyElement>,
    version: Option<i64>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> TableIterator<'static, (name!(node_id, AnyElement), name!(component_id, i64))> {
    entry(|| {
        let graph = graph.ok_or(TraversalSqlError::NullArgument("graph"))?;
        let resolved = resolve(graph, version, "read")?;
        let mut recorder = Recorder::start("components", graph, resolved.version);
        let rows: Box<dyn Iterator<Item = Row>> = with_id_type!(resolved.graph.id_type, E => {
            // SAFETY: fcinfo is this call's, and argument 1 exists.
            let witness = unsafe { pgrx::fcinfo::pg_getarg_type(fcinfo, 1) };
            check_type::<E>(graph, "id_type", witness, false)?;
            let loaded = cached::<E>(&resolved)?;
            let labelled = label_components(&loaded.graph, &mut PostgresInterrupts)?;
            recorder.record(labelled.stats);
            Box::new(live_rows(loaded, labelled.result))
        });
        Ok(TableIterator::new(rows))
    })
}

/// A row of the result: a node's external id and its component.
type Row = (AnyElement, i64);

/// The rows of each live node in dense id order, each made when it is
/// returned (DESIGN.md §12): the labels stay in the extension's buffer,
/// and only the current row's external id is palloc'd, in the context
/// Postgres resets between rows. The rows hold the loaded version, which
/// keeps it mapped until the call ends, however it ends.
fn live_rows<E: CachedIds>(
    loaded: Arc<LoadedVersion<E>>,
    labels: Vec<DenseId>,
) -> impl Iterator<Item = Row> {
    (0..).zip(labels).filter_map(move |(node, component)| {
        if loaded.id_map.tombstones().contains(node) {
            return None;
        }
        let id = external(&loaded.id_map, node)?;
        Some((id, i64::try_from(component).unwrap_or(i64::MAX)))
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{held_after_first_row, iri_path, refresh, rows, small};

    #[pg_test]
    fn components_label_live_nodes_by_their_smallest_dense_id() -> Result<(), Error> {
        small("t")?;
        let found = rows(
            "SELECT node_id || ':' || component_id
             FROM warren.components('t', NULL::bigint) ORDER BY node_id",
        )?;
        assert_eq!(found.as_deref(), Some("1:0 2:0 3:0 4:0 5:4 6:5"));
        Spi::run("DELETE FROM t_nodes WHERE id = 2")?;
        refresh("t", true)?;
        let found = rows(
            "SELECT node_id || ':' || component_id
             FROM warren.components('t', NULL::bigint) ORDER BY node_id",
        )?;
        assert_eq!(found.as_deref(), Some("1:0 3:0 4:0 5:4 6:5"));
        Ok(())
    }

    /// Each row's text id is made when the row is returned, so a call
    /// holds no more Postgres memory for 50,000 rows to come than for
    /// 1,000 (built up front, the ids took about 72 bytes each).
    #[pg_test]
    fn components_make_each_rows_id_as_it_is_returned() -> Result<(), Error> {
        iri_path("few", 1_000)?;
        iri_path("many", 50_000)?;
        let few = held_after_first_row("warren.components('few', NULL::text)")?;
        let many = held_after_first_row("warren.components('many', NULL::text)")?;
        assert!(many < few + (256 << 10), "{many} bytes held for 50,000 rows, {few} for 1,000");
        Ok(())
    }

    #[pg_test(
        error = "graph \"t\" has node ids of type bigint, but id_type is of type uuid; cast it to bigint"
    )]
    fn the_id_type_witness_must_match_the_graph() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT * FROM warren.components('t', NULL::uuid)")
    }
}
