//! The peak memory of a build, estimated before it starts by following the
//! build's own allocations in order (see `columns` and `deduplication`),
//! with every relation at its edge count before deduplication. It is an
//! upper bound: deduplication only shrinks what follows it.

use std::mem::size_of;

use super::columns::Rows;
use super::deduplication::{merged_entry, relation_entry};
use super::{target_bytes, BuildPlan};
use crate::graph::RelationIndex;

/// Bytes held and the most held at once.
struct Ledger {
    held: u64,
    peak: u64,
}

impl Ledger {
    fn hold(&mut self, bytes: u64) {
        self.held = self.held.saturating_add(bytes);
        self.peak = self.peak.max(self.held);
    }

    fn free(&mut self, bytes: u64) {
        self.held = self.held.saturating_sub(bytes);
    }

    /// Holds `bytes` and frees them again: a transient buffer.
    fn pass(&mut self, bytes: u64) {
        self.hold(bytes);
        self.free(bytes);
    }
}

impl BuildPlan {
    /// The most bytes the build holds at once, the edge list included.
    pub(super) fn peak_bytes(&self) -> u64 {
        let edges = self.edges;
        let offsets = self.id_space.saturating_add(1).saturating_mul(8);
        let target = target_bytes(self.id_space);
        let weight = if self.weighted { 8 } else { 0 };
        let endpoint_column = edges.saturating_mul(self.endpoint_bytes);
        let relation_column = edges.saturating_mul(size_of::<RelationIndex>() as u64);
        let weight_column = edges.saturating_mul(weight);
        let csr = |edges: u64| offsets.saturating_add(edges.saturating_mul(target + weight));
        let mut ledger = Ledger { held: self.edge_bytes, peak: self.edge_bytes };
        let several = self.relation_edges.len() > 1;
        if !several {
            ledger.free(relation_column);
        }
        for (index, group) in self.groups.iter().enumerate() {
            let release = !self.store_merged && index + 1 == self.groups.len();
            let group_edges = &self.relation_edges[group.clone()];
            ledger.hold(rows_headers(group_edges.len()));
            for &edges in group_edges {
                ledger.hold(offsets.saturating_add(edges.saturating_mul(target)));
            }
            if release {
                ledger.free(endpoint_column);
            }
            for &edges in group_edges {
                ledger.hold(edges.saturating_mul(weight));
            }
            if release {
                let relations = if several { relation_column } else { 0 };
                ledger
                    .free(endpoint_column.saturating_add(relations).saturating_add(weight_column));
            }
            for &edges in group_edges {
                if self.weighted {
                    ledger.pass(self.longest_row.saturating_mul(2 * relation_entry_bytes(target)));
                }
                if self.build_reverse {
                    ledger.hold(csr(edges));
                    ledger.free(csr(edges));
                }
                ledger.free(csr(edges));
            }
            ledger.free(rows_headers(group_edges.len()));
        }
        if self.store_merged {
            ledger.hold(csr(edges).saturating_sub(edges.saturating_mul(weight)));
            ledger.free(endpoint_column);
            if self.weighted {
                if several {
                    ledger.hold(relation_column);
                    ledger.free(relation_column);
                }
                ledger.hold(weight_column);
                ledger.free(weight_column);
            } else if several {
                ledger.free(relation_column);
            }
            ledger.free(endpoint_column);
            if self.weighted {
                ledger.pass(self.longest_row.saturating_mul(2 * merged_entry_bytes(target)));
                if several {
                    ledger.free(relation_column);
                }
            }
            if self.build_reverse {
                ledger.hold(csr(edges));
            }
        }
        ledger.peak
    }
}

/// The bytes of the rows a group of `relations` relations is scattered
/// into, without their buffers.
fn rows_headers(relations: usize) -> u64 {
    (relations as u64).saturating_mul(size_of::<Rows<u32>>() as u64)
}

/// [`relation_entry`] at a target width of `target` bytes.
fn relation_entry_bytes(target: u64) -> u64 {
    match target {
        4 => relation_entry::<u32>(),
        _ => relation_entry::<u64>(),
    }
}

/// [`merged_entry`] at a target width of `target` bytes.
fn merged_entry_bytes(target: u64) -> u64 {
    match target {
        4 => merged_entry::<u32>(),
        _ => merged_entry::<u64>(),
    }
}
