//! The vectors bound to one version, as one compute call sees them: the
//! session's transient vectors of the version first, then its persisted
//! ones, read through the backend's vector cache and charged to the call's
//! compute memory budget, and written back with tombstoned rows cleared.

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::{Arc, OnceLock};

use warren_core::compute::{AnyVector, ComputeMemory, Dtype, MemoryCharge};
use warren_core::graph::Tombstones;

use super::settings::compute_memory;
use super::ComputeSqlError;
use crate::access::require_creator_or_owner;
use crate::cache::{cached, LoadedVersion, Resolved};
use crate::catalog::{GraphDefinition, VersionId};
use crate::segment::CHUNK_SIZE;
use crate::source::IdDatum;
use crate::vector::{
    drop_transient, forget, keep_transient, load, remember, transient, transients, write_vector,
    Binding, SpiVectorStore, VectorError, VectorId, VectorRecord, VectorStore,
};

/// The budget cached vectors are charged to while cached: unlimited, since
/// what bounds a call is its own budget, which each vector it reads is also
/// charged to.
fn cache_memory() -> &'static ComputeMemory {
    static CACHE_MEMORY: OnceLock<ComputeMemory> = OnceLock::new();
    CACHE_MEMORY.get_or_init(|| ComputeMemory::new(u64::MAX))
}

/// The bytes of a vector of `dtype` with `length` rows of `width`.
fn bytes(dtype: Dtype, width: usize, length: u64) -> u64 {
    let element = match dtype {
        Dtype::F32 => size_of::<f32>(),
        Dtype::F64 | Dtype::I64 => size_of::<f64>(),
        Dtype::Bool => size_of::<bool>(),
    };
    length.saturating_mul(width as u64).saturating_mul(element as u64)
}

/// A vector name bound to a version, and its element type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BoundName {
    /// The name.
    pub(super) name: String,
    /// Its dtype.
    pub(super) dtype: Dtype,
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
pub(super) struct Vectors<E> {
    /// The graph.
    pub(super) graph: GraphDefinition,
    /// The version.
    pub(super) version: VersionId,
    /// The loaded version.
    pub(super) loaded: Arc<LoadedVersion<E>>,
    /// The call's compute memory budget.
    pub(super) memory: ComputeMemory,
    records: Vec<VectorRecord>,
    held: HashMap<String, MemoryCharge>,
}

impl<E: IdDatum> Vectors<E> {
    /// Opens `resolved`'s version and lists its vectors, with a fresh call
    /// budget of `warren.max_compute_memory`.
    pub(super) fn open(resolved: Resolved) -> Result<Self, ComputeSqlError> {
        Self::open_within(resolved, &compute_memory())
    }

    /// Opens `resolved`'s version and lists its vectors, charging what the
    /// call reads to `memory` (a budget shared with other versions).
    pub(super) fn open_within(
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
    pub(super) fn id_space(&self) -> u64 {
        self.loaded.id_map.id_space()
    }

    /// The version's tombstones.
    pub(super) fn tombstones(&self) -> &Tombstones {
        self.loaded.id_map.tombstones()
    }

    /// The names of the vectors bound to the version that this call sees.
    pub(super) fn names(&self) -> Result<Vec<BoundName>, ComputeSqlError> {
        bound_names(self.graph.graph_id, self.version)
    }

    /// The record of persisted vector `name`, if bound.
    fn record(&self, name: &str) -> Option<&VectorRecord> {
        self.records.iter().find(|record| record.name == name)
    }

    /// Vector `name`, which must be bound to the version.
    pub(super) fn get(&mut self, name: &str) -> Result<Arc<AnyVector>, ComputeSqlError> {
        self.find(name)?.ok_or_else(|| ComputeSqlError::VectorNotFound {
            graph: self.graph.name.clone(),
            name: name.to_owned(),
            version: self.version,
        })
    }

    /// Charges `name`'s bytes to the call's budget, once per call.
    fn charge(&mut self, name: &str, bytes: u64) -> Result<(), ComputeSqlError> {
        if !self.held.contains_key(name) {
            let charge = self.memory.charge(bytes)?;
            self.held.insert(name.to_owned(), charge);
        }
        Ok(())
    }

    /// Vector `name`, if bound: the session's transient vector of that
    /// name, else the persisted one. Its bytes are charged to the call's
    /// budget (once per call) before it is read.
    pub(super) fn find(&mut self, name: &str) -> Result<Option<Arc<AnyVector>>, ComputeSqlError> {
        let graph_id = self.graph.graph_id;
        if let Some(vector) = transient(graph_id, self.version, name) {
            self.charge(name, bytes(vector.dtype(), vector.width(), vector.id_space()))?;
            return Ok(Some(vector));
        }
        let Some(record) = self.record(name).cloned() else {
            return Ok(None);
        };
        self.charge(name, bytes(record.dtype, record.width, record.length))?;
        Ok(Some(load(&mut SpiVectorStore, cache_memory(), graph_id, &record)?))
    }

    /// Stores `vector` as `name` after clearing its tombstoned rows:
    /// persisted (returning its vector id) or, if not `persist`, as a
    /// transient vector of this session (returning `None`). Either replaces
    /// any vector of that name of either kind; a persisted one replaced
    /// must have been written by the caller, or the caller must own the
    /// graph.
    pub(super) fn save(
        &mut self,
        name: &str,
        mut vector: AnyVector,
        persist: bool,
    ) -> Result<Option<VectorId>, ComputeSqlError> {
        if name.is_empty() {
            return Err(ComputeSqlError::EmptyName);
        }
        let graph_id = self.graph.graph_id;
        let old = self.record(name).map(|old| (old.vector, old.created_by));
        if let Some((_, created_by)) = old {
            require_creator_or_owner(created_by, self.graph.owner, &self.graph.name, name)?;
        }
        let replacing = old.map(|(vector, _)| vector);
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
        self.records.retain(|bound| bound.name != name);
        self.records.push(record);
        Ok(Some(id))
    }
}
