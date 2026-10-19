//! The port to a graph's sources: the rows of its node query and edge query,
//! read under one snapshot (DESIGN.md §5.4 steps 1–3). Its SPI adapter is
//! [`super::spi_source::SpiSource`].

use super::error::BuildError;

/// One row of an edge query. Fields the graph does not have are `None`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct EdgeRow<'r, E> {
    /// The external id of the source node.
    pub(crate) source: Option<E>,
    /// The external id of the target node.
    pub(crate) target: Option<E>,
    /// The relation name, if the graph has relations.
    pub(crate) relation: Option<&'r str>,
    /// The weight, if the graph is weighted.
    pub(crate) weight: Option<f64>,
}

/// Reads a graph's node ids and edge rows.
pub(crate) trait SourceReader<E> {
    /// Every non-NULL external id the node query returns.
    fn node_ids(&mut self) -> Result<Vec<E>, BuildError>;

    /// Passes every row of the edge query to `sink`, stopping at its first
    /// error, after telling it roughly how many rows to expect.
    fn edges(&mut self, sink: &mut dyn EdgeSink<E>) -> Result<(), BuildError>;
}

/// Takes the rows of an edge query.
pub(crate) trait EdgeSink<E> {
    /// A hint, before the first row: about how many rows follow (for
    /// example the planner's estimate). It may be wrong either way.
    fn expect_rows(&mut self, rows: u64);

    /// Takes one row.
    fn row(&mut self, row: EdgeRow<'_, E>) -> Result<(), BuildError>;
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// An edge row owned by a [`MemorySource`].
    pub(in crate::build) type OwnedEdgeRow =
        (Option<i64>, Option<i64>, Option<String>, Option<f64>);

    /// An in-memory [`SourceReader`].
    #[derive(Debug, Default, Clone)]
    pub(in crate::build) struct MemorySource {
        pub(in crate::build) nodes: Vec<i64>,
        pub(in crate::build) edges: Vec<OwnedEdgeRow>,
        /// The row estimate to announce; the true count if `None`.
        pub(in crate::build) estimated_rows: Option<u64>,
    }

    impl SourceReader<i64> for MemorySource {
        fn node_ids(&mut self) -> Result<Vec<i64>, BuildError> {
            Ok(self.nodes.clone())
        }

        fn edges(&mut self, sink: &mut dyn EdgeSink<i64>) -> Result<(), BuildError> {
            sink.expect_rows(self.estimated_rows.unwrap_or(self.edges.len() as u64));
            for (source, target, relation, weight) in &self.edges {
                sink.row(EdgeRow {
                    source: *source,
                    target: *target,
                    relation: relation.as_deref(),
                    weight: *weight,
                })?;
            }
            Ok(())
        }
    }
}
