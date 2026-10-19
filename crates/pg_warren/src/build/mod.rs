//! Build pipeline adapter: reads node/edge sources through SPI and writes
//! chunked segments for a new version (DESIGN.md §5.4). Build logic talks to
//! Postgres only through narrow ports (a source reader, a segment store, a
//! catalog of versions) so it is unit-testable with in-memory fakes.

mod chunk;
mod error;
mod pipeline;
mod refresh;
mod source;
mod spi_catalog;
mod spi_source;
mod version;

pub(crate) use source::{check_edge_columns, check_node_columns, SourceError};
pub(crate) use spi_source::source_columns;
