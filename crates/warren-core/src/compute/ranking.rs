//! The `k` live nodes with the largest (or smallest) values of a vector:
//! what `vector_topk` returns (DESIGN.md §6.2).

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::mem::size_of;

use super::{AnyVector, ComputeError, ComputeMemory, Element};
use crate::graph::{DenseId, Tombstones};
use crate::interrupts::Interrupts;

/// Rows ranked between interrupt polls.
const POLL_ROWS: u64 = 1 << 16;

/// A ranked row: better rows order first.
#[derive(Debug, Clone, Copy)]
struct Ranked {
    value: f64,
    node: DenseId,
    descending: bool,
}

impl Ord for Ranked {
    /// Larger values first if descending, else smaller, in SQL order
    /// (`NaN` above every number); ties by ascending dense id.
    fn cmp(&self, other: &Self) -> Ordering {
        let by_value = match self.descending {
            true => other.value.sql_cmp(self.value),
            false => self.value.sql_cmp(other.value),
        };
        by_value.then(self.node.cmp(&other.node))
    }
}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Ranked {}

/// The `k` live rows of the width-1 vector `x` with the largest values
/// (`descending`) or the smallest, best first, as (dense id, value); ties
/// go to the smaller dense id. Fewer if there are fewer live rows.
pub fn top_k<I: Interrupts>(
    memory: &ComputeMemory,
    interrupts: &mut I,
    x: &AnyVector,
    tombstones: &Tombstones,
    k: u64,
    descending: bool,
) -> Result<Vec<(DenseId, f64)>, ComputeError> {
    if x.width() != 1 {
        return Err(ComputeError::WidthMismatch { expected: 1, found: x.width() });
    }
    interrupts.check()?;
    let kept = k.min(x.id_space()) as usize;
    let charge = memory.charge((kept as u64 + 1).saturating_mul(size_of::<Ranked>() as u64))?;
    let mut heap = BinaryHeap::new();
    heap.try_reserve_exact(kept + 1)
        .map_err(|_| ComputeError::OutOfMemory { bytes: charge.bytes() })?;
    let mut rank = |node: DenseId, value: f64| {
        heap.push(Ranked { value, node, descending });
        if heap.len() > kept {
            heap.pop();
        }
    };
    match x {
        AnyVector::F32(x) => each_live(interrupts, x.values(), tombstones, &mut rank)?,
        AnyVector::F64(x) => each_live(interrupts, x.values(), tombstones, &mut rank)?,
        AnyVector::I64(x) => each_live(interrupts, x.values(), tombstones, &mut rank)?,
        AnyVector::Bool(x) => each_live(interrupts, x.values(), tombstones, &mut rank)?,
    }
    Ok(heap.into_sorted_vec().into_iter().map(|ranked| (ranked.node, ranked.value)).collect())
}

/// Calls `f(node, value)` for each live row, polling `interrupts`.
fn each_live<E: Element, I: Interrupts>(
    interrupts: &mut I,
    values: &[E],
    tombstones: &Tombstones,
    f: &mut impl FnMut(DenseId, f64),
) -> Result<(), ComputeError> {
    for (node, value) in (0..).zip(values) {
        if node % POLL_ROWS == POLL_ROWS - 1 {
            interrupts.check()?;
        }
        if !tombstones.contains(node) {
            f(node, value.to_f64());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute::{AnyVector, ComputeMemory, Vector};
    use crate::interrupts::{Cancelled, Uninterrupted};

    fn memory() -> ComputeMemory {
        ComputeMemory::new(1 << 20)
    }

    fn vector(values: Vec<f64>) -> AnyVector {
        Vector::from_values(&memory(), values.len() as u64, 1, values).unwrap().into()
    }

    fn top(x: &AnyVector, tombstoned: &[u64], k: u64, descending: bool) -> Vec<(u64, f64)> {
        let mut tombstones = Tombstones::new(x.id_space()).unwrap();
        tombstoned.iter().for_each(|&id| tombstones.insert(id));
        top_k(&memory(), &mut Uninterrupted, x, &tombstones, k, descending).unwrap()
    }

    #[test]
    fn the_largest_values_come_first_with_ties_by_dense_id() {
        let x = vector(vec![3.0, 9.0, 1.0, 9.0, 5.0, 7.0]);
        assert_eq!(top(&x, &[], 3, true), [(1, 9.0), (3, 9.0), (5, 7.0)]);
        assert_eq!(top(&x, &[], 2, false), [(2, 1.0), (0, 3.0)]);
    }

    #[test]
    fn tombstoned_rows_are_never_ranked_and_k_may_exceed_the_live_nodes() {
        let x = vector(vec![3.0, 9.0, 1.0]);
        assert_eq!(top(&x, &[1], 10, true), [(0, 3.0), (2, 1.0)]);
        assert!(top(&x, &[], 0, true).is_empty());
    }

    #[test]
    fn nan_ranks_above_every_number() {
        let x = vector(vec![1.0, f64::NAN, f64::INFINITY]);
        let found = top(&x, &[], 2, true);
        assert!(found[0].0 == 1 && found[0].1.is_nan());
        assert_eq!(found[1], (2, f64::INFINITY));
        assert_eq!(top(&x, &[], 1, false), [(0, 1.0)]);
    }

    #[test]
    fn integers_and_booleans_rank_as_numbers() {
        let ints = Vector::from_values(&memory(), 3, 1, vec![4i64, -2, 8]).unwrap().into();
        assert_eq!(top(&ints, &[], 1, true), [(2, 8.0)]);
        let flags = Vector::from_values(&memory(), 2, 1, vec![false, true]).unwrap().into();
        assert_eq!(top(&flags, &[], 1, true), [(1, 1.0)]);
    }

    #[test]
    fn only_width_one_vectors_rank() {
        let matrix = Vector::filled(&memory(), 2, 3, 0.0f64).unwrap().into();
        let tombstones = Tombstones::new(2).unwrap();
        assert_eq!(
            top_k(&memory(), &mut Uninterrupted, &matrix, &tombstones, 1, true),
            Err(ComputeError::WidthMismatch { expected: 1, found: 3 })
        );
    }

    #[test]
    fn an_interrupt_stops_the_ranking() {
        struct Stop;
        impl Interrupts for Stop {
            fn check(&mut self) -> Result<(), Cancelled> {
                Err(Cancelled)
            }
        }
        let x = vector(vec![1.0]);
        let tombstones = Tombstones::new(1).unwrap();
        assert_eq!(
            top_k(&memory(), &mut Stop, &x, &tombstones, 1, true),
            Err(ComputeError::Cancelled)
        );
    }
}
