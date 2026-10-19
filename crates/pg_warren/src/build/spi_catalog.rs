//! The SPI adapter of the version catalog port: delegates to the catalog's
//! version rows (DESIGN.md §5.1, §7).

use super::error::BuildError;
use super::version::Catalog;
use crate::catalog::{self, GraphDefinition, NewVersion, VersionId};

/// `warren.versions` and `warren.graphs.current_version`.
pub(crate) struct SpiCatalog {
    /// `pg_current_wal_lsn()` when the build's snapshot was pinned.
    built_lsn: String,
}

impl SpiCatalog {
    /// A catalog whose versions record the current WAL position as
    /// `built_lsn`.
    pub(crate) fn at_current_wal_lsn() -> Result<Self, BuildError> {
        Ok(Self::at_lsn(catalog::current_wal_lsn()?))
    }

    /// A catalog whose versions record `built_lsn`: a compaction records
    /// the LSN of the version it compacts, whose data it carries.
    pub(crate) fn at_lsn(built_lsn: String) -> Self {
        Self { built_lsn }
    }
}

impl Catalog for SpiCatalog {
    fn insert_version(&mut self, version: &NewVersion<'_>) -> Result<VersionId, BuildError> {
        Ok(catalog::insert_version(version, &self.built_lsn)?)
    }

    fn record_bytes(&mut self, version: VersionId, bytes: u64) -> Result<(), BuildError> {
        Ok(catalog::record_bytes(version, bytes)?)
    }

    fn promote(&mut self, graph: &GraphDefinition, version: VersionId) -> Result<(), BuildError> {
        Ok(catalog::promote_version(graph, version)?)
    }
}
