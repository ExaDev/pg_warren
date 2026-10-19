//! Placeholder benchmark target. Real micro-benchmarks arrive with M1 (DESIGN.md §13).

use criterion::{criterion_group, criterion_main, Criterion};

fn bench_version(c: &mut Criterion) {
    c.bench_function("version", |b| b.iter(|| std::hint::black_box(warren_core::version())));
}

criterion_group!(benches, bench_version);
criterion_main!(benches);
