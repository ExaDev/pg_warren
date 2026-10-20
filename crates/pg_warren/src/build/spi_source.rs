//! The SPI adapter of the source port: the node and edge queries run as
//! read-only cursors under the build's pinned snapshot, as the graph owner
//! (DESIGN.md §5.4 step 1, §7).

use pgrx::prelude::*;
use warren_core::graph::{BuildBuffer, BuildMemory};

use super::error::BuildError;
use super::source::{EdgeRow, EdgeSink, SourceReader};
use crate::access::{as_role, with_read_only, with_search_path};
use crate::catalog::GraphDefinition;
use crate::source::{
    check_edge_columns, check_node_columns, for_each_row, for_each_tuple, open_read_only,
    text_in_place, IdDatum, SourceError,
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
    fn node_ids(&mut self, memory: &BuildMemory) -> Result<BuildBuffer<E>, BuildError> {
        let graph = self.graph;
        as_role(graph.owner, || {
            with_search_path(&graph.search_path, || {
                with_read_only(|| {
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
                        let mut ids = BuildBuffer::new(memory);
                        for_each_row::<BuildError>(&mut opened.cursor, |row| {
                            if let Some(id) = row.get::<E::Sql>(1)?.map(E::from_sql) {
                                ids.push(id)?;
                            }
                            Ok(())
                        })?;
                        Ok(ids)
                    })
                })
            })
        })
    }

    fn edges(&mut self, sink: &mut dyn EdgeSink<E>) -> Result<(), BuildError> {
        let graph = self.graph;
        let weight_column = if graph.has_relations { 4 } else { 3 };
        as_role(graph.owner, || {
            with_search_path(&graph.search_path, || {
                with_read_only(|| {
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
                        for_each_tuple(&mut opened.cursor, |row| {
                            sink.row(EdgeRow {
                                source: row.get::<E::Sql>(1)?.map(E::from_sql),
                                target: row.get::<E::Sql>(2)?.map(E::from_sql),
                                relation: match graph.has_relations {
                                    true => text_in_place(row, 3)?,
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
        })
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::SpiSource;
    use crate::build::error::BuildError;
    use crate::build::source::{EdgeRow, EdgeSink, SourceReader};
    use crate::catalog::GraphDefinition;
    use crate::fixture::allocated;
    use crate::source::with_pinned_snapshot;

    /// An edge sink that records the most Postgres memory allocated since
    /// it was made, over the first batch of rows and over all of them.
    struct Probe {
        before: usize,
        first_batch: usize,
        peak: usize,
        rows: u64,
    }

    impl EdgeSink<i64> for Probe {
        fn expect_rows(&mut self, _rows: u64) {}

        fn row(&mut self, row: EdgeRow<'_, i64>) -> Result<(), BuildError> {
            self.peak = self.peak.max(allocated().saturating_sub(self.before));
            self.rows += u64::from(row.relation.is_some());
            if self.rows == 10_000 {
                self.first_batch = self.peak;
            }
            Ok(())
        }
    }

    /// Reading edge rows holds no Postgres memory per row: each relation
    /// name is read in place, not copied into a memory context that lives
    /// until the refresh ends (DESIGN.md §5.4; 100M rows held 1.7 GB): it
    /// stays what one batch of rows takes.
    #[pg_test]
    fn reading_edges_holds_no_memory_per_row() -> Result<(), Error> {
        Spi::run(
            "CREATE ROLE leak_owner;
             CREATE TABLE leak_nodes (id bigint);
             CREATE TABLE leak_edges (src bigint, dst bigint, rel text);
             INSERT INTO leak_nodes SELECT generate_series(1, 1000);
             INSERT INTO leak_edges
                 SELECT g % 1000 + 1, g % 997 + 1, 'relation ' || g % 5
                 FROM generate_series(1, 400000) g;
             GRANT SELECT ON leak_nodes, leak_edges TO leak_owner;
             SET ROLE leak_owner;
             SELECT warren.create_graph('leak', 'SELECT id FROM leak_nodes',
                                        'SELECT src, dst, rel FROM leak_edges',
                                        has_relations := true);
             RESET ROLE",
        )?;
        let graph = GraphDefinition::find("leak").map_err(|_| Error::InvalidPosition)?;
        let mut probe = Probe { before: allocated(), first_batch: 0, peak: 0, rows: 0 };
        with_pinned_snapshot(|| {
            SourceReader::<i64>::edges(&mut SpiSource::new(&graph), &mut probe)
        })
        .map_err(|_| Error::InvalidPosition)?;
        assert_eq!(probe.rows, 400_000);
        assert!(
            probe.peak <= probe.first_batch + (256 << 10),
            "{} bytes allocated while reading, {} after the first batch",
            probe.peak,
            probe.first_batch
        );
        Ok(())
    }
}
