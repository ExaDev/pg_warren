//! The vectors bound to one version, as one compute call sees them: read
//! through the backend's vector cache, charged to the call's compute
//! memory budget, and written back with tombstoned rows cleared.

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
    forget, load, remember, write_vector, Binding, SpiVectorStore, VectorId, VectorRecord,
    VectorStore,
};

/// The budget cached vectors are charged to while cached: unlimited, since
/// what bounds a call is its own budget, which each vector it reads is also
/// charged to.
fn cache_memory() -> &'static ComputeMemory {
    static CACHE_MEMORY: OnceLock<ComputeMemory> = OnceLock::new();
    CACHE_MEMORY.get_or_init(|| ComputeMemory::new(u64::MAX))
}

/// The bytes of a vector of `record`'s shape.
fn bytes(record: &VectorRecord) -> u64 {
    let element = match record.dtype {
        Dtype::F32 => size_of::<f32>(),
        Dtype::F64 | Dtype::I64 => size_of::<f64>(),
        Dtype::Bool => size_of::<bool>(),
    };
    record.length.saturating_mul(record.width as u64).saturating_mul(element as u64)
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
    held: HashMap<VectorId, MemoryCharge>,
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

    /// The vectors bound to the version, by name.
    pub(super) fn records(&self) -> &[VectorRecord] {
        &self.records
    }

    /// The record of vector `name`, if bound.
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

    /// Vector `name`, if bound. Its bytes are charged to the call's budget
    /// (once per call) before it is read.
    pub(super) fn find(&mut self, name: &str) -> Result<Option<Arc<AnyVector>>, ComputeSqlError> {
        let Some(record) = self.record(name).cloned() else {
            return Ok(None);
        };
        if !self.held.contains_key(&record.vector) {
            let charge = self.memory.charge(bytes(&record))?;
            self.held.insert(record.vector, charge);
        }
        let graph_id = self.graph.graph_id;
        Ok(Some(load(&mut SpiVectorStore, cache_memory(), graph_id, &record)?))
    }

    /// Stores `vector` as `name`, replacing any vector of that name (which
    /// the caller must have written, or own the graph), after clearing its
    /// tombstoned rows; returns its vector id.
    pub(super) fn save(
        &mut self,
        name: &str,
        mut vector: AnyVector,
    ) -> Result<VectorId, ComputeSqlError> {
        if name.is_empty() {
            return Err(ComputeSqlError::EmptyName);
        }
        if let Some(old) = self.record(name) {
            require_creator_or_owner(old.created_by, self.graph.owner, &self.graph.name, name)?;
            forget(old.vector);
        }
        vector.clear_tombstoned(self.tombstones());
        let graph_id = self.graph.graph_id;
        // SAFETY: reads the backend's current user id: the caller's role.
        let caller = unsafe { pgrx::pg_sys::GetUserId() };
        let binding = Binding::new(graph_id, self.version, name, caller);
        let record = write_vector(&mut SpiVectorStore, &binding, &vector, CHUNK_SIZE)?;
        let id = record.vector;
        remember(graph_id, id, Arc::new(vector));
        self.records.retain(|bound| bound.name != name);
        self.records.push(record);
        Ok(id)
    }
}
