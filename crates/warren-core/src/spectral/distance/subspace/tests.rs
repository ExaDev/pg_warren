use std::f64::consts::FRAC_PI_2;

use super::*;
use crate::compute::{ComputeError, ComputeMemory, Dtype, Vector, Workers};
use crate::interrupts::Uninterrupted;

fn memory() -> ComputeMemory {
    ComputeMemory::new(1 << 24)
}

fn f64s(values: &[f64]) -> AnyVector {
    Vector::from_values(&memory(), values.len() as u64, 1, values.to_vec()).unwrap().into()
}

fn f32s(values: &[f32]) -> AnyVector {
    Vector::from_values(&memory(), values.len() as u64, 1, values.to_vec()).unwrap().into()
}

fn distance_over(
    threads: usize,
    nodes: ComparedRows,
    before: &[&AnyVector],
    after: &[&AnyVector],
) -> Result<SubspaceDistance, SpectralError> {
    let workers = Workers::new(threads).unwrap();
    let memory = memory();
    let mut resources =
        Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted };
    subspace_distance(&mut resources, before, after, nodes)
}

fn distance_on(
    threads: usize,
    before: &[&AnyVector],
    after: &[&AnyVector],
) -> Result<SubspaceDistance, SpectralError> {
    distance_over(threads, ComparedRows::All, before, after)
}

fn distance(before: &[&AnyVector], after: &[&AnyVector]) -> SubspaceDistance {
    distance_on(2, before, after).unwrap()
}

#[test]
fn identical_sets_are_exactly_zero_apart() {
    let x = f64s(&[0.3, -0.1, 0.7, 0.2]);
    let y = f64s(&[0.5, 0.5, -0.2, 0.1]);
    let d = distance(&[&x, &y], &[&x, &y]);
    assert_eq!(d, SubspaceDistance { projection: 0.0, max_angle: 0.0, angles: vec![0.0, 0.0] });
}

#[test]
fn another_basis_of_the_same_span_is_zero_apart() {
    let (x, y) = ([1.0, 2.0, 0.0, -1.0], [0.0, 1.0, 1.0, 3.0]);
    let sum: Vec<f64> = x.iter().zip(&y).map(|(a, b)| 3.0 * a + b).collect();
    let difference: Vec<f64> = x.iter().zip(&y).map(|(a, b)| a - 0.5 * b).collect();
    let d = distance(&[&f64s(&x), &f64s(&y)], &[&f64s(&sum), &f64s(&difference)]);
    assert!(d.projection < 1e-7 && d.max_angle < 1e-7, "{d:?}");
}

#[test]
fn a_known_angle() {
    let theta: f64 = 0.3;
    let d = distance(&[&f64s(&[1.0, 0.0, 0.0])], &[&f64s(&[theta.cos(), theta.sin(), 0.0])]);
    assert!((d.max_angle - theta).abs() < 1e-12);
    assert!((d.projection - 2f64.sqrt() * theta.sin()).abs() < 1e-12);
    let orthogonal = distance(&[&f64s(&[1.0, 0.0])], &[&f64s(&[0.0, 2.0])]);
    assert_eq!(orthogonal.angles, [FRAC_PI_2]);
    assert!((orthogonal.projection - 2f64.sqrt()).abs() < 1e-15);
}

#[test]
fn spans_of_different_dimension() {
    let (e1, e2) = (f64s(&[1.0, 0.0, 0.0]), f64s(&[0.0, 1.0, 0.0]));
    let d = distance(&[&e1, &e2], &[&e1]);
    assert_eq!(d.angles, [0.0]);
    assert_eq!(d.max_angle, 0.0);
    assert_eq!(d.projection, 1.0);
    assert_eq!(distance(&[&e1], &[&e1, &e2]), d);
}

#[test]
fn rows_past_an_id_space_are_zero() {
    // A later version's vectors cover one more node.
    let before = f32s(&[1.0, 0.0, 0.0]);
    let after = f64s(&[0.0, 0.0, 0.0, 1.0]);
    assert_eq!(distance(&[&before], &[&after]).angles, [FRAC_PI_2]);
    let grown = f64s(&[1.0, 0.0, 0.0, 0.0]);
    assert_eq!(distance(&[&before], &[&grown]).projection, 0.0);
}

#[test]
fn the_distance_is_symmetric_and_the_same_for_every_thread_count() {
    let vectors: Vec<AnyVector> = (0..6)
        .map(|v| f64s(&(0..40).map(|i| ((i * 31 + v * 17) % 13) as f64 - 6.0).collect::<Vec<_>>()))
        .collect();
    let (x, y): (Vec<&AnyVector>, Vec<&AnyVector>) =
        (vectors[..3].iter().collect(), vectors[3..].iter().collect());
    let d = distance_on(1, &x, &y).unwrap();
    assert!(d.projection > 0.1);
    for threads in [2, 3, 4] {
        assert_eq!(distance_on(threads, &x, &y).unwrap(), d);
    }
    let reversed = distance(&y, &x);
    assert!((reversed.projection - d.projection).abs() < 1e-12);
    assert!((reversed.max_angle - d.max_angle).abs() < 1e-12);
}

#[test]
fn the_vectors_must_be_independent_float_columns() {
    let x = f64s(&[1.0, 2.0]);
    let twice = f64s(&[2.0, 4.0]);
    assert_eq!(distance_on(1, &[&x, &twice], &[&x]), Err(SpectralError::RankDeficient));
    assert_eq!(distance_on(1, &[], &[&x]), Err(SpectralError::EmptySubspace));
    let ints: AnyVector = Vector::from_values(&memory(), 2, 1, vec![1i64, 2]).unwrap().into();
    assert_eq!(
        distance_on(1, &[&ints], &[&x]),
        Err(ComputeError::DtypeMismatch { expected: Dtype::F64, found: Dtype::I64 }.into())
    );
    let matrix: AnyVector = Vector::from_values(&memory(), 1, 2, vec![1.0f64, 2.0]).unwrap().into();
    assert_eq!(
        distance_on(1, &[&matrix], &[&x]),
        Err(ComputeError::WidthMismatch { expected: 1, found: 2 }.into())
    );
}

#[test]
fn over_shared_nodes_rows_only_one_set_has_are_left_out() {
    // Node 3 is only in the later version; node 0 is only in the earlier.
    let before = [f64s(&[1.0, 2.0, 1.0, 0.0]), f64s(&[3.0, 0.0, 1.0, 0.0])];
    let after = [f64s(&[0.0, 2.0, 1.0, 5.0]), f64s(&[0.0, 0.0, 1.0, -1.0])];
    let (x, y): (Vec<&AnyVector>, Vec<&AnyVector>) =
        (before.iter().collect(), after.iter().collect());
    let all = distance_over(1, ComparedRows::All, &x, &y).unwrap();
    assert!(all.max_angle > 0.5);
    // On nodes 1 and 2 both spans are the whole plane.
    assert_eq!(distance_over(1, ComparedRows::Shared, &x, &y).unwrap().projection, 0.0);
}
