//! Building versions (DESIGN.md §5.4): `warren.refresh` reads a graph's
//! sources and writes a new version's segments and version row.
//!
//! The pipeline is pure logic over three ports this feature defines — a
//! source reader, a segment store (`segment`) and a version catalog — each
//! with an SPI adapter and an in-memory fake. It depends on `catalog`,
//! `source`, `segment` and `access`; nothing depends on it but the
//! operations that also produce versions.

mod budget;
mod error;
mod pipeline;
mod refresh;
mod source;
mod spi_catalog;
mod spi_source;
mod version;

pub(crate) use budget::{define_max_build_memory, max_build_memory};
pub(crate) use error::BuildError;
pub(crate) use spi_catalog::SpiCatalog;
pub(crate) use version::Catalog;

#[cfg(test)]
pub(crate) use version::tests::MemoryCatalog;
