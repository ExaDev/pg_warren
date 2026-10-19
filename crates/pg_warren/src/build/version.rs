//! The port to the catalog of versions: a build records its new version in
//! `warren.versions` and may promote it (DESIGN.md §5.4 steps 5–6, §7).

use super::error::BuildError;
use crate::catalog::{GraphDefinition, VersionId, VersionStats};

/// Records versions of a graph.
pub(crate) trait Catalog {
    /// Inserts a staged version of graph `graph_id` with the next sequence
    /// number, and returns its id.
    fn insert_version(
        &mut self,
        graph_id: i32,
        stats: &VersionStats,
        label: Option<&str>,
    ) -> Result<VersionId, BuildError>;

    /// Records the total size of the version's segments.
    fn record_bytes(&mut self, version: VersionId, bytes: u64) -> Result<(), BuildError>;

    /// Makes `version` the current version of `graph`, marks it no longer
    /// staged, and applies retention.
    fn promote(&mut self, graph: &GraphDefinition, version: VersionId) -> Result<(), BuildError>;
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A version row of a [`MemoryCatalog`].
    #[derive(Debug, Clone, PartialEq)]
    pub(in crate::build) struct VersionRow {
        pub(in crate::build) graph_id: i32,
        pub(in crate::build) stats: VersionStats,
        pub(in crate::build) label: Option<String>,
        pub(in crate::build) bytes: u64,
        pub(in crate::build) staged: bool,
    }

    /// An in-memory [`Catalog`]; version ids start at 1.
    #[derive(Debug, Default)]
    pub(in crate::build) struct MemoryCatalog {
        pub(in crate::build) versions: Vec<VersionRow>,
        pub(in crate::build) current_version: Option<VersionId>,
    }

    impl MemoryCatalog {
        pub(in crate::build) fn version(&self, version: VersionId) -> &VersionRow {
            &self.versions[version as usize - 1]
        }
    }

    impl Catalog for MemoryCatalog {
        fn insert_version(
            &mut self,
            graph_id: i32,
            stats: &VersionStats,
            label: Option<&str>,
        ) -> Result<VersionId, BuildError> {
            self.versions.push(VersionRow {
                graph_id,
                stats: *stats,
                label: label.map(str::to_owned),
                bytes: 0,
                staged: true,
            });
            Ok(self.versions.len() as VersionId)
        }

        fn record_bytes(&mut self, version: VersionId, bytes: u64) -> Result<(), BuildError> {
            self.versions[version as usize - 1].bytes = bytes;
            Ok(())
        }

        fn promote(
            &mut self,
            _graph: &GraphDefinition,
            version: VersionId,
        ) -> Result<(), BuildError> {
            self.versions[version as usize - 1].staged = false;
            self.current_version = Some(version);
            Ok(())
        }
    }
}
