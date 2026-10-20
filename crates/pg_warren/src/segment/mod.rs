//! Chunked persistence: each segment of a version is stored in
//! `warren.segments` as fixed-size chunks (DESIGN.md §5.1, §5.3). Owns the
//! segment store port and its SPI adapter, chunking, and segment names.
//!
//! `build` writes segments through it and `cache` reads them; it depends
//! only on `access` (for the switch to the catalog owner) and on
//! `catalog` for the version id type.

mod chunk;
mod error;
mod layout;
mod name;
mod spi;

pub(crate) use chunk::{
    read_optional_segment, read_segment, write_segment, EachChunk, SegmentStore, CHUNK_SIZE,
};
pub(crate) use error::SegmentError;
pub(crate) use layout::write_version;
pub(crate) use name::{
    csr_segment, CsrPart, CsrScope, Direction, ID_MAP, RELATION_DICTIONARY, TOMBSTONES,
};
pub(crate) use spi::{each_chunk, SpiSegmentStore};

#[cfg(test)]
pub(crate) use chunk::tests::MemorySegmentStore;
