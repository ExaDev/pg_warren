//! Measures what the locality node order buys (DESIGN.md §14 item 22):
//! SpMV, BFS and components on one graph whose dense ids are in a random
//! order (`id`: external ids that carry no structure, such as uuids or
//! insertion order across a busy table) and in locality order (Reverse
//! Cuthill–McKee over the same edges), plus the ordering's own cost. For the
//! community-structured graph it also times the planted order (each
//! community's nodes contiguous), the best a community-aware order could
//! do. Prints one JSON line per graph, order and kernel; each time is the
//! median of `RUNS` runs.
//!
//! The graphs are seeded and directed, with reverse CSRs:
//! - `sbm`: a stochastic block model, communities of 1,000 nodes, each edge
//!   inside its source's community with probability 0.9;
//! - `powerlaw`: Chung–Lu with expected degrees following a power law of
//!   exponent 2.1 (hubs of tens of thousands of edges).
//!
//! ```text
//! cargo run --release -p warren-core --example locality -- sbm|powerlaw NODES DEGREE RUNS
//! ```

use std::error::Error;
use std::ops::ControlFlow;
use std::thread::available_parallelism;
use std::time::Instant;

use warren_core::compute::{spmv, AnyVector, ComputeMemory, Product, Semiring, Vector, Workers};
use warren_core::graph::{
    BuildBuffer, BuildMemory, DenseId, Direction, EdgeIntake, EdgeList, FixedKeys, GraphCsr, IdMap,
    Permutation, RelationDictionary, RelationSelection,
};
use warren_core::interrupts::Uninterrupted;
use warren_core::traversal::{bfs_each, components, Adjacency, Limits};

/// Nodes per community of the block model.
const COMMUNITY: u64 = 1_000;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [kind, nodes, degree, runs] = args.as_slice() else {
        return Err("usage: sbm|powerlaw NODES DEGREE RUNS".into());
    };
    let (nodes, degree, runs): (u64, u64, usize) = (nodes.parse()?, degree.parse()?, runs.parse()?);
    let mut random = Lcg(0x2545_f491_4f6c_dd1d);
    let natural = match kind.as_str() {
        "sbm" => block_model(nodes, degree, &mut random)?,
        "powerlaw" => power_law(nodes, degree, &mut random)?,
        _ => return Err("the graph is sbm or powerlaw".into()),
    };
    let scramble = shuffled(nodes, &mut random);
    let start = scramble[0];
    let mut orders = vec![("id", renumbered(&natural, nodes, |node| scramble[node as usize])?)];
    let edges = orders[0].1.clone();
    let mut scratch = 0;
    let (permutation, ordering) = timed(runs, || {
        let memory = BuildMemory::unlimited();
        let permutation = Permutation::locality(&edges, nodes, &memory);
        scratch = memory.peak();
        permutation
    })?;
    let mut local = edges.clone();
    let started = Instant::now();
    local.renumber(&permutation)?;
    let renumbering = started.elapsed().as_secs_f64();
    let (intake_peak, ordering_peak) = first_build_peaks(&edges, nodes)?;
    println!(
        "{{\"graph\": \"{kind}\", \"nodes\": {nodes}, \"edges\": {}, \"kernel\": \"order\", \"median_seconds\": {ordering:e}, \"renumber_seconds\": {renumbering:e}, \"scratch_bytes\": {scratch}, \"first_build_intake_peak_bytes\": {intake_peak}, \"first_build_ordering_peak_bytes\": {ordering_peak}}}",
        edges.len(),
    );
    let local_start = permutation.new_dense_id(start);
    orders.push(("locality", local));
    if kind == "sbm" {
        orders.push(("planted", natural));
    }
    for (order, edges) in orders {
        let start = match order {
            "id" => start,
            "locality" => local_start,
            _ => 0,
        };
        measure(kind, order, edges, nodes, start, runs)?;
    }
    Ok(())
}

/// Times SpMV (one thread and all), BFS and components on `edges` in one
/// order, and reports the mean gap between an edge's endpoints' ids.
fn measure(
    kind: &str,
    order: &str,
    edges: EdgeList,
    nodes: u64,
    start: DenseId,
    runs: usize,
) -> Result<(), Box<dyn Error>> {
    let gap = edges.edges().map(|(_, s, t)| s.abs_diff(t) as f64).sum::<f64>() / edges.len() as f64;
    let graph = GraphCsr::build(edges, 1, nodes, true)?;
    let report = |kernel: &str, seconds: f64| {
        println!(
            "{{\"graph\": \"{kind}\", \"order\": \"{order}\", \"kernel\": \"{kernel}\", \"median_seconds\": {seconds:e}, \"mean_id_gap\": {gap:.0}}}"
        );
    };
    let memory = ComputeMemory::new(u64::MAX);
    let x = AnyVector::from(Vector::filled(&memory, nodes, 1, 1.0f64)?);
    let mut y = AnyVector::from(Vector::filled(&memory, nodes, 1, 0.0f64)?);
    let cores = available_parallelism().map_or(1, |n| n.get());
    for threads in [1, cores] {
        let workers = Workers::new(threads)?;
        let product = Product::new(Semiring::PlusTimes);
        let (_, seconds) = timed(runs, || {
            spmv(&workers, &memory, &mut Uninterrupted, &graph, &product, &x, &mut y)
        })?;
        report(&format!("spmv_threads_{threads}"), seconds);
    }
    let adjacency = Adjacency::new(&graph, Direction::Out, RelationSelection::All)?;
    let (_, seconds) = timed(runs, || {
        bfs_each(&adjacency, &[start], None, Limits::default(), &mut Uninterrupted, |_| {
            ControlFlow::Continue(())
        })
    })?;
    report("bfs", seconds);
    let (_, seconds) = timed(runs, || components(&graph, &mut Uninterrupted))?;
    report("components", seconds);
    Ok(())
}

/// The build memory a first build charges, as a refresh does, for `int8`
/// node ids `0..nodes` and the edges of `edges` (one relation,
/// unweighted, reserved up front): its peak once the edges are read, and
/// its peak once they are ordered for locality and renumbered.
fn first_build_peaks(edges: &EdgeList, nodes: u64) -> Result<(u64, u64), Box<dyn Error>> {
    let memory = BuildMemory::unlimited();
    let mut ids = BuildBuffer::with_capacity(&memory, nodes as usize)?;
    for id in 0..nodes as i64 {
        ids.push(id)?;
    }
    let (mut id_map, _id_map_charge) = IdMap::<FixedKeys<i64>>::new().rebuild_within(ids)?;
    let relation = RelationDictionary::new().intern("edge")?;
    let mut intake = EdgeIntake::directed(&id_map, false).within(&memory);
    intake.reserve(edges.len())?;
    for (_, source, target) in edges.edges() {
        intake.push(relation, Some(&(source as i64)), Some(&(target as i64)), 1.0)?;
    }
    let (mut list, _) = intake.finish();
    let intake_peak = memory.peak();
    let permutation = Permutation::locality(&list, nodes, &memory)?;
    list.renumber(&permutation)?;
    id_map.renumber(&permutation, &memory)?;
    Ok((intake_peak, memory.peak()))
}

/// Runs `work` `runs` times; its last result and the median seconds.
fn timed<T, E: Error + 'static>(
    runs: usize,
    mut work: impl FnMut() -> Result<T, E>,
) -> Result<(T, f64), Box<dyn Error>> {
    let mut seconds = Vec::new();
    let mut last = None;
    for _ in 0..runs.max(1) {
        let started = Instant::now();
        last = Some(work()?);
        seconds.push(started.elapsed().as_secs_f64());
    }
    seconds.sort_by(f64::total_cmp);
    let result = last.ok_or("no run")?;
    Ok((result, seconds[seconds.len() / 2]))
}

/// `edges` with every endpoint mapped through `id`.
fn renumbered(
    edges: &EdgeList,
    nodes: u64,
    id: impl Fn(DenseId) -> DenseId,
) -> Result<EdgeList, Box<dyn Error>> {
    let relation = RelationDictionary::new().intern("edge")?;
    let mut list = EdgeList::for_id_space(nodes, false);
    list.reserve(edges.len())?;
    for (_, source, target) in edges.edges() {
        list.push(relation, id(source), id(target), 1.0)?;
    }
    Ok(list)
}

/// A seeded 64-bit linear congruential generator.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 =
            self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn unit(&mut self) -> f64 {
        self.next() as f64 / (1u64 << 53) as f64
    }
}

/// A uniformly random permutation of `0..nodes` (Fisher–Yates).
fn shuffled(nodes: u64, random: &mut Lcg) -> Vec<DenseId> {
    let mut ids: Vec<DenseId> = (0..nodes).collect();
    for i in (1..ids.len()).rev() {
        ids.swap(i, random.below(i as u64 + 1) as usize);
    }
    ids
}

/// `degree` out-edges per node; each target lies in the source's community
/// (consecutive ids) with probability 0.9, else anywhere.
fn block_model(nodes: u64, degree: u64, random: &mut Lcg) -> Result<EdgeList, Box<dyn Error>> {
    let mut edges = EdgeList::for_id_space(nodes, false);
    let relation = RelationDictionary::new().intern("edge")?;
    for source in 0..nodes {
        let community = source / COMMUNITY * COMMUNITY;
        let size = COMMUNITY.min(nodes - community);
        for _ in 0..degree {
            let target = match random.unit() < 0.9 {
                true => community + random.below(size),
                false => random.below(nodes),
            };
            edges.push(relation, source, target, 1.0)?;
        }
    }
    Ok(edges)
}

/// `nodes × degree` edges whose endpoints are drawn with probability
/// proportional to `(i + 1)^(-1 / (2.1 - 1))`: expected degrees follow a
/// power law of exponent 2.1 (Chung–Lu), hubs at the low ids.
fn power_law(nodes: u64, degree: u64, random: &mut Lcg) -> Result<EdgeList, Box<dyn Error>> {
    let mut edges = EdgeList::for_id_space(nodes, false);
    let relation = RelationDictionary::new().intern("edge")?;
    let exponent = -1.0 / (2.1 - 1.0);
    let mut cumulative = Vec::with_capacity(nodes as usize);
    let mut total = 0.0;
    for i in 0..nodes {
        total += ((i + 1) as f64).powf(exponent);
        cumulative.push(total);
    }
    let mut draw = || {
        let at = random.unit() * total;
        cumulative.partition_point(|&c| c < at).min(nodes as usize - 1) as DenseId
    };
    for _ in 0..nodes * degree {
        let (source, target) = (draw(), draw());
        edges.push(relation, source, target, 1.0)?;
    }
    Ok(edges)
}
