//! `align_graph` charges at least the bytes its rebuild allocates at its
//! peak (DESIGN.md §8), measured with a counting global allocator. An
//! integration test, so the allocator is this test binary's alone; one
//! `#[test]`, so no other test allocates while it measures.

// Not under Miri: graph-sized allocations are far too slow there, and the
// resources hold the compute pool (see compute::workers).
#![cfg(not(miri))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use warren_core::compute::{ComputeMemory, Workers};
use warren_core::graph::{DenseId, EdgeList, GraphCsr, IdMap, RelationDictionary, RelationIndex};
use warren_core::interrupts::Uninterrupted;
use warren_core::spectral::{align_graph, align_ids, Resources};
use warren_core::version::GraphVersion;

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

/// One relation-indexed edge: (relation, source, target).
type Edge = (usize, DenseId, DenseId);

struct Case {
    nodes: u64,
    relations: usize,
    edges: Vec<Edge>,
    weighted: bool,
    reverse: bool,
}

/// A helper's result: test helpers propagate failures to the test.
type Outcome<T> = Result<T, Box<dyn Error>>;

/// Relations `r0`, `r1`, …, and their indexes.
fn dictionary(relations: usize) -> Outcome<(RelationDictionary, Vec<RelationIndex>)> {
    let mut dictionary = RelationDictionary::new();
    let indexes = (0..relations)
        .map(|relation| dictionary.intern(&format!("r{relation}")))
        .collect::<Result<_, _>>()?;
    Ok((dictionary, indexes))
}

fn graph(case: &Case, nodes: u64, edges: &[Edge]) -> Outcome<GraphCsr> {
    let (_, indexes) = dictionary(case.relations)?;
    let mut list = EdgeList::for_id_space(nodes, case.weighted);
    for &(relation, source, target) in edges {
        list.push(indexes[relation], source, target, 1.5)?;
    }
    Ok(GraphCsr::build(list, case.relations, nodes, case.reverse)?)
}

/// (bytes charged, bytes allocated at the peak) of aligning `case`'s graph
/// onto a one-node graph, so every node and relation is the other's own.
fn charged_and_peak(case: &Case) -> Outcome<(u64, u64)> {
    let (relations, _) = dictionary(case.relations)?;
    let this_ids = IdMap::new().rebuild(vec![-1i64])?;
    let other_ids = IdMap::new().rebuild((0..case.nodes as i64).collect())?;
    let (this_graph, other_graph) = (graph(case, 1, &[])?, graph(case, case.nodes, &case.edges)?);
    let this = GraphVersion::new(&this_ids, &relations, &this_graph)?;
    let other = GraphVersion::new(&other_ids, &relations, &other_graph)?;
    let workers = Workers::new(1)?;
    let memory = ComputeMemory::new(u64::MAX);
    let mut resources =
        Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted };
    let alignment = align_ids(&mut resources, &this, &other)?;
    let charged_before = memory.in_use();
    let live_before = LIVE.load(SeqCst);
    PEAK.store(live_before, SeqCst);
    let aligned = align_graph(&mut resources, &other, &alignment)?;
    let charged = memory.in_use() - charged_before;
    let peak = (PEAK.load(SeqCst) - live_before) as u64;
    drop(aligned);
    Ok((charged, peak))
}

#[test]
fn aligning_a_graph_charges_at_least_its_peak_allocation() -> Outcome<()> {
    let path = |count: u64| (0..count).map(|i| (0, i, i + 1)).collect::<Vec<_>>();
    let cases = [
        Case { nodes: 200_000, relations: 1, edges: vec![], weighted: false, reverse: false },
        Case { nodes: 200_000, relations: 1, edges: vec![], weighted: false, reverse: true },
        Case { nodes: 200_000, relations: 1, edges: path(1000), weighted: true, reverse: false },
        Case { nodes: 200_000, relations: 3, edges: vec![], weighted: false, reverse: true },
        Case {
            nodes: 10_000,
            relations: 1,
            edges: (0..200_000).map(|i| (0, i % 10_000, (i * 7) % 10_000)).collect(),
            weighted: true,
            reverse: false,
        },
        Case {
            nodes: 100_000,
            relations: 1,
            edges: (1..100_000).map(|i| (0, 0, i)).collect(),
            weighted: true,
            reverse: true,
        },
        Case {
            nodes: 5_000,
            relations: 3,
            edges: (0..100_000).map(|i| ((i % 3) as usize, i % 5_000, (i * 13) % 5_000)).collect(),
            weighted: true,
            reverse: true,
        },
    ];
    for (number, case) in cases.iter().enumerate() {
        let (charged, peak) = charged_and_peak(case)?;
        assert!(charged >= peak, "case {number}: charged {charged} < peak {peak}");
    }
    Ok(())
}
