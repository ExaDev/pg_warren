//! The degree-distribution shift of a diff: per-direction degree histograms
//! over live nodes, their means, and the two-sample Kolmogorov–Smirnov
//! statistic between them.

use super::nodes::is_live;
use crate::graph::{memory, CsrRows, GraphError, IdKeys, IdMap};

/// How many live nodes have one degree, before and after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DegreeCount {
    /// The degree.
    pub degree: u64,
    /// Live nodes with this degree in the earlier version.
    pub before: u64,
    /// Live nodes with this degree in the later version.
    pub after: u64,
}

/// The shift of one direction's degree distribution between two versions.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DegreeShift {
    /// The degrees that occur in either version, ascending.
    pub histogram: Vec<DegreeCount>,
    /// The mean degree over live nodes; `None` without live nodes.
    pub mean_before: Option<f64>,
    /// The mean degree over live nodes; `None` without live nodes.
    pub mean_after: Option<f64>,
    /// The largest gap between the two empirical degree CDFs, in `[0, 1]`;
    /// `None` when either version has no live nodes.
    pub ks_statistic: Option<f64>,
}

/// Live nodes per degree (index) of the out-degrees in `csr`.
pub(super) fn out_degree_counts<K: IdKeys>(
    id_map: &IdMap<K>,
    csr: CsrRows<'_>,
) -> Result<Vec<u64>, GraphError> {
    counts_by_degree(id_map, |dense| csr.degree(dense))
}

/// Live nodes per degree (index) of the in-degrees of `csr`, counted from
/// its targets so that no reverse CSR is needed.
pub(super) fn in_degree_counts<K: IdKeys>(
    id_map: &IdMap<K>,
    csr: CsrRows<'_>,
) -> Result<Vec<u64>, GraphError> {
    let mut in_degrees = memory::filled(csr.id_space() as usize, 0u64)?;
    for source in 0..csr.id_space() {
        for target in csr.neighbours(source) {
            in_degrees[target as usize] += 1;
        }
    }
    counts_by_degree(id_map, |dense| in_degrees.get(dense as usize).copied().unwrap_or(0))
}

/// Live nodes per degree, indexed by degree, for a degree function.
fn counts_by_degree<K: IdKeys>(
    id_map: &IdMap<K>,
    degree: impl Fn(u64) -> u64,
) -> Result<Vec<u64>, GraphError> {
    let live = || (0..id_map.id_space()).filter(|&dense| is_live(id_map, dense));
    let Some(max) = live().map(&degree).max() else {
        return Ok(Vec::new());
    };
    let mut counts = memory::filled(max as usize + 1, 0u64)?;
    for dense in live() {
        counts[degree(dense) as usize] += 1;
    }
    Ok(counts)
}

/// The shift from `before` to `after`, each giving live nodes per degree.
pub(super) fn degree_shift(before: &[u64], after: &[u64]) -> DegreeShift {
    let count = |counts: &[u64], degree: usize| counts.get(degree).copied().unwrap_or(0);
    let (n_before, n_after): (u64, u64) = (before.iter().sum(), after.iter().sum());
    let mut histogram = Vec::new();
    let (mut cumulative_before, mut cumulative_after) = (0u128, 0u128);
    let (mut sum_before, mut sum_after) = (0u128, 0u128);
    let mut largest_gap = 0u128;
    for degree in 0..before.len().max(after.len()) {
        let (b, a) = (count(before, degree), count(after, degree));
        if b == 0 && a == 0 {
            continue;
        }
        histogram.push(DegreeCount { degree: degree as u64, before: b, after: a });
        sum_before += u128::from(b) * degree as u128;
        sum_after += u128::from(a) * degree as u128;
        cumulative_before += u128::from(b);
        cumulative_after += u128::from(a);
        // CDF gap scaled by n_before × n_after, exact in integers.
        let gap = (cumulative_before * u128::from(n_after))
            .abs_diff(cumulative_after * u128::from(n_before));
        largest_gap = largest_gap.max(gap);
    }
    let mean = |sum: u128, n: u64| (n > 0).then(|| sum as f64 / n as f64);
    let both = u128::from(n_before) * u128::from(n_after);
    DegreeShift {
        histogram,
        mean_before: mean(sum_before, n_before),
        mean_after: mean(sum_after, n_after),
        ks_statistic: (both > 0).then(|| largest_gap as f64 / both as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::history::Build;

    #[test]
    fn counts_cover_live_nodes_only() {
        // 1 → {2, 3}, 2 → {3}; then 4 appended and 2 tombstoned.
        let v1 =
            Build::first(&[1, 2, 3], &[(0, 1, 2, 1), (0, 1, 3, 1), (0, 2, 3, 1)], false, false);
        let forward = v1.graph.all_relations().forward();
        assert_eq!(out_degree_counts(&v1.id_map, forward).unwrap(), [1, 1, 1]);
        assert_eq!(in_degree_counts(&v1.id_map, forward).unwrap(), [1, 1, 1]);
        let v2 = v1.next(&[1, 3, 4], &[(0, 1, 3, 1), (0, 4, 3, 1)], false, false);
        let forward = v2.graph.all_relations().forward();
        assert_eq!(out_degree_counts(&v2.id_map, forward).unwrap(), [1, 2]);
        assert_eq!(in_degree_counts(&v2.id_map, forward).unwrap(), [2, 0, 1]);
    }

    #[test]
    fn the_shift_reports_histogram_means_and_ks_statistic() {
        // before: degrees {0, 1, 1, 2}; after: {1, 1, 3}.
        let shift = degree_shift(&[1, 2, 1], &[0, 2, 0, 1]);
        assert_eq!(
            shift.histogram,
            [
                DegreeCount { degree: 0, before: 1, after: 0 },
                DegreeCount { degree: 1, before: 2, after: 2 },
                DegreeCount { degree: 2, before: 1, after: 0 },
                DegreeCount { degree: 3, before: 0, after: 1 },
            ]
        );
        assert_eq!(shift.mean_before, Some(1.0));
        assert_eq!(shift.mean_after, Some(5.0 / 3.0));
        // CDFs at 0..3: before 1/4, 3/4, 1, 1; after 0, 2/3, 2/3, 1.
        assert_eq!(shift.ks_statistic, Some(1.0 / 3.0));
    }

    #[test]
    fn identical_distributions_have_no_shift_and_empty_ones_no_statistic() {
        assert_eq!(degree_shift(&[2, 3], &[2, 3]).ks_statistic, Some(0.0));
        let empty = degree_shift(&[], &[0, 1]);
        assert_eq!(empty.mean_before, None);
        assert_eq!(empty.mean_after, Some(1.0));
        assert_eq!(empty.ks_statistic, None);
        assert_eq!(empty.histogram, [DegreeCount { degree: 1, before: 0, after: 1 }]);
    }
}
