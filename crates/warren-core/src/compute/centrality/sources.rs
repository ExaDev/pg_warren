//! The sources a centrality's shortest paths start from (DESIGN.md §6.7):
//! every live node, for the exact centralities, or `k` of them drawn
//! without replacement from a seeded sample.
//!
//! The sample is a bottom-k sample: each live node of the population has a
//! key, a seeded hash of its dense id, and the `k` smallest keys are taken.
//! Every `k`-subset is equally likely, the sample depends only on the seed
//! and the dense ids (never on the thread count), and a node keeps its key
//! across graph versions while it keeps its dense id. So after a refresh a
//! seed samples the same nodes only if no node joined with a smaller key
//! (it displaces the sampled node with the largest) and none sampled was
//! deleted; `compact` renumbers dense ids, and so re-keys every node.
//! Sources are then taken in ascending dense id order.

use std::collections::BinaryHeap;
use std::mem::size_of;

use super::super::random::{mix, GOLDEN};
use super::super::{ComputeError, ComputeMemory, Mask, MemoryCharge};
use crate::graph::{DenseId, Tombstones};
use crate::interrupts::{Cancellation, Work};

/// How the sources are chosen.
#[derive(Debug, Clone, Copy)]
pub struct Sampling<'a> {
    /// How many sources to sample (at most the population); `None` takes
    /// every node of the population.
    pub k: Option<u64>,
    /// The sample's seed.
    pub seed: u64,
    /// The nodes sources are drawn from (those the mask selects); `None`
    /// is every live node.
    pub population: Option<Mask<'a>>,
}

/// The chosen sources, ascending.
#[derive(Debug)]
pub(super) struct Sources {
    nodes: Vec<DenseId>,
    /// Whether the sources are every live node: the exact centrality.
    every_live_node: bool,
    _charge: MemoryCharge,
}

impl Sources {
    /// The bytes [`Self::choose`] charges: the live nodes, and the heap a
    /// sample of `k` of them is kept in.
    pub(super) fn bytes(sampling: &Sampling<'_>, tombstones: &Tombstones) -> u64 {
        let live = tombstones.id_space() - tombstones.count();
        let heap = sampling.k.map_or(0, |k| k.min(live) + 1);
        let nodes = live.saturating_mul(size_of::<DenseId>() as u64);
        nodes.saturating_add(heap.saturating_mul(size_of::<(u64, DenseId)>() as u64))
    }

    /// The sources `sampling` chooses among the live nodes.
    pub(super) fn choose(
        memory: &ComputeMemory,
        cancellation: &Cancellation,
        sampling: &Sampling<'_>,
        tombstones: &Tombstones,
    ) -> Result<Self, ComputeError> {
        let id_space = tombstones.id_space();
        if let Some(population) = &sampling.population {
            population.check(id_space)?;
        }
        let charge = memory.charge(Self::bytes(sampling, tombstones))?;
        let live = id_space - tombstones.count();
        let mut population = Vec::new();
        population
            .try_reserve_exact(live as usize)
            .map_err(|_| ComputeError::OutOfMemory { bytes: charge.bytes() })?;
        let mut work = Work::new(cancellation);
        for node in 0..id_space {
            work.spend(1)?;
            let selected = sampling.population.is_none_or(|mask| mask.selects(node));
            if selected && !tombstones.contains(node) {
                population.push(node);
            }
        }
        let nodes = match sampling.k {
            Some(k) if k < population.len() as u64 => {
                sample(&population, k, sampling.seed, &mut work)?
            }
            _ => population,
        };
        Ok(Self { every_live_node: nodes.len() as u64 == live, nodes, _charge: charge })
    }

    /// The sources, ascending.
    pub(super) fn nodes(&self) -> &[DenseId] {
        &self.nodes
    }

    /// Whether the sources are every live node.
    pub(super) fn every_live_node(&self) -> bool {
        self.every_live_node
    }
}

/// The `k` nodes of `population` with the smallest keys, ascending.
fn sample(
    population: &[DenseId],
    k: u64,
    seed: u64,
    work: &mut Work<'_>,
) -> Result<Vec<DenseId>, ComputeError> {
    let bytes = (k + 1).saturating_mul(size_of::<(u64, DenseId)>() as u64);
    let mut smallest = BinaryHeap::new();
    smallest.try_reserve_exact(k as usize + 1).map_err(|_| ComputeError::OutOfMemory { bytes })?;
    for &node in population {
        work.spend(1)?;
        smallest.push((key(seed, node), node));
        if smallest.len() as u64 > k {
            smallest.pop();
        }
    }
    let mut nodes: Vec<DenseId> = smallest.into_iter().map(|(_, node)| node).collect();
    nodes.sort_unstable();
    Ok(nodes)
}

/// Node `node`'s key under `seed`: distinct nodes have distinct keys.
fn key(seed: u64, node: DenseId) -> u64 {
    mix(mix(seed) ^ node.wrapping_mul(GOLDEN))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;
    use crate::compute::{AnyVector, Vector};

    fn tombstones(id_space: u64, tombstoned: &[DenseId]) -> Tombstones {
        let mut tombstones = Tombstones::new(id_space).unwrap();
        tombstoned.iter().for_each(|&node| tombstones.insert(node).unwrap());
        tombstones
    }

    fn choose(sampling: &Sampling<'_>, tombstones: &Tombstones) -> Result<Sources, ComputeError> {
        let memory = ComputeMemory::new(1 << 20);
        Sources::choose(&memory, &Cancellation::new(), sampling, tombstones)
    }

    fn mask(values: Vec<bool>) -> AnyVector {
        let memory = ComputeMemory::new(1 << 20);
        let id_space = values.len() as u64;
        Vector::from_values(&memory, id_space, 1, values).unwrap().into()
    }

    #[test]
    fn without_k_every_live_node_is_a_source() {
        let sampling = Sampling { k: None, seed: 0, population: None };
        let sources = choose(&sampling, &tombstones(5, &[2])).unwrap();
        assert_eq!(sources.nodes(), &[0, 1, 3, 4]);
        assert!(sources.every_live_node());
    }

    #[test]
    fn a_population_restricts_the_sources() {
        let selected = mask(vec![true, false, true, true, false]);
        let population = Some(Mask { vector: &selected, complement: false });
        let sampling = Sampling { k: None, seed: 0, population };
        let sources = choose(&sampling, &tombstones(5, &[3])).unwrap();
        assert_eq!(sources.nodes(), &[0, 2]);
        assert!(!sources.every_live_node());
    }

    #[test]
    fn k_at_least_the_population_takes_it_all() {
        let sampling = Sampling { k: Some(9), seed: 3, population: None };
        let sources = choose(&sampling, &tombstones(4, &[])).unwrap();
        assert_eq!(sources.nodes(), &[0, 1, 2, 3]);
        assert!(sources.every_live_node());
    }

    #[test]
    fn a_sample_depends_only_on_the_seed() {
        let none = tombstones(1000, &[]);
        let sample = |seed| {
            let sampling = Sampling { k: Some(10), seed, population: None };
            choose(&sampling, &none).unwrap().nodes().to_vec()
        };
        assert_eq!(sample(7), sample(7));
        assert_ne!(sample(7), sample(8));
    }

    #[test]
    fn every_node_is_sampled_about_equally_often() {
        // 2,000 samples of 5 of 20 nodes: each node 500 times expected.
        let none = tombstones(20, &[]);
        let mut counts = [0u32; 20];
        for seed in 0..2000 {
            let sampling = Sampling { k: Some(5), seed, population: None };
            for &node in choose(&sampling, &none).unwrap().nodes() {
                counts[node as usize] += 1;
            }
        }
        assert!(counts.iter().all(|&c| (400..600).contains(&c)), "{counts:?}");
    }

    #[test]
    fn a_mask_of_another_id_space_is_refused() {
        let selected = mask(vec![true; 3]);
        let population = Some(Mask { vector: &selected, complement: false });
        let sampling = Sampling { k: None, seed: 0, population };
        let refused = choose(&sampling, &tombstones(4, &[])).map(|_| ());
        assert_eq!(refused, Err(ComputeError::IdSpaceMismatch { expected: 4, found: 3 }));
    }

    #[test]
    fn a_cancelled_choice_stops() {
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let memory = ComputeMemory::new(1 << 24);
        let sampling = Sampling { k: Some(3), seed: 0, population: None };
        let chosen = Sources::choose(&memory, &cancellation, &sampling, &tombstones(1 << 17, &[]));
        assert_eq!(chosen.map(|_| ()), Err(ComputeError::Cancelled));
    }

    proptest! {
        #[test]
        fn samples_are_k_distinct_live_nodes_of_the_population(
            id_space in 1u64..60,
            tombstoned in prop::collection::vec(0u64..60, 0..10),
            selected in prop::collection::vec(any::<bool>(), 60),
            k in 1u64..70,
            seed in any::<u64>(),
        ) {
            let tombstoned: Vec<_> = tombstoned.into_iter().filter(|&t| t < id_space).collect();
            let tombstones = tombstones(id_space, &tombstoned);
            let selected = mask(selected[..id_space as usize].to_vec());
            let mask = Mask { vector: &selected, complement: false };
            let sampling = Sampling { k: Some(k), seed, population: Some(mask) };
            let memory = ComputeMemory::new(1 << 20);
            let sources = Sources::choose(&memory, &Cancellation::new(), &sampling, &tombstones).unwrap();
            let eligible: BTreeSet<_> = (0..id_space)
                .filter(|&n| !tombstones.contains(n) && mask.selects(n))
                .collect();
            let nodes = sources.nodes();
            prop_assert!(nodes.windows(2).all(|w| w[0] < w[1]));
            prop_assert!(nodes.iter().all(|n| eligible.contains(n)));
            prop_assert_eq!(nodes.len() as u64, k.min(eligible.len() as u64));
            prop_assert!(memory.in_use() <= Sources::bytes(&sampling, &tombstones));
        }
    }
}
