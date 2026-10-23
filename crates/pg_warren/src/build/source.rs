//! The port to a graph's sources: the rows of its node query and edge query,
//! read under one snapshot (DESIGN.md §5.4 steps 1–3). Its SPI adapter is
//! [`super::spi_source::SpiSource`].

use warren_core::graph::{BuildMemory, IdKeys};

use super::error::BuildError;

/// One row of an edge query, read in place. Fields the graph does not have
/// are `None`.
#[derive(Debug, PartialEq)]
pub(crate) struct EdgeRow<'r, K: ?Sized> {
    /// The external id of the source node.
    pub(crate) source: Option<&'r K>,
    /// The external id of the target node.
    pub(crate) target: Option<&'r K>,
    /// The relation name, if the graph has relations.
    pub(crate) relation: Option<&'r str>,
    /// The weight, if the graph is weighted.
    pub(crate) weight: Option<f64>,
}

/// Reads the node ids and edge rows of a graph whose ids `K` stores.
pub(crate) trait SourceReader<K: IdKeys> {
    /// Every non-NULL external id the node query returns, charged to
    /// `memory`.
    fn node_ids(&mut self, memory: &BuildMemory) -> Result<K::Ids, BuildError>;

    /// Passes every row of the edge query to `sink`, stopping at its first
    /// error, after telling it roughly how many rows to expect.
    fn edges(&mut self, sink: &mut dyn EdgeSink<K::Key>) -> Result<(), BuildError>;
}

/// Takes the rows of an edge query, whose ids are `K`s.
pub(crate) trait EdgeSink<K: ?Sized> {
    /// A hint, before the first row: about how many rows follow (for
    /// example the planner's estimate). It may be wrong either way.
    fn expect_rows(&mut self, rows: u64);

    /// Takes one row.
    fn row(&mut self, row: EdgeRow<'_, K>) -> Result<(), BuildError>;
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
pub(super) mod tests {
    use warren_core::graph::{FixedKeys, TextIds, TextKeys};

    use super::*;

    /// An edge row owned by a [`MemorySource`].
    pub(in crate::build) type OwnedEdgeRow<T = i64> =
        (Option<T>, Option<T>, Option<String>, Option<f64>);

    /// An in-memory [`SourceReader`], of `int8` ids or (`Vec<u8>`) text.
    #[derive(Debug, Clone)]
    pub(in crate::build) struct MemorySource<T = i64> {
        pub(in crate::build) nodes: Vec<T>,
        pub(in crate::build) edges: Vec<OwnedEdgeRow<T>>,
        /// The row estimate to announce; the true count if `None`.
        pub(in crate::build) estimated_rows: Option<u64>,
        /// How many edge rows the sink has taken, shared by clones.
        pub(in crate::build) rows_read: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl<T> Default for MemorySource<T> {
        fn default() -> Self {
            Self {
                nodes: Vec::new(),
                edges: Vec::new(),
                estimated_rows: None,
                rows_read: std::rc::Rc::default(),
            }
        }
    }

    impl<T> MemorySource<T> {
        fn each_edge<K: ?Sized>(
            &mut self,
            sink: &mut dyn EdgeSink<K>,
            key: impl Fn(&T) -> &K,
        ) -> Result<(), BuildError> {
            sink.expect_rows(self.estimated_rows.unwrap_or(self.edges.len() as u64));
            for (source, target, relation, weight) in &self.edges {
                sink.row(EdgeRow {
                    source: source.as_ref().map(&key),
                    target: target.as_ref().map(&key),
                    relation: relation.as_deref(),
                    weight: *weight,
                })?;
                self.rows_read.set(self.rows_read.get() + 1);
            }
            Ok(())
        }
    }

    impl SourceReader<FixedKeys<i64>> for MemorySource<i64> {
        fn node_ids(
            &mut self,
            memory: &BuildMemory,
        ) -> Result<warren_core::graph::BuildBuffer<i64>, BuildError> {
            let mut ids = warren_core::graph::BuildBuffer::new(memory);
            for &id in &self.nodes {
                ids.push(id)?;
            }
            Ok(ids)
        }

        fn edges(&mut self, sink: &mut dyn EdgeSink<i64>) -> Result<(), BuildError> {
            self.each_edge(sink, |id| id)
        }
    }

    impl SourceReader<TextKeys> for MemorySource<Vec<u8>> {
        fn node_ids(&mut self, memory: &BuildMemory) -> Result<TextIds, BuildError> {
            let mut ids = TextIds::new(memory);
            for id in &self.nodes {
                ids.push(id)?;
            }
            Ok(ids)
        }

        fn edges(&mut self, sink: &mut dyn EdgeSink<[u8]>) -> Result<(), BuildError> {
            self.each_edge(sink, Vec::as_slice)
        }
    }
}
