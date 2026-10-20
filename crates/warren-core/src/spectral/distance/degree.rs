//! The `degree` distance: how far apart the degree distributions of two
//! versions are.
//!
//! A node's degree is its number of distinct out-neighbours over all
//! relations (the all-relations forward CSR; for an undirected graph, its
//! number of neighbours). The distribution is over live nodes: tombstoned
//! nodes are not in the version, isolated live nodes have degree 0. A
//! version without live nodes counts as a point mass at degree 0.

use super::super::{Resources, SpectralError};
use crate::compute::{ComputeError, MemoryCharge};
use crate::graph::{GraphCsr, MergedRows, Tombstones};
use crate::interrupts::{Cancelled, Interrupts};

/// How often the histogram pass checks the cancellation flag, in rows.
const ROWS_PER_CHECK: u64 = 1 << 16;

/// The distance between two degree distributions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DegreeDistance {
    /// The Wasserstein-1 (earth mover's) distance, in degrees: the mean
    /// change in degree under the best matching of the two node sets.
    pub wasserstein: f64,
    /// The Kolmogorov–Smirnov statistic: the largest difference of the two
    /// cumulative distributions, in `[0, 1]`.
    pub ks: f64,
}

/// The `degree` distance from `before` to `after`, each a version's CSRs
/// and tombstones.
pub fn degree_distance<I: Interrupts>(
    resources: &mut Resources<'_, I>,
    before: (&GraphCsr, &Tombstones),
    after: (&GraphCsr, &Tombstones),
) -> Result<DegreeDistance, SpectralError> {
    let before = Histogram::of(resources, before.0.all_relations().forward(), before.1)?;
    let after = Histogram::of(resources, after.0.all_relations().forward(), after.1)?;
    let degrees = before.counts.len().max(after.counts.len());
    let (mut wasserstein, mut ks) = (0.0f64, 0.0f64);
    let (mut below_before, mut below_after) = (0u64, 0u64);
    for degree in 0..degrees {
        below_before += before.counts.get(degree).copied().unwrap_or(0);
        below_after += after.counts.get(degree).copied().unwrap_or(0);
        let gap = (before.cumulative(below_before) - after.cumulative(below_after)).abs();
        wasserstein += gap;
        ks = ks.max(gap);
    }
    Ok(DegreeDistance { wasserstein, ks })
}

/// The number of live nodes of each degree.
#[derive(Debug)]
struct Histogram {
    counts: Vec<u64>,
    nodes: u64,
    _charge: MemoryCharge,
}

impl Histogram {
    fn of<I: Interrupts>(
        resources: &mut Resources<'_, I>,
        csr: MergedRows<'_>,
        tombstones: &Tombstones,
    ) -> Result<Self, ComputeError> {
        let rows = csr.id_space();
        let live = |row: u64| !tombstones.contains(row);
        let max = (0..rows).filter(|&row| live(row)).map(|row| csr.degree(row)).max();
        let (mut counts, charge) =
            resources.memory.filled(max.map_or(0, |max| max as usize + 1), 0u64)?;
        let nodes = resources.run(|cancellation| {
            let mut nodes = 0;
            for row in (0..rows).filter(|&row| live(row)) {
                if row % ROWS_PER_CHECK == 0 && cancellation.is_cancelled() {
                    return Err(Cancelled.into());
                }
                counts[csr.degree(row) as usize] += 1;
                nodes += 1;
            }
            Ok(nodes)
        })?;
        Ok(Self { counts, nodes, _charge: charge })
    }

    /// The fraction of nodes of degree at most `d`, given the `below`
    /// nodes of degree at most `d`; 1 for a version without nodes.
    fn cumulative(&self, below: u64) -> f64 {
        match self.nodes {
            0 => 1.0,
            nodes => below as f64 / nodes as f64,
        }
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::compute::reference::{self, Edges};
    use crate::compute::{ComputeMemory, Workers};
    use crate::interrupts::Uninterrupted;

    fn version(id_space: u64, edges: &Edges, tombstoned: &[u64]) -> (GraphCsr, Tombstones) {
        let mut tombstones = Tombstones::new(id_space).unwrap();
        tombstoned.iter().for_each(|&id| tombstones.insert(id));
        (reference::graph(id_space, 1, edges, false, false), tombstones)
    }

    fn distance(before: &(GraphCsr, Tombstones), after: &(GraphCsr, Tombstones)) -> DegreeDistance {
        let workers = Workers::new(2).unwrap();
        let memory = ComputeMemory::new(1 << 20);
        let mut resources =
            Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted };
        degree_distance(&mut resources, (&before.0, &before.1), (&after.0, &after.1)).unwrap()
    }

    #[test]
    fn identical_versions_are_zero_apart() {
        let v = version(4, &vec![(0, 0, 1, 1.0), (0, 0, 2, 1.0), (0, 2, 3, 1.0)], &[]);
        assert_eq!(distance(&v, &v), DegreeDistance { wasserstein: 0.0, ks: 0.0 });
    }

    #[test]
    fn a_shift_by_one_degree() {
        // Degrees {0, 1, 1, 0} → {1, 2, 2, 1}: every node gains an edge.
        let before = version(4, &vec![(0, 1, 0, 1.0), (0, 2, 0, 1.0)], &[]);
        let after_edges = vec![
            (0, 0, 1, 1.0),
            (0, 1, 0, 1.0),
            (0, 1, 2, 1.0),
            (0, 2, 0, 1.0),
            (0, 2, 1, 1.0),
            (0, 3, 0, 1.0),
        ];
        let after = version(4, &after_edges, &[]);
        assert_eq!(distance(&before, &after), DegreeDistance { wasserstein: 1.0, ks: 0.5 });
    }

    #[test]
    fn tombstoned_nodes_are_left_out_and_isolated_nodes_count() {
        // Before: degrees 2 and 0 (node 2 tombstoned). After: one more node
        // of degree 0 in a larger id space: {2, 0, 0}.
        let before = version(3, &vec![(0, 0, 1, 1.0), (0, 0, 2, 1.0)], &[2]);
        let after = version(4, &vec![(0, 0, 1, 1.0), (0, 0, 3, 1.0)], &[2]);
        // F before: 1/2 up to degree 1, then 1; F after: 2/3, then 1.
        let d = distance(&before, &after);
        assert!((d.ks - 1.0 / 6.0).abs() < 1e-15);
        assert!((d.wasserstein - 2.0 / 6.0).abs() < 1e-15);
        assert_eq!(distance(&after, &before), d);
    }

    #[test]
    fn a_version_without_live_nodes_is_a_point_mass_at_zero() {
        let empty = version(2, &vec![], &[0, 1]);
        let star = version(3, &vec![(0, 0, 1, 1.0), (0, 0, 2, 1.0)], &[]);
        // Degrees {2, 0, 0} against all mass at 0.
        let d = distance(&empty, &star);
        assert!((d.wasserstein - 2.0 / 3.0).abs() < 1e-15);
        assert!((d.ks - 1.0 / 3.0).abs() < 1e-15);
    }
}
