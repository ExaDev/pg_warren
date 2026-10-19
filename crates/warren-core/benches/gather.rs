//! `gather` throughput over a random graph of about one million edges,
//! every node sending one message: `sum` of `f64` messages and `collect`
//! of `f64` and of 8-byte messages, pulled over the reverse CSR and pushed
//! without one, on one thread and on every available core (DESIGN.md §6.4,
//! §6.9, §12).

use std::thread::available_parallelism;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use warren_core::compute::{
    gather, Bytes, Combine, Combiner, ComputeMemory, Gather, Message, Messages, Workers,
};
use warren_core::graph::GraphCsr;
use warren_core::interrupts::Uninterrupted;

mod random_graph;

use random_graph::{random_edges, EDGES, ID_SPACE};

/// One message per node, `message(node)`.
fn messages<M: Message>(
    memory: &ComputeMemory,
    message: impl Fn(u64) -> M::View<'static>,
) -> Option<Messages<M>> {
    let mut messages = Messages::new(memory).ok()?;
    for node in 0..ID_SPACE {
        messages.push(node, message(node)).ok()?;
    }
    Some(messages)
}

fn bench_gather(c: &mut Criterion) {
    let memory = ComputeMemory::new(u64::MAX);
    let built =
        |reverse| random_edges(false).and_then(|e| GraphCsr::build(e, 1, ID_SPACE, reverse));
    let (Ok(with_reverse), Ok(forward_only)) = (built(true), built(false)) else {
        eprintln!("skipping gather benchmarks: graph build failed");
        return;
    };
    static BYTES: [[u8; 8]; 256] = {
        let mut bytes = [[0; 8]; 256];
        let mut i = 0;
        while i < 256 {
            bytes[i] = [i as u8; 8];
            i += 1;
        }
        bytes
    };
    let (Some(numbers), Some(strings)) = (
        messages::<f64>(&memory, |node| node as f64),
        messages::<Bytes>(&memory, |node| &BYTES[node as usize % 256]),
    ) else {
        eprintln!("skipping gather benchmarks: message allocation failed");
        return;
    };
    let cores = available_parallelism().map_or(1, |n| n.get());
    let mut group = c.benchmark_group("gather");
    group.throughput(Throughput::Elements(EDGES));
    group.sample_size(10);
    for threads in [1, cores] {
        let Ok(workers) = Workers::new(threads) else { continue };
        for (form, graph) in [("pull_reverse", &with_reverse), ("push_forward", &forward_only)] {
            let name = |case: &str| format!("{case}/{form}/threads_{threads}");
            for (case, combiner) in [("sum_f64", Combiner::Sum), ("collect_f64", Combiner::Collect)]
            {
                let spec = Gather::new(combiner);
                group.bench_function(name(case), |b| {
                    b.iter(|| run_gather(&workers, &memory, graph, &spec, &numbers))
                });
            }
            let spec = Gather::new(Combiner::Collect);
            group.bench_function(name("collect_bytes"), |b| {
                b.iter(|| run_gather(&workers, &memory, graph, &spec, &strings))
            });
        }
    }
    group.finish();
}

fn run_gather<M: Combine>(
    workers: &Workers,
    memory: &ComputeMemory,
    graph: &GraphCsr,
    spec: &Gather<'_>,
    messages: &Messages<M>,
) -> usize {
    gather(workers, memory, &mut Uninterrupted, graph, spec, messages).map_or(0, |g| g.len())
}

criterion_group!(benches, bench_gather);
criterion_main!(benches);
