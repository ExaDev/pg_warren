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
    BuildBuffer, BuildMemory, BuildPlan, BuiltCsr, CsrLayout, CsrPart, CsrSink, EdgeIntake,
    GraphError, IdMap, RelationDictionary,
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
struct Case {
    nodes: u64,
    edges: u64,
    relations: usize,
    weighted: bool,
    layout: CsrLayout,
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
        for part in [CsrPart::Offsets, CsrPart::Targets, CsrPart::Weights] {
            let written = built.csr.encode_part(part, CHUNK_SIZE, &self.memory, &mut |_| Ok(()))?;
            self.bytes += written.unwrap_or(0);
        }
        Ok(())
    }
}

/// Bytes a build allocates without charging them: the vectors that track
/// relations and groups, which grow with the relation count but not with
/// the nodes or edges.
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
    let mut relations = RelationDictionary::new();
    let indexes = (0..case.relations)
        .map(|relation| relations.intern(&format!("r{relation}")))
        .collect::<Result<Vec<_>, _>>()?;
    let memory = BuildMemory::unlimited();
    let live_before = LIVE.load(SeqCst);
    PEAK.store(live_before, SeqCst);

    let mut node_ids = BuildBuffer::new(&memory);
    for id in 0..case.nodes as i64 {
        node_ids.push(id * 3)?;
    }
    let (id_map, _id_map_charge) = IdMap::new().rebuild_within(node_ids)?;
    let mut intake = EdgeIntake::directed(&id_map, case.weighted).within(&memory);
    intake.reserve(rows.len())?;
    for &(source, target, relation, weight) in &rows {
        intake.push(indexes[relation], Some(&source), Some(&target), weight)?;
    }
    let (edges, _) = intake.finish();
    let plan = BuildPlan::new(&edges, case.relations, id_map.id_space(), case.layout, &memory)?;
    let chunk = (CHUNK_SIZE + 8) as u64;
    let planned = memory.peak().max(memory.in_use() + plan.bytes_needed() + chunk);
    let mut encoder = Encoder { memory: memory.clone(), bytes: 0 };
    plan.build(edges, &mut encoder)?;
    encoder.bytes +=
        id_map.encode_chunked(CHUNK_SIZE, &memory, &mut |_| Ok::<_, GraphError>(()))?;
    let tombstones = id_map.tombstones();
    encoder.bytes +=
        tombstones.encode_chunked(CHUNK_SIZE, &memory, &mut |_| Ok::<_, GraphError>(()))?;

    let allocated = (PEAK.load(SeqCst) - live_before) as u64;
    Ok(Measured { allocated, charged: memory.peak(), planned })
}

#[test]
fn a_build_charges_at_least_its_peak_allocation_and_plans_for_it() -> Outcome<()> {
    let reverse = |store_merged| CsrLayout { build_reverse: true, store_merged };
    let cases = [
        Case {
            nodes: 20_000,
            edges: 200_000,
            relations: 1,
            weighted: false,
            layout: reverse(true),
        },
        Case { nodes: 20_000, edges: 200_000, relations: 5, weighted: true, layout: reverse(true) },
        Case {
            nodes: 20_000,
            edges: 200_000,
            relations: 5,
            weighted: true,
            layout: reverse(false),
        },
    ];
    for (number, case) in cases.iter().enumerate() {
        let measured = measure(case)?;
        eprintln!("case {number}: {measured:?}");
        assert!(
            measured.charged + BOOKKEEPING >= measured.allocated,
            "case {number}: {measured:?}"
        );
        assert!(measured.planned >= measured.charged, "case {number}: {measured:?}");
    }
    Ok(())
}
