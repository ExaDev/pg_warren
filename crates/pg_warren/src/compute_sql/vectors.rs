//! The vectors bound to one version, as one compute call sees them: the
//! session's transient vectors of the version first, then its persisted
//! ones, read through the backend's vector cache and charged to the call's
//! compute memory budget, and written back with tombstoned rows cleared.
//! Each read or write into the backend's cache then trims it to
//! `warren.max_cache_memory` (DESIGN.md §5.5).

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::{Arc, OnceLock};

use warren_core::compute::{AnyVector, ComputeMemory, Dtype, MemoryCharge};
use warren_core::graph::Tombstones;

use super::settings::compute_memory;
use super::ComputeSqlError;
use crate::access::require_creator_or_owner;
use crate::cache::{admit_transient, cached, trim, LoadedVersion, Resolved};
use crate::catalog::{GraphDefinition, VersionId};
use crate::segment::CHUNK_SIZE;
use crate::source::IdDatum;
use crate::vector::{
    drop_transient, forget, keep_transient, load, remember, transient, transients, write_vector,
    Binding, SpiVectorStore, VectorError, VectorId, VectorRecord, VectorStore,
};

/// The budget cached vectors are charged to while cached: unlimited, since
/// what bounds a call is its own budget, which each vector it reads is also
/// charged to, and what bounds the cache is `warren.max_cache_memory`.
fn cache_memory() -> &'static ComputeMemory {
    static CACHE_MEMORY: OnceLock<ComputeMemory> = OnceLock::new();
    CACHE_MEMORY.get_or_init(|| ComputeMemory::new(u64::MAX))
}

/// The bytes of a vector of `dtype` with `length` rows of `width`.
pub(crate) fn vector_bytes(dtype: Dtype, width: usize, length: u64) -> u64 {
    let element = match dtype {
        Dtype::F32 => size_of::<f32>(),
        Dtype::F64 | Dtype::I64 => size_of::<f64>(),
        Dtype::Bool => size_of::<bool>(),
    };
    length.saturating_mul(width as u64).saturating_mul(element as u64)
}

/// The prefix of the library's intermediate vectors (§6.7): such names
/// are only ever transient, so a library function's intermediate can
/// never replace, and so delete, a persisted vector.
const RESERVED_PREFIX: &str = "_warren_";

/// A vector name bound to a version, and its element type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundName {
    /// The name.
    pub(crate) name: String,
    /// Its dtype.
    pub(crate) dtype: Dtype,
}

/// The names of the vectors bound to `version` of graph `graph_id` that a
/// call sees: the session's transient vectors, then the persisted vectors
/// of other names, by name.
pub(super) fn bound_names(
    graph_id: i32,
    version: VersionId,
) -> Result<Vec<BoundName>, ComputeSqlError> {
    let mut names: Vec<BoundName> = transients(graph_id, version)
        .into_iter()
        .map(|(name, vector)| BoundName { name, dtype: vector.dtype() })
        .collect();
    for record in SpiVectorStore.bound(version)? {
        if !names.iter().any(|bound| bound.name == record.name) {
            names.push(BoundName { name: record.name, dtype: record.dtype });
        }
    }
    names.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(names)
}

/// A version, the vectors bound to it, and the call's compute memory.
pub(crate) struct Vectors<E> {
    /// The graph.
    pub(crate) graph: GraphDefinition,
    /// The version.
    pub(crate) version: VersionId,
    /// The loaded version.
    pub(crate) loaded: Arc<LoadedVersion<E>>,
    /// The call's compute memory budget.
    pub(crate) memory: ComputeMemory,
    records: Vec<VectorRecord>,
    held: HashMap<String, Read>,
}

/// A vector a call has read, and its charge to the call's budget.
struct Read {
    _charge: MemoryCharge,
    vector: Arc<AnyVector>,
}

impl<E: IdDatum> Vectors<E> {
    /// Opens `resolved`'s version and lists its vectors, with a fresh call
    /// budget of `warren.max_compute_memory`.
    pub(crate) fn open(resolved: Resolved) -> Result<Self, ComputeSqlError> {
        Self::open_within(resolved, &compute_memory())
    }

    /// Opens `resolved`'s version and lists its vectors, charging what the
    /// call reads to `memory` (a budget shared with other versions).
    pub(crate) fn open_within(
        resolved: Resolved,
        memory: &ComputeMemory,
    ) -> Result<Self, ComputeSqlError> {
        let loaded = cached::<E>(&resolved)?;
        let records = SpiVectorStore.bound(resolved.version)?;
        Ok(Self {
            graph: resolved.graph,
            version: resolved.version,
            loaded,
            memory: memory.clone(),
            records,
            held: HashMap::new(),
        })
    }

    /// The version's id space: every vector's row count.
    pub(crate) fn id_space(&self) -> u64 {
        self.loaded.id_map.id_space()
    }

    /// The version's tombstones.
    pub(crate) fn tombstones(&self) -> &Tombstones {
        self.loaded.id_map.tombstones()
    }

    /// The names of the vectors bound to the version that this call sees.
    pub(crate) fn names(&self) -> Result<Vec<BoundName>, ComputeSqlError> {
        bound_names(self.graph.graph_id, self.version)
    }

    /// The record of persisted vector `name`, if bound.
    fn record(&self, name: &str) -> Option<&VectorRecord> {
        self.records.iter().find(|record| record.name == name)
    }

    /// Vector `name`, which must be bound to the version.
    pub(crate) fn get(&mut self, name: &str) -> Result<Arc<AnyVector>, ComputeSqlError> {
        self.find(name)?.ok_or_else(|| ComputeSqlError::VectorNotFound {
            graph: self.graph.name.clone(),
            name: name.to_owned(),
            version: self.version,
        })
    }

    /// Vector `name`, if bound: the session's transient vector of that
    /// name, else the persisted one. Its bytes are charged to the call's
    /// budget before it is first read; the call then holds it, so reading
    /// it again returns the same vector even if the cache has evicted it.
    pub(super) fn find(&mut self, name: &str) -> Result<Option<Arc<AnyVector>>, ComputeSqlError> {
        if let Some(read) = self.held.get(name) {
            return Ok(Some(Arc::clone(&read.vector)));
        }
        let graph_id = self.graph.graph_id;
        let (charge, vector) = match transient(graph_id, self.version, name) {
            Some(vector) => {
                let bytes = vector_bytes(vector.dtype(), vector.width(), vector.id_space());
                (self.memory.charge(bytes)?, vector)
            }
            None => {
                let Some(record) = self.record(name).cloned() else {
                    return Ok(None);
                };
                let bytes = vector_bytes(record.dtype, record.width, record.length);
                let charge = self.memory.charge(bytes)?;
                let vector = load(&mut SpiVectorStore, cache_memory(), graph_id, &record)?;
                trim();
                (charge, vector)
            }
        };
        let read = Read { _charge: charge, vector: Arc::clone(&vector) };
        self.held.insert(name.to_owned(), read);
        Ok(Some(vector))
    }

    /// Whether the caller may store a vector as `name` (persisted if
    /// `persist`): a non-empty name, reserved only if transient, and any
    /// persisted vector of that name written by the caller or the caller a
    /// member of the graph's owner role.
    pub(crate) fn check_writable(&self, name: &str, persist: bool) -> Result<(), ComputeSqlError> {
        if name.is_empty() {
            return Err(ComputeSqlError::EmptyName);
        }
        if persist && name.starts_with(RESERVED_PREFIX) {
            return Err(ComputeSqlError::ReservedName(name.to_owned()));
        }
        if let Some(old) = self.record(name) {
            require_creator_or_owner(old.created_by, self.graph.owner, &self.graph.name, name)?;
        }
        Ok(())
    }

    /// Refuses, before anything is written, keeping transient vectors
    /// `names` of `bytes` each, if they would take this backend's transient
    /// vectors past `warren.max_cache_memory`. The current role's transient
    /// vectors of `names` (which they replace) and of `released` (which the
    /// caller drops before writing) are not counted.
    pub(crate) fn admit_transients(
        &self,
        names: &[String],
        released: &[String],
        bytes: u64,
    ) -> Result<(), ComputeSqlError> {
        let (graph_id, version) = (self.graph.graph_id, self.version);
        let replaced = names
            .iter()
            .chain(released)
            .filter_map(|name| transient(graph_id, version, name))
            .map(|vector| vector.bytes())
            .sum();
        let added = bytes.saturating_mul(names.len() as u64);
        Ok(admit_transient(replaced, added)?)
    }

    /// Stores `vector` as `name` (which may be reserved only if not
    /// `persist`) after clearing its tombstoned rows:
    /// persisted (returning its vector id) or, if not `persist`, as a
    /// transient vector of this session (returning `None`). Either replaces
    /// any vector of that name of either kind; a persisted one replaced
    /// must have been written by the caller, or the caller must own the
    /// graph. A transient one is refused if it would take this backend's
    /// transient vectors past `warren.max_cache_memory`; either kind is
    /// kept in the cache only as far as the cap allows.
    pub(crate) fn save(
        &mut self,
        name: &str,
        mut vector: AnyVector,
        persist: bool,
    ) -> Result<Option<VectorId>, ComputeSqlError> {
        self.check_writable(name, persist)?;
        let graph_id = self.graph.graph_id;
        let old = self.record(name).map(|old| (old.vector, old.created_by));
        let replacing = old.map(|(vector, _)| vector);
        if !persist {
            let replaced = transient(graph_id, self.version, name).map_or(0, |old| old.bytes());
            admit_transient(replaced, vector.bytes())?;
        }
        vector.clear_tombstoned(self.tombstones());
        self.held.remove(name);
        // The catalog change comes first: if it fails (a concurrent write),
        // this session's transient vectors are left as they were.
        if !persist {
            if let Some(old) = replacing {
                // Only the row checked above: a row of that name committed
                // since is another transaction's, not ours to delete.
                if !SpiVectorStore.delete(old)? {
                    return Err(VectorError::WrittenConcurrently {
                        name: name.to_owned(),
                        version: self.version,
                    }
                    .into());
                }
                forget(old);
                self.records.retain(|bound| bound.name != name);
            }
            keep_transient(graph_id, self.version, name, Arc::new(vector));
            trim();
            return Ok(None);
        }
        // SAFETY: reads the backend's current user id: the caller's role.
        let caller = unsafe { pgrx::pg_sys::GetUserId() };
        let binding = Binding::new(graph_id, self.version, name, caller).replacing(replacing);
        let record = write_vector(&mut SpiVectorStore, &binding, &vector, CHUNK_SIZE)?;
        if let Some(old) = replacing {
            forget(old);
        }
        drop_transient(graph_id, self.version, name);
        let id = record.vector;
        remember(graph_id, id, Arc::new(vector));
        trim();
        self.records.retain(|bound| bound.name != name);
        self.records.push(record);
        Ok(Some(id))
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use std::sync::Arc;

    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::Vectors;
    use crate::cache::resolve;
    use crate::fixture::{refresh, ring};

    #[pg_test]
    fn a_vector_read_twice_in_one_call_is_loaded_once() -> Result<(), Error> {
        ring("a", 2000, 1)?;
        refresh("a", true)?;
        Spi::run("SELECT warren.vector_const('a', 'p', 1)")?;
        for name in ["w", "x", "y", "z"] {
            Spi::run(&format!("SELECT warren.vector_const('a', '{name}', 1, persist := false)"))?;
        }
        // p (16,000 bytes) cannot be kept beside the transient vectors.
        Spi::run("SET LOCAL warren.max_cache_memory = '64kB'")?;
        let resolved = resolve("a", None, "read").map_err(|_| Error::InvalidPosition)?;
        let mut vectors = Vectors::<i64>::open(resolved).map_err(|_| Error::InvalidPosition)?;
        let first = vectors.get("p").map_err(|_| Error::InvalidPosition)?;
        let second = vectors.get("p").map_err(|_| Error::InvalidPosition)?;
        assert!(Arc::ptr_eq(&first, &second), "p was loaded again");
        Ok(())
    }
}
