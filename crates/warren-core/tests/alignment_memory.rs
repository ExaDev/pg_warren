//! `aligned_edit_distance` allocates nothing graph-sized and charges
//! nothing (DESIGN.md §8): it reads both versions where they are, so what
//! it allocates is its result, the same for a graph of any size with the
//! same relations. Measured with a counting global allocator that counts
//! only the measuring thread's allocations, while it measures: the compute
//! pools of earlier measurements may still be winding their threads down.
//! An integration test, so the allocator is this test binary's alone.

// Not under Miri: graph-sized allocations are far too slow there, and the
// resources hold the compute pool (see compute::workers).
#![cfg(not(miri))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::error::Error;
use std::sync::atomic::{AtomicIsize, Ordering::SeqCst};

use warren_core::compute::{ComputeMemory, Workers};
use warren_core::graph::{DenseId, EdgeList, GraphCsr, IdMap, RelationDictionary, RelationIndex};
use warren_core::interrupts::Uninterrupted;
use warren_core::spectral::{align_ids, aligned_edit_distance, Resources};
use warren_core::version::GraphVersion;

/// The system allocator, counting the bytes the measuring thread has
/// allocated since it started measuring, now and at the peak (negative
/// when it frees more than it allocates).
struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    /// Whether this thread is measuring.
    static MEASURING: Cell<bool> = const { Cell::new(false) };
}

fn changed(bytes: isize) {
    if MEASURING.try_with(Cell::get).unwrap_or(false) {
        let live = LIVE.fetch_add(bytes, SeqCst) + bytes;
        PEAK.fetch_max(live, SeqCst);
    }
}

/// `bytes` as a change in the live count.
fn signed(bytes: usize) -> isize {
    isize::try_from(bytes).unwrap_or(isize::MAX)
}

// SAFETY: every call is forwarded unchanged to `System`, which upholds the
// `GlobalAlloc` contract; the counters only observe sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        changed(signed(layout.size()));
        // SAFETY: the caller's `layout` obligations pass through to `System`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        changed(-signed(layout.size()));
        // SAFETY: `pointer` came from `System` with `layout`, as the caller
        // guarantees for this allocator.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        changed(signed(size) - signed(layout.size()));
        // SAFETY: the caller's obligations pass through to `System`.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// One relation-indexed edge: (relation, source, target).
type Edge = (usize, DenseId, DenseId);

/// A helper's result: test helpers propagate failures to the test.
type Outcome<T> = Result<T, Box<dyn Error>>;

/// Relations `r0`, `r1`, `r2`, and their indexes.
fn dictionary() -> Outcome<(RelationDictionary, Vec<RelationIndex>)> {
    let mut dictionary = RelationDictionary::new();
    let indexes = (0..3)
        .map(|relation| dictionary.intern(&format!("r{relation}")))
        .collect::<Result<_, _>>()?;
    Ok((dictionary, indexes))
}

fn graph(nodes: u64, edges: &[Edge], weighted: bool, reverse: bool) -> Outcome<GraphCsr> {
    let (_, indexes) = dictionary()?;
    let mut list = EdgeList::for_id_space(nodes, weighted);
    for &(relation, source, target) in edges {
        list.push(indexes[relation], source, target, 1.5)?;
    }
    Ok(GraphCsr::build(list, 3, nodes, reverse)?)
}

/// Four edges from each of `nodes` nodes, over three relations.
fn edges(nodes: u64, shift: u64) -> Vec<Edge> {
    (0..nodes * 4).map(|i| ((i % 3) as usize, i % nodes, (i * 7 + shift) % nodes)).collect()
}

/// (bytes charged, bytes allocated at the peak) of the `edit` distance
/// between two graphs of `nodes` nodes, about half of them shared.
fn charged_and_peak(nodes: u64, weighted: bool, reverse: bool) -> Outcome<(u64, u64)> {
    let (relations, _) = dictionary()?;
    let this_ids = IdMap::new().rebuild((0..nodes as i64).collect())?;
    let other_ids = IdMap::new().rebuild((0..nodes as i64).map(|id| id * 2).collect())?;
    let this_graph = graph(nodes, &edges(nodes, 0), weighted, reverse)?;
    let other_graph = graph(nodes, &edges(nodes, 1), !weighted, !reverse)?;
    let this = GraphVersion::new(&this_ids, &relations, &this_graph)?;
    let other = GraphVersion::new(&other_ids, &relations, &other_graph)?;
    let workers = Workers::new(1)?;
    let memory = ComputeMemory::new(u64::MAX);
    let mut resources =
        Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted };
    let alignment = align_ids(&mut resources, &this, &other)?;
    let charged_before = memory.in_use();
    LIVE.store(0, SeqCst);
    PEAK.store(0, SeqCst);
    MEASURING.set(true);
    let distance = aligned_edit_distance(&mut Uninterrupted, &this, &other, &alignment);
    MEASURING.set(false);
    let distance = distance?;
    let charged = memory.in_use() - charged_before;
    let peak = u64::try_from(PEAK.load(SeqCst)).unwrap_or(0);
    assert!(distance.overall.union > 0);
    drop(distance);
    Ok((charged, peak))
}

#[test]
fn comparing_two_graphs_allocates_only_its_result() -> Outcome<()> {
    for (weighted, reverse) in [(false, false), (true, true), (true, false)] {
        let small = charged_and_peak(10, weighted, reverse)?;
        let large = charged_and_peak(200_000, weighted, reverse)?;
        assert_eq!(small, large, "weighted {weighted}, reverse {reverse}");
        assert_eq!(large.0, 0, "charged {}", large.0);
    }
    Ok(())
}
