//! The SPI adapter of the source port: the node and edge queries run as
//! read-only cursors under the build's pinned snapshot, as the graph owner
//! (DESIGN.md §5.4 step 1, §7).

use pgrx::prelude::*;
use warren_core::graph::GraphError;

use super::error::BuildError;
use super::source::{EdgeRow, SourceReader};
use crate::access::as_role;
use crate::catalog::GraphDefinition;
use crate::source::{check_edge_columns, check_node_columns, for_each_row, open_read_only};

/// A graph's sources, read through SPI.
pub(crate) struct SpiSource<'g> {
    graph: &'g GraphDefinition,
}

impl<'g> SpiSource<'g> {
    /// The sources of `graph`, read as its owner.
    pub(crate) fn new(graph: &'g GraphDefinition) -> Self {
        Self { graph }
    }
}

impl SourceReader<i64> for SpiSource<'_> {
    fn node_ids(&mut self) -> Result<Vec<i64>, BuildError> {
        let graph = self.graph;
        as_role(graph.owner, || {
            Spi::connect(|client| {
                let (mut cursor, columns) =
                    open_read_only::<BuildError>(client, "node_query", &graph.node_query)?;
                check_node_columns(&columns)?;
                let mut ids = Vec::new();
                for_each_row::<BuildError>(&mut cursor, |row| {
                    if let Some(id) = row.get::<i64>(1)? {
                        ids.try_reserve(1)
                            .map_err(|_| GraphError::OutOfMemory { elements: ids.len() + 1 })?;
                        ids.push(id);
                    }
                    Ok(())
                })?;
                Ok(ids)
            })
        })
    }

    fn edges(
        &mut self,
        each: &mut dyn FnMut(EdgeRow<'_, i64>) -> Result<(), BuildError>,
    ) -> Result<(), BuildError> {
        let graph = self.graph;
        let weight_column = if graph.has_relations { 4 } else { 3 };
        as_role(graph.owner, || {
            Spi::connect(|client| {
                let (mut cursor, columns) =
                    open_read_only::<BuildError>(client, "edge_query", &graph.edge_query)?;
                check_edge_columns(&columns, graph.has_relations, graph.weighted)?;
                for_each_row(&mut cursor, |row| {
                    each(EdgeRow {
                        source: row.get::<i64>(1)?,
                        target: row.get::<i64>(2)?,
                        relation: match graph.has_relations {
                            true => row.get::<&str>(3)?,
                            false => None,
                        },
                        weight: match graph.weighted {
                            true => row.get::<f64>(weight_column)?,
                            false => None,
                        },
                    })
                })
            })
        })
    }
}
