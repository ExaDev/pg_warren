//! The port to the catalog of versions: a build records its new version in
//! `warren.versions` and may promote it (DESIGN.md §5.4 steps 5–6, §7).

use super::error::BuildError;
use crate::catalog::{GraphDefinition, NewVersion, VersionId};

/// Records versions of a graph.
pub(crate) trait Catalog {
    /// Inserts a staged version with the graph's next sequence number, and
    /// returns its id.
    fn insert_version(&mut self, version: &NewVersion<'_>) -> Result<VersionId, BuildError>;

    /// Records the total size of the version's segments.
    fn record_bytes(&mut self, version: VersionId, bytes: u64) -> Result<(), BuildError>;

    /// Makes `version` the current version of `graph`, marks it no longer
    /// staged, and applies retention.
    fn promote(&mut self, graph: &GraphDefinition, version: VersionId) -> Result<(), BuildError>;
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
pub(crate) mod tests {
    use super::*;
    use crate::catalog::VersionStats;

    /// A version row of a [`MemoryCatalog`].
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) struct VersionRow {
        pub(crate) graph_id: i32,
        pub(crate) stats: VersionStats,
        pub(crate) label: Option<String>,
        pub(crate) compacted_from: Option<VersionId>,
        pub(crate) bytes: u64,
        pub(crate) staged: bool,
    }

    /// An in-memory [`Catalog`]; version ids start at 1.
    #[derive(Debug, Default)]
    pub(crate) struct MemoryCatalog {
        pub(crate) versions: Vec<VersionRow>,
        pub(crate) current_version: Option<VersionId>,
    }

    impl MemoryCatalog {
        pub(crate) fn version(&self, version: VersionId) -> &VersionRow {
            &self.versions[version as usize - 1]
        }
    }

    impl Catalog for MemoryCatalog {
        fn insert_version(&mut self, version: &NewVersion<'_>) -> Result<VersionId, BuildError> {
            self.versions.push(VersionRow {
                graph_id: version.graph_id,
                stats: version.stats,
                label: version.label.map(str::to_owned),
                compacted_from: version.compacted_from,
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
