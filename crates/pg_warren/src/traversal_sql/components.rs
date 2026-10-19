//! `warren.components`: weakly connected components (DESIGN.md §9.3).

use pgrx::prelude::*;
use pgrx::AnyElement;
use warren_core::graph::GraphError;
use warren_core::traversal::{components as label_components, TraversalError};

use super::ids::{check_type, external};
use super::stats::Recorder;
use super::{entry, TraversalSqlError};
use crate::cache::{cached, resolve};
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
STABLE PARALLEL RESTRICTED LANGUAGE c AS 'MODULE_PATHNAME', 'components_wrapper';
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
        let rows = with_id_type!(resolved.graph.id_type, E => {
            // SAFETY: fcinfo is this call's, and argument 1 exists.
            let witness = unsafe { pgrx::fcinfo::pg_getarg_type(fcinfo, 1) };
            check_type::<E>(graph, "id_type", witness, false)?;
            let loaded = cached::<E>(&resolved)?;
            let labelled = label_components(&loaded.graph, &mut PostgresInterrupts)?;
            recorder.record(labelled.stats);
            let tombstones = loaded.id_map.tombstones();
            let mut rows = Vec::new();
            let live = usize::try_from(loaded.id_map.node_count()).unwrap_or(usize::MAX);
            rows.try_reserve_exact(live)
                .map_err(|_| TraversalError::from(GraphError::OutOfMemory { elements: live }))?;
            for (node, &component) in (0..).zip(&labelled.result) {
                if tombstones.contains(node) {
                    continue;
                }
                if let Some(id) = external(&loaded.id_map, node) {
                    rows.push((id, i64::try_from(component).unwrap_or(i64::MAX)));
                }
            }
            rows
        });
        Ok(TableIterator::new(rows))
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{refresh, rows, small};

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

    #[pg_test(
        error = "graph \"t\" has node ids of type bigint, but id_type is of type uuid; cast it to bigint"
    )]
    fn the_id_type_witness_must_match_the_graph() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT * FROM warren.components('t', NULL::uuid)")
    }
}
