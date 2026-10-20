//! The two versions a distance compares, opened with one compute memory
//! budget: two versions of one graph, aligned by dense id, or versions of
//! two graphs, the second renumbered into the first's id space by external
//! id (the shadow-schema pattern, DESIGN.md §8).

use std::sync::Arc;

use warren_core::compute::AnyVector;
use warren_core::spectral::{
    align, align_graph, align_ids, edit_distance, EditDistance, IdAlignment, Resources,
};
use warren_core::version;

use super::super::SpectralSqlError;
use crate::cache::Resolved;
use crate::catalog::compaction_between;
use crate::compute_sql::{workers, Vectors};
use crate::interrupts::PostgresInterrupts;
use crate::source::IdDatum;

/// The versions compared, `before` first.
pub(super) struct Pair<E> {
    pub(super) before: Vectors<E>,
    pub(super) after: Vectors<E>,
    /// For versions of two graphs: how `after` maps into `before`'s id
    /// space.
    alignment: Option<IdAlignment>,
}

impl<E: IdDatum> Pair<E> {
    /// Opens `before` and `after`. Versions of one graph must not have a
    /// compaction between them, and their shared dense ids must name the
    /// same nodes; versions of two graphs are aligned by external id.
    pub(super) fn open(before: Resolved, after: Resolved) -> Result<Self, SpectralSqlError> {
        let same_graph = before.graph.graph_id == after.graph.graph_id;
        if same_graph {
            let (from, to) = (before.version, after.version);
            if let Some((compacted, source_version)) = compaction_between(&before.graph, from, to)?
            {
                let graph = before.graph.name.clone();
                return Err(SpectralSqlError::CompactionBoundary {
                    graph,
                    from,
                    to,
                    compacted,
                    source_version,
                });
            }
        } else if before.graph.id_type != after.graph.id_type {
            return Err(SpectralSqlError::IdTypeMismatch(before.graph.name, after.graph.name));
        }
        let (before, after) = Vectors::<E>::open_pair(before, after)?;
        let alignment = match same_graph {
            // One graph's versions are compared by dense id, so each shared
            // dense id must name one node (not so for sibling staged
            // versions); relations are matched by name.
            true => {
                version::shared_dense_ids(&before.loaded.id_map, &after.loaded.id_map)?;
                None
            }
            false => {
                let pool = workers()?;
                let mut resources = Resources {
                    workers: &pool,
                    memory: &before.memory,
                    interrupts: &mut PostgresInterrupts,
                };
                let (this, other) = (before.loaded.version()?, after.loaded.version()?);
                Some(align_ids(&mut resources, &this, &other)?)
            }
        };
        Ok(Self { before, after, alignment })
    }

    /// The `edit` distance between the versions, relations matched by name.
    /// For versions of two graphs, `after`'s CSRs are rebuilt in the shared
    /// dense ids first (charged to the budget).
    pub(super) fn edit_distance(&self) -> Result<EditDistance, SpectralSqlError> {
        let before = (&self.before.loaded.relations, &self.before.loaded.graph);
        let Some(alignment) = &self.alignment else {
            let after = &self.after.loaded;
            return Ok(edit_distance(before, (&after.relations, &after.graph)));
        };
        let pool = workers()?;
        let memory = &self.before.memory;
        let mut resources =
            Resources { workers: &pool, memory, interrupts: &mut PostgresInterrupts };
        let aligned = align_graph(&mut resources, &self.after.loaded.version()?, alignment)?;
        Ok(edit_distance(before, (&alignment.relations, &aligned.graph)))
    }

    /// Vector `name` of `before`.
    pub(super) fn before_vector(&mut self, name: &str) -> Result<Arc<AnyVector>, SpectralSqlError> {
        Ok(self.before.get(name)?)
    }

    /// Vector `name` of `after`, in the shared id space.
    pub(super) fn after_vector(&mut self, name: &str) -> Result<Arc<AnyVector>, SpectralSqlError> {
        let vector = self.after.get(name)?;
        match &self.alignment {
            None => Ok(vector),
            Some(alignment) => {
                let memory = &self.after.memory;
                Ok(Arc::new(align(memory, &vector, &alignment.dense_ids, alignment.id_space)?))
            }
        }
    }
}
