//! A build charges at least the bytes it allocates at its peak to its
//! memory budget (DESIGN.md §5.4, `warren.max_build_memory`), measured with
//! a counting global allocator. An integration test, so the allocator is
//! this test binary's alone; one `#[test]`, so no other test allocates
//! while it measures.

// Not under Miri: graph-sized allocations are far too slow there.
#![cfg(not(miri))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use warren_core::graph::{
    BuildBuffer, BuildCharge, BuildMemory, BuildPlan, BuiltCsr, CsrLayout, CsrPart, CsrSink,
    EdgeIntake, GraphError, IdMap, Permutation, RelationDictionary,
};

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

/// A random graph: `nodes` external ids and `edges` rows of (source,
/// target, relation, weight), generated before anything is measured.
#[derive(Clone, Copy)]
struct Case {
    nodes: u64,
    edges: u64,
    relations: usize,
    weighted: bool,
    layout: CsrLayout,
    /// Edges are emitted in both directions.
    undirected: bool,
    /// The edge list is reserved for the row count up front, as from an
    /// exact planner estimate, rather than grown as rows arrive.
    reserve: bool,
    /// The build starts from a version over the same nodes and as many
    /// again that have since disappeared (tombstoned).
    base: bool,
    /// The first build orders its ids for locality (DESIGN.md §14 item
    /// 22) before its CSRs are planned.
    locality: bool,
}

impl Case {
    fn new(nodes: u64, edges: u64, relations: usize, weighted: bool, store_merged: bool) -> Self {
        let layout = CsrLayout { build_reverse: true, store_merged };
        let (undirected, reserve, base, locality) = (false, true, false, false);
        Self { nodes, edges, relations, weighted, layout, undirected, reserve, base, locality }
    }
}

/// One edge row in external ids.
type Row = (i64, i64, usize, f64);

/// G(n, m) with replacement, from a fixed linear congruential sequence.
fn rows(case: &Case) -> Vec<Row> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move |bound: u64| {
        state =
            state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % bound
    };
    (0..case.edges)
        .map(|_| {
            let (source, target) = (next(case.nodes), next(case.nodes));
            let relation = next(case.relations as u64) as usize;
            (source as i64 * 3, target as i64 * 3, relation, 0.25 * (next(8) + 1) as f64)
        })
        .collect()
}

/// The `id_map` and `tombstones` segments of the version a build starts
/// from: `nodes` ids that survive and as many that disappear.
fn base_segments(nodes: u64) -> Outcome<(Vec<u8>, Vec<u8>)> {
    let ids = (0..2 * nodes as i64).map(|id| if id % 2 == 0 { id / 2 * 3 } else { -id });
    let first = IdMap::new().rebuild(ids.collect())?;
    let base = first.rebuild((0..nodes as i64).map(|id| id * 3).collect())?;
    Ok((base.encode()?, base.tombstones().encode()?))
}

/// The chunk size segments are encoded in, scaled down with the graphs.
const CHUNK_SIZE: usize = 64 << 10;

/// A sink that encodes each CSR's segments in chunks and drops them, as a
/// refresh writes them to `warren.segments`.
struct Encoder {
    memory: BuildMemory,
    bytes: u64,
}

impl CsrSink for Encoder {
    type Error = GraphError;

    fn csr(&mut self, built: BuiltCsr) -> Result<(), GraphError> {
        for part in [CsrPart::Rows, CsrPart::Offsets, CsrPart::Targets, CsrPart::Weights] {
            let written = built.csr.encode_part(part, CHUNK_SIZE, &self.memory, &mut |_| Ok(()))?;
            self.bytes += written.unwrap_or(0);
        }
        Ok(())
    }
}

/// Bytes a build allocates without charging them: a fixed few kilobytes
/// (the plan, the budget and its groups, a segment name), whatever the
/// nodes, edges or relations. Everything that grows with them is charged,
/// the relation dictionary and the per-relation counts included.
const BOOKKEEPING: u64 = 4 << 10;

/// What a build of a case charged and allocated, in bytes.
#[derive(Debug)]
struct Measured {
    /// The most bytes allocated at once (the counting allocator's peak).
    allocated: u64,
    /// The most bytes charged at once.
    charged: u64,
    /// The most the plan said the build would charge: what was charged when
    /// it was made, plus its estimate and a chunk buffer.
    planned: u64,
}

/// Builds `case` from its node ids and edge rows through to its encoded
/// segments, charging `memory`, as a refresh does.
fn measure(case: &Case) -> Outcome<Measured> {
    let rows = rows(case);
    let names: Vec<String> = (0..case.relations).map(|relation| format!("r{relation}")).collect();
    let base = match case.base {
        true => Some(base_segments(case.nodes)?),
        false => None,
    };
    let memory = BuildMemory::unlimited();
    let live_before = LIVE.load(SeqCst);
    PEAK.store(live_before, SeqCst);

    let (base, base_charge) = match &base {
        Some((id_map, tombstones)) => IdMap::decode_within(id_map, tombstones, &memory)?,
        None => (IdMap::new(), BuildCharge::none(&memory)),
    };
    let mut node_ids = BuildBuffer::new(&memory);
    for id in 0..case.nodes as i64 {
        node_ids.push(id * 3)?;
    }
    let (mut id_map, _id_map_charge) = base.rebuild_within(node_ids)?;
    drop((base, base_charge));
    let mut relations = RelationDictionary::new();
    let mut relations_charge = BuildCharge::none(&memory);
    let mut intake = match case.undirected {
        true => EdgeIntake::undirected(&id_map, case.weighted),
        false => EdgeIntake::directed(&id_map, case.weighted),
    }
    .within(&memory);
    if case.reserve {
        intake.reserve(rows.len())?;
    }
    for &(source, target, relation, weight) in &rows {
        let relation = relations.intern_within(&names[relation], &mut relations_charge)?;
        intake.push(relation, Some(&source), Some(&target), weight)?;
    }
    let (mut edges, _) = intake.finish();
    if case.locality {
        let permutation = Permutation::locality(&edges, id_map.id_space(), &memory)?;
        edges.renumber(&permutation)?;
        id_map.renumber(&permutation, &memory)?;
    }
    let chunk = (CHUNK_SIZE + 8) as u64;
    let id_space = id_map.id_space();
    let plan = BuildPlan::new(&edges, relations.len(), id_space, case.layout, &memory, chunk)?;
    let planned = memory.peak().max(memory.in_use() + plan.bytes_needed() + chunk);
    let mut encoder = Encoder { memory: memory.clone(), bytes: 0 };
    plan.build(edges, &mut encoder)?;
    let ignore = &mut |_: &[u8]| Ok::<_, GraphError>(());
    encoder.bytes += id_map.encode_chunked(CHUNK_SIZE, &memory, ignore)?;
    encoder.bytes += id_map.tombstones().encode_chunked(CHUNK_SIZE, &memory, ignore)?;
    let _encoded_charge = memory.charge(relations.encoded_bytes())?;
    encoder.bytes += relations.encode()?.len() as u64;

    let allocated = (PEAK.load(SeqCst) - live_before) as u64;
    Ok(Measured { allocated, charged: memory.peak(), planned })
}

#[test]
fn a_build_charges_at_least_its_peak_allocation_and_plans_for_it() -> Outcome<()> {
    let one = Case::new(20_000, 200_000, 1, false, true);
    let merged = Case::new(20_000, 200_000, 5, true, true);
    let unmerged = Case::new(20_000, 200_000, 5, true, false);
    let cases = [
        one,
        merged,
        unmerged,
        Case { undirected: true, ..merged },
        Case { reserve: false, ..unmerged },
        Case { undirected: true, reserve: false, ..one },
        Case { base: true, ..merged },
        // A relation per row, as a mistaken relation column gives.
        Case::new(10, 20_000, 20_000, true, false),
        // Many relations of few rows each, whose offsets are sparse.
        Case::new(20_000, 200_000, 500, true, true),
        Case { undirected: true, ..Case::new(20_000, 200_000, 500, false, false) },
    ];
    for (number, case) in cases.iter().enumerate() {
        let measured = measure(case)?;
        eprintln!("case {number}: {measured:?}");
        assert!(
            measured.charged + BOOKKEEPING >= measured.allocated,
            "case {number}: {measured:?}"
        );
        assert!(measured.planned >= measured.charged, "case {number}: {measured:?}");
        // Sparse offsets: no plan holds an offsets array per relation.
        let dense_offsets = case.relations as u64 * (case.nodes + 1) * 8 * 2;
        if case.relations == 500 {
            assert!(measured.planned < dense_offsets / 4, "case {number}: {measured:?}");
        }
    }
    Ok(())
}
