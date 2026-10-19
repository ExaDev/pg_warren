//! The SPI adapter of the version catalog port: delegates to the catalog's
//! version rows (DESIGN.md §5.1, §7).

use super::error::BuildError;
use super::version::Catalog;
use crate::catalog::{self, VersionId, VersionStats};

/// `warren.versions` and `warren.graphs.current_version`.
pub(crate) struct SpiCatalog {
    /// `pg_current_wal_lsn()` when the build's snapshot was pinned.
    built_lsn: String,
}

impl SpiCatalog {
    /// A catalog whose versions record the current WAL position as
    /// `built_lsn`.
    pub(crate) fn at_current_wal_lsn() -> Result<Self, BuildError> {
        Ok(Self { built_lsn: catalog::current_wal_lsn()? })
    }
}

impl Catalog for SpiCatalog {
    fn insert_version(
        &mut self,
        graph_id: i32,
        stats: &VersionStats,
        label: Option<&str>,
    ) -> Result<VersionId, BuildError> {
        Ok(catalog::insert_version(graph_id, stats, label, &self.built_lsn)?)
    }

    fn record_bytes(&mut self, version: VersionId, bytes: u64) -> Result<(), BuildError> {
        Ok(catalog::record_bytes(version, bytes)?)
    }

    fn promote(&mut self, graph_id: i32, version: VersionId) -> Result<(), BuildError> {
        Ok(catalog::promote(graph_id, version)?)
    }
}
