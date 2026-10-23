//! Leiden's refinement charges at least the bytes it allocates at its peak
//! (DESIGN.md §6.7), measured with a counting global allocator. An
//! integration test, so the allocator is this test binary's alone; one
//! `#[test]`, so no other test allocates while it measures.

// Not under Miri: the compute pool's work stealing runs through
// crossbeam-epoch, which Miri's Stacked Borrows model rejects.
#![cfg(not(miri))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use warren_core::community::{refine, CommunityError, Refinement, Undirected};
use warren_core::compute::{ComputeError, ComputeMemory, Workers};
use warren_core::graph::{EdgeList, GraphCsr, RelationDictionary, Tombstones};
use warren_core::interrupts::{Cancellation, Uninterrupted};

/// The system allocator, counting the bytes live now and at the peak.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(bytes: usize) {
    let live = LIVE.fetch_add(bytes, SeqCst) + bytes;
    PEAK.fetch_max(live, SeqCst);
}

// SAFETY: every call is forwarded unchanged to `System`, which upholds the
// `GlobalAlloc` contract; the counters only observe sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        // SAFETY: the caller's `layout` obligations pass through to `System`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), SeqCst);
        // SAFETY: `pointer` came from `System` with `layout`, as the caller
        // guarantees for this allocator.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        match size >= layout.size() {
            true => grew(size - layout.size()),
            false => _ = LIVE.fetch_sub(layout.size() - size, SeqCst),
        }
        // SAFETY: the caller's obligations pass through to `System`.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// A helper's result: test helpers propagate failures to the test.
type Outcome<T> = Result<T, Box<dyn Error>>;

/// Bytes the compute pool's own bookkeeping may allocate beyond the
/// charges: a few hundred, varying with how the threads take the work,
/// and not growing with the graph.
const SLACK: u64 = 4096;

const OPTIONS: Refinement = Refinement { resolution: 1.0, seed: 0, randomness: 0.01 };

/// An undirected path of `nodes` nodes.
fn path(nodes: u64) -> Outcome<Undirected> {
    let relation = RelationDictionary::new().intern("edge")?;
    let mut edges = EdgeList::for_id_space(nodes, false);
    for node in 1..nodes {
        edges.push(relation, node - 1, node, 1.0)?;
    }
    let graph = GraphCsr::build(edges, 1, nodes, false)?;
    let memory = ComputeMemory::new(u64::MAX);
    Ok(Undirected::new(&memory, &Cancellation::new(), &graph, false, false)?)
}

/// Whether the refinement of `graph`'s `communities` fits in `limit` bytes.
fn fits(
    graph: &Undirected,
    tombstones: &Tombstones,
    communities: &[i64],
    workers: &Workers,
    limit: u64,
) -> Outcome<bool> {
    let memory = ComputeMemory::new(limit);
    let refined =
        refine(workers, &memory, &mut Uninterrupted, graph, tombstones, communities, None, OPTIONS);
    match refined {
        Ok(_) => Ok(true),
        Err(CommunityError::Compute(ComputeError::MemoryLimit { .. })) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// (the least budget the refinement fits in, the bytes it allocates at
/// its peak within that budget).
fn charged_and_peak(nodes: u64, size: u64, threads: usize) -> Outcome<(u64, u64)> {
    let graph = path(nodes)?;
    let communities: Vec<i64> = (0..nodes).map(|node| (node / size) as i64).collect();
    let tombstones = Tombstones::new(nodes)?;
    let workers = Workers::new(threads)?;
    let (mut refused, mut fitted) = (0u64, 1u64 << 32);
    while fitted - refused > 1 {
        let limit = refused + (fitted - refused) / 2;
        match fits(&graph, &tombstones, &communities, &workers, limit)? {
            true => fitted = limit,
            false => refused = limit,
        }
    }
    let live_before = LIVE.load(SeqCst);
    PEAK.store(live_before, SeqCst);
    assert!(fits(&graph, &tombstones, &communities, &workers, fitted)?);
    Ok((fitted, (PEAK.load(SeqCst) - live_before) as u64))
}

#[test]
fn refining_charges_at_least_its_peak_allocation() -> Outcome<()> {
    // Every node a community of its own, communities of a few nodes, and
    // one community of every node.
    for (nodes, size, threads) in [(50_000, 1, 4), (50_000, 3, 4), (20_000, 20_000, 1)] {
        let (charged, peak) = charged_and_peak(nodes, size, threads)?;
        let within = charged + SLACK >= peak;
        assert!(within, "{nodes} nodes by {size}: charged {charged} < peak {peak}");
    }
    Ok(())
}
