//! Projection and lifting (DESIGN.md §6.6): a vector of the parent version
//! aggregated over each community's members into a vector of the quotient
//! (`Pᵀx`, with `sum`, `mean`, `max`, `min` or `count`), and a vector of
//! the quotient broadcast back to every member (`P·y`). Matrices (width
//! d) are aggregated or broadcast row by row, column by column.

use std::fmt::{self, Display};
use std::str::FromStr;

use crate::compute::{AnyVector, ComputeError, ComputeMemory, Element, Vector};
use crate::graph::{GraphError, Membership};

use super::CommunityError;

/// How [`project`] aggregates a community's members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregate {
    /// The sum.
    Sum,
    /// The mean.
    Mean,
    /// The largest, in SQL order (`NaN` above every number).
    Max,
    /// The smallest, in SQL order.
    Min,
    /// The number of members.
    Count,
}

impl FromStr for Aggregate {
    type Err = CommunityError;

    fn from_str(name: &str) -> Result<Self, CommunityError> {
        match name {
            "sum" => Ok(Self::Sum),
            "mean" => Ok(Self::Mean),
            "max" => Ok(Self::Max),
            "min" => Ok(Self::Min),
            "count" => Ok(Self::Count),
            _ => Err(CommunityError::UnknownAggregate(name.to_owned())),
        }
    }
}

impl Display for Aggregate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Sum => "sum",
            Self::Mean => "mean",
            Self::Max => "max",
            Self::Min => "min",
            Self::Count => "count",
        })
    }
}

/// Each community's aggregate of `x`'s rows over its members: a vector of
/// the quotient with `x`'s width, `f64` (`i64` for [`Aggregate::Count`]),
/// whatever `x`'s element type (read as `double precision`, `bool` as 0
/// or 1). Members are taken in ascending dense id, so sums do not depend
/// on anything but the membership. A community without members (a
/// tombstoned node of the quotient) holds 0.
pub fn project(
    memory: &ComputeMemory,
    membership: &Membership,
    x: &AnyVector,
    aggregate: Aggregate,
) -> Result<AnyVector, CommunityError> {
    require_id_space(x, membership.id_space())?;
    let (width, communities) = (x.width(), membership.quotient_id_space());
    let rows = usize::try_from(communities).map_err(|_| GraphError::IdSpaceTooLarge)?;
    let (mut members, _members_charge) = memory.filled(rows, 0u64)?;
    for community in (0..membership.id_space()).filter_map(|node| membership.community(node)) {
        members[community as usize] += 1;
    }
    if aggregate == Aggregate::Count {
        let mut counts = Vector::filled(memory, communities, width, 0i64)?;
        for (row, &count) in counts.values_mut()?.chunks_exact_mut(width).zip(members.iter()) {
            row.fill(count as i64);
        }
        return Ok(counts.into());
    }
    let mut y = Vector::filled(memory, communities, width, 0.0f64)?;
    let (mut started, _started_charge) = memory.filled(rows, false)?;
    let into = Folded { y: y.values_mut()?, started: &mut started, width, aggregate };
    match x {
        AnyVector::F32(x) => into.fold(x.values(), membership),
        AnyVector::F64(x) => into.fold(x.values(), membership),
        AnyVector::I64(x) => into.fold(x.values(), membership),
        AnyVector::Bool(x) => into.fold(x.values(), membership),
    }
    if aggregate == Aggregate::Mean {
        for (row, &count) in y.values_mut()?.chunks_exact_mut(width).zip(members.iter()) {
            row.iter_mut().filter(|_| count > 0).for_each(|value| *value /= count as f64);
        }
    }
    Ok(y.into())
}

/// A projection's rows as they are folded.
struct Folded<'y> {
    /// The communities' rows, 0 on entry.
    y: &'y mut [f64],
    /// Whether each community has had a member folded in.
    started: &'y mut [bool],
    width: usize,
    aggregate: Aggregate,
}

impl Folded<'_> {
    /// Folds each member's row of `x` into its community's row, the first
    /// member's taken as it is: sums for `sum` and `mean`, else the
    /// largest or smallest in SQL order, the first on a tie.
    fn fold<E: Element>(self, x: &[E], membership: &Membership) {
        let width = self.width;
        for (node, row) in (0..).zip(x.chunks_exact(width)) {
            let Some(community) = membership.community(node) else { continue };
            let community = community as usize;
            let first = !std::mem::replace(&mut self.started[community], true);
            let into = &mut self.y[community * width..(community + 1) * width];
            for (into, &value) in into.iter_mut().zip(row) {
                let value = value.to_f64();
                *into = match (first, self.aggregate) {
                    (true, _) => value,
                    (false, Aggregate::Max) if value.sql_cmp(*into).is_gt() => value,
                    (false, Aggregate::Min) if value.sql_cmp(*into).is_lt() => value,
                    (false, Aggregate::Max | Aggregate::Min) => *into,
                    (false, _) => *into + value,
                };
            }
        }
    }
}

/// Each parent node's row of `y`, its community's: a vector of the parent
/// version with `y`'s width and element type. A node in no community (a
/// tombstoned one) holds 0.
pub fn lift(
    memory: &ComputeMemory,
    membership: &Membership,
    y: &AnyVector,
) -> Result<AnyVector, CommunityError> {
    require_id_space(y, membership.quotient_id_space())?;
    Ok(match y {
        AnyVector::F32(y) => broadcast(memory, membership, y)?.into(),
        AnyVector::F64(y) => broadcast(memory, membership, y)?.into(),
        AnyVector::I64(y) => broadcast(memory, membership, y)?.into(),
        AnyVector::Bool(y) => broadcast(memory, membership, y)?.into(),
    })
}

/// [`lift`] of `y`.
fn broadcast<E: Element>(
    memory: &ComputeMemory,
    membership: &Membership,
    y: &Vector<E>,
) -> Result<Vector<E>, CommunityError> {
    let width = y.width();
    let mut x = Vector::filled(memory, membership.id_space(), width, E::default())?;
    for (node, row) in (0..).zip(x.values_mut()?.chunks_exact_mut(width)) {
        if let Some(community) = membership.community(node) {
            row.copy_from_slice(y.row(community));
        }
    }
    Ok(x)
}

/// Checks that `vector` has `id_space` rows.
fn require_id_space(vector: &AnyVector, id_space: u64) -> Result<(), CommunityError> {
    match vector.id_space() == id_space {
        true => Ok(()),
        false => {
            Err(ComputeError::IdSpaceMismatch { expected: id_space, found: vector.id_space() }
                .into())
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::graph::DenseId;

    fn memory() -> ComputeMemory {
        ComputeMemory::new(u64::MAX)
    }

    fn vector<E: Element>(width: usize, values: Vec<E>) -> Vector<E> {
        let id_space = (values.len() / width) as u64;
        Vector::from_values(&memory(), id_space, width, values).unwrap()
    }

    /// Nodes 0 and 3 in community 1, node 2 in community 0, node 1 in
    /// none; community 2 has no member.
    fn membership() -> Membership {
        Membership::new(&[Some(1), None, Some(0), Some(1)], 3).unwrap()
    }

    fn f64s(found: AnyVector) -> Vec<f64> {
        match found {
            AnyVector::F64(vector) => vector.values().to_vec(),
            other => panic!("not f64: {other:?}"),
        }
    }

    #[test]
    fn each_aggregate_of_a_vector() {
        let x = AnyVector::F64(vector(1, vec![1.0, 100.0, 5.0, 3.0]));
        let project = |aggregate| project(&memory(), &membership(), &x, aggregate).unwrap();
        assert_eq!(f64s(project(Aggregate::Sum)), [5.0, 4.0, 0.0]);
        assert_eq!(f64s(project(Aggregate::Mean)), [5.0, 2.0, 0.0]);
        assert_eq!(f64s(project(Aggregate::Max)), [5.0, 3.0, 0.0]);
        assert_eq!(f64s(project(Aggregate::Min)), [5.0, 1.0, 0.0]);
        match project(Aggregate::Count) {
            AnyVector::I64(counts) => assert_eq!(counts.values(), [1, 2, 0]),
            other => panic!("not i64: {other:?}"),
        }
    }

    #[test]
    fn a_matrix_is_aggregated_column_by_column() {
        let x = AnyVector::F32(vector(2, vec![1.0f32, 10.0, 0.0, 0.0, 2.0, 20.0, 3.0, 30.0]));
        let mean = project(&memory(), &membership(), &x, Aggregate::Mean).unwrap();
        assert_eq!(mean.width(), 2);
        assert_eq!(f64s(mean), [2.0, 20.0, 2.0, 20.0, 0.0, 0.0]);
    }

    #[test]
    fn integers_and_booleans_are_read_as_numbers() {
        let x = AnyVector::I64(vector(1, vec![1, 9, 2, 4]));
        assert_eq!(
            f64s(project(&memory(), &membership(), &x, Aggregate::Sum).unwrap()),
            [2.0, 5.0, 0.0]
        );
        let x = AnyVector::Bool(vector(1, vec![true, true, false, true]));
        assert_eq!(
            f64s(project(&memory(), &membership(), &x, Aggregate::Sum).unwrap()),
            [0.0, 2.0, 0.0]
        );
    }

    #[test]
    fn nan_is_the_largest_value() {
        let x = AnyVector::F64(vector(1, vec![f64::NAN, 0.0, 1.0, 2.0]));
        let max = f64s(project(&memory(), &membership(), &x, Aggregate::Max).unwrap());
        assert!(max[1].is_nan());
        let min = f64s(project(&memory(), &membership(), &x, Aggregate::Min).unwrap());
        assert_eq!(min[1], 2.0);
    }

    #[test]
    fn lifting_copies_each_communitys_row_to_its_members() {
        let y = AnyVector::I64(vector(2, vec![1, 2, 3, 4, 5, 6]));
        match lift(&memory(), &membership(), &y).unwrap() {
            AnyVector::I64(x) => {
                assert_eq!(x.width(), 2);
                assert_eq!(x.values(), [3, 4, 0, 0, 1, 2, 3, 4]);
            }
            other => panic!("not i64: {other:?}"),
        }
    }

    #[test]
    fn vectors_must_cover_their_versions() {
        let short = AnyVector::F64(vector(1, vec![1.0, 2.0]));
        assert_eq!(
            project(&memory(), &membership(), &short, Aggregate::Sum),
            Err(CommunityError::Compute(ComputeError::IdSpaceMismatch { expected: 4, found: 2 }))
        );
        assert_eq!(
            lift(&memory(), &membership(), &short),
            Err(CommunityError::Compute(ComputeError::IdSpaceMismatch { expected: 3, found: 2 }))
        );
    }

    #[test]
    fn the_results_are_charged_to_the_budget() {
        let x = AnyVector::F64(vector(1, vec![1.0; 4]));
        let small = ComputeMemory::new(8);
        assert!(matches!(
            project(&small, &membership(), &x, Aggregate::Sum),
            Err(CommunityError::Compute(ComputeError::MemoryLimit { .. }))
        ));
    }

    #[test]
    fn aggregates_are_named_as_in_sql() {
        let names = ["sum", "mean", "max", "min", "count"];
        let parsed: Vec<Aggregate> = names.iter().map(|name| name.parse().unwrap()).collect();
        assert_eq!(parsed.iter().map(ToString::to_string).collect::<Vec<_>>(), names);
        assert_eq!("avg".parse::<Aggregate>(), Err(CommunityError::UnknownAggregate("avg".into())));
    }

    proptest! {
        #[test]
        fn projection_and_lifting_agree_with_their_definitions(
            (communities, values) in (1usize..30).prop_flat_map(|n| (
                prop::collection::vec(prop::option::weighted(0.8, 0u64..5), n),
                prop::collection::vec(-100i32..100, n * 2),
            )),
        ) {
            let p = Membership::new(&communities, 5).unwrap();
            let values: Vec<f64> = values.into_iter().map(f64::from).collect();
            let x = AnyVector::F64(vector(2, values.clone()));
            let members = |c: DenseId| -> Vec<usize> {
                (0..communities.len()).filter(|&u| communities[u] == Some(c)).collect()
            };
            let sum = f64s(project(&memory(), &p, &x, Aggregate::Sum).unwrap());
            let mean = f64s(project(&memory(), &p, &x, Aggregate::Mean).unwrap());
            let max = f64s(project(&memory(), &p, &x, Aggregate::Max).unwrap());
            for c in 0..5u64 {
                for j in 0..2 {
                    let column: Vec<f64> = members(c).iter().map(|&u| values[u * 2 + j]).collect();
                    let at = c as usize * 2 + j;
                    prop_assert_eq!(sum[at], column.iter().sum::<f64>());
                    let expected_mean = match column.len() {
                        0 => 0.0,
                        n => column.iter().sum::<f64>() / n as f64,
                    };
                    prop_assert_eq!(mean[at], expected_mean);
                    prop_assert_eq!(max[at], column.iter().copied().fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v)))).unwrap_or(0.0));
                }
            }
            let lifted = f64s(lift(&memory(), &p, &AnyVector::F64(vector(2, sum.clone()))).unwrap());
            for (u, community) in communities.iter().enumerate() {
                for j in 0..2 {
                    let expected = community.map_or(0.0, |c| sum[c as usize * 2 + j]);
                    prop_assert_eq!(lifted[u * 2 + j], expected);
                }
            }
        }
    }
}
