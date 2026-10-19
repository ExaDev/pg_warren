//! The SPI adapter of the source port: the node and edge queries run as
//! read-only cursors under the build's pinned snapshot, as the graph owner
//! (DESIGN.md §5.4 step 1, §7).

use pgrx::prelude::*;
use warren_core::graph::GraphError;

use super::error::BuildError;
use super::source::{EdgeRow, EdgeSink, SourceReader};
use crate::access::{as_role, with_search_path};
use crate::catalog::GraphDefinition;
use crate::source::{
    check_edge_columns, check_node_columns, for_each_row, open_read_only, IdDatum, SourceError,
};

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

impl<E: IdDatum> SourceReader<E> for SpiSource<'_> {
    fn node_ids(&mut self) -> Result<Vec<E>, BuildError> {
        let graph = self.graph;
        as_role(graph.owner, || {
            with_search_path(&graph.search_path, || {
                Spi::connect(|client| {
                    let mut opened =
                        open_read_only::<BuildError>(client, "node_query", &graph.node_query)?;
                    if check_node_columns(&opened.columns)? != graph.id_type {
                        return Err(SourceError::ColumnType {
                            query: "node_query",
                            column: 1,
                            role: "node id",
                            expected: graph.id_type.sql_name(),
                            found: opened.columns[0].name.clone(),
                        }
                        .into());
                    }
                    let mut ids = Vec::new();
                    for_each_row::<BuildError>(&mut opened.cursor, |row| {
                        if let Some(id) = row.get::<E::Sql>(1)?.map(E::from_sql) {
                            ids.try_reserve(1)
                                .map_err(|_| GraphError::OutOfMemory { elements: ids.len() + 1 })?;
                            ids.push(id);
                        }
                        Ok(())
                    })?;
                    Ok(ids)
                })
            })
        })
    }

    fn edges(&mut self, sink: &mut dyn EdgeSink<E>) -> Result<(), BuildError> {
        let graph = self.graph;
        let weight_column = if graph.has_relations { 4 } else { 3 };
        as_role(graph.owner, || {
            with_search_path(&graph.search_path, || {
                Spi::connect(|client| {
                    let mut opened =
                        open_read_only::<BuildError>(client, "edge_query", &graph.edge_query)?;
                    check_edge_columns(
                        &opened.columns,
                        graph.id_type,
                        graph.has_relations,
                        graph.weighted,
                    )?;
                    sink.expect_rows(opened.estimated_rows);
                    for_each_row(&mut opened.cursor, |row| {
                        sink.row(EdgeRow {
                            source: row.get::<E::Sql>(1)?.map(E::from_sql),
                            target: row.get::<E::Sql>(2)?.map(E::from_sql),
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
        })
    }
}
