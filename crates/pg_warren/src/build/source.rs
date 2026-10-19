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

    /// Passes every row of the edge query to `each`, stopping at its first
    /// error.
    fn edges(
        &mut self,
        each: &mut dyn FnMut(EdgeRow<'_, E>) -> Result<(), BuildError>,
    ) -> Result<(), BuildError>;
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
    }

    impl SourceReader<i64> for MemorySource {
        fn node_ids(&mut self) -> Result<Vec<i64>, BuildError> {
            Ok(self.nodes.clone())
        }

        fn edges(
            &mut self,
            each: &mut dyn FnMut(EdgeRow<'_, i64>) -> Result<(), BuildError>,
        ) -> Result<(), BuildError> {
            for (source, target, relation, weight) in &self.edges {
                each(EdgeRow {
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
