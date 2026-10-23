//! Named vectors bound to a graph version (DESIGN.md §5.1, §6.2, §7): their
//! rows in `warren.vectors` and chunked bytes in `warren.vector_chunks`,
//! behind a vector store port with an SPI adapter and an in-memory fake;
//! the backend-local cache of loaded vectors, keyed by vector id; and
//! transient vectors, kept only in the backend (`persist := false`); and
//! quotas on what each reader role persists on a graph (`quota`).
//!
//! A vector row is never updated: writing a name again deletes the old row
//! (its chunks cascade) and inserts a new one, so a vector id names one
//! immutable vector and a cached entry never goes stale. Deleting a
//! version, or its graph, deletes its vectors.
//!
//! Depends on `access` (statements run as the catalog owner), `catalog`
//! (version ids) and `segment` (the chunk size); `cache` and `compute_sql`
//! use it.

mod cache;
mod carry;
mod error;
mod persist;
mod quota;
mod spi;
mod store;
mod transient;

pub(crate) use cache::{
    cached_vector_bytes, cached_vectors, evict_vectors, forget, in_use as vector_in_use, load,
    lookup as lookup_vector, remember,
};
pub(crate) use carry::{move_vectors, moved_notice};
pub(crate) use error::VectorError;
pub(crate) use persist::{read_vector, write_vector};
pub(crate) use quota::admit as admit_to_quota;
pub(crate) use spi::SpiVectorStore;
pub(crate) use store::{Binding, Checked, ChunkSource, VectorId, VectorRecord, VectorStore};
pub(crate) use transient::{
    drop_transient, evict_transients, keep_transient, retain_transients, role_transient_bytes,
    transient, transient_bytes, transient_versions, transients,
};

#[cfg(test)]
pub(crate) use store::tests::MemoryVectorStore;
