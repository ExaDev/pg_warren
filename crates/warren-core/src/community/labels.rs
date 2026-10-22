//! Labels: each live node's community as an integer, read from a label
//! vector (DESIGN.md §6.6). The quotient's node ids are the distinct
//! labels, so a label must be an integral number.

use crate::compute::{AnyVector, Element};
use crate::graph::build_memory::bytes_of;
use crate::graph::{
    memory, BuildBuffer, BuildCharge, BuildMemory, DenseId, IdMap, Membership, Tombstones,
    NO_COMMUNITY,
};

use super::CommunityError;

/// Each node's label, by dense id; tombstoned nodes have none.
#[derive(Debug, PartialEq, Eq)]
pub struct Labels {
    labels: BuildBuffer<i64>,
    tombstones: Tombstones,
}

impl Labels {
    /// The labels in `vector` (one per node) of the live nodes of a version
    /// with `tombstones`, charged to `memory`. Each must be an integral
    /// number within `bigint`'s range: `f64` and `f32` labels are refused
    /// if NaN, infinite or fractional; `bool` vectors are refused.
    pub fn read(
        vector: &AnyVector,
        tombstones: &Tombstones,
        memory: &BuildMemory,
    ) -> Result<Self, CommunityError> {
        if vector.width() != 1 {
            return Err(CommunityError::LabelWidth(vector.width()));
        }
        if vector.id_space() != tombstones.id_space() {
            return Err(CommunityError::LabelLength {
                expected: tombstones.id_space(),
                found: vector.id_space(),
            });
        }
        let labels = match vector {
            AnyVector::I64(vector) => integral(vector.values(), tombstones, memory, Some)?,
            AnyVector::F64(vector) => integral(vector.values(), tombstones, memory, exact)?,
            AnyVector::F32(vector) => {
                integral(vector.values(), tombstones, memory, |v| exact(f64::from(v)))?
            }
            AnyVector::Bool(_) => return Err(CommunityError::LabelDtype(vector.dtype())),
        };
        Ok(Self { labels, tombstones: tombstones.try_clone()? })
    }

    /// The distinct labels of live nodes, ascending, charged to `memory`:
    /// the quotient's node ids.
    pub fn distinct(&self, memory: &BuildMemory) -> Result<BuildBuffer<i64>, CommunityError> {
        let live = self.live().count();
        let mut distinct = BuildBuffer::with_capacity(memory, live)?;
        distinct.extend(self.live().map(|(_, label)| label));
        distinct.sort_unstable();
        distinct.dedup();
        distinct.shrink_to_fit();
        Ok(distinct)
    }

    /// The membership of each node in the community of its label in the
    /// quotient's id map, which holds every label as a live node, with
    /// its charge to `memory`.
    pub fn membership(
        &self,
        quotient: &IdMap<i64>,
        memory: &BuildMemory,
    ) -> Result<(Membership, BuildCharge), CommunityError> {
        let nodes = self.labels.len();
        let charge = memory.charge(bytes_of::<u64>(nodes))?;
        let mut communities = memory::with_capacity(nodes)?;
        for node in 0..nodes as u64 {
            let community = match self.label(node) {
                None => NO_COMMUNITY,
                Some(label) => {
                    quotient.dense_id(&label).ok_or(CommunityError::UnknownLabel { node, label })?
                }
            };
            communities.push(community);
        }
        Ok((Membership::from_stored(communities, quotient.id_space())?, charge))
    }

    /// Every node's label by dense id, 0 for a tombstoned node.
    pub fn values(&self) -> &[i64] {
        &self.labels
    }

    /// The label of `node`, if it is live.
    pub fn label(&self, node: DenseId) -> Option<i64> {
        let label = *self.labels.get(usize::try_from(node).ok()?)?;
        (!self.tombstones.contains(node)).then_some(label)
    }

    /// The live nodes and their labels, by dense id.
    fn live(&self) -> impl Iterator<Item = (DenseId, i64)> + '_ {
        (0..self.labels.len() as u64).filter_map(|node| Some((node, self.label(node)?)))
    }
}

/// `values` as labels, each live node's converted by `integer` (0 for
/// tombstoned nodes), charged to `memory`.
fn integral<E: Element>(
    values: &[E],
    tombstones: &Tombstones,
    memory: &BuildMemory,
    integer: impl Fn(E) -> Option<i64>,
) -> Result<BuildBuffer<i64>, CommunityError> {
    let mut labels = BuildBuffer::with_capacity(memory, values.len())?;
    for (node, &value) in (0..).zip(values) {
        let label = match tombstones.contains(node) {
            true => 0,
            false => {
                integer(value).ok_or(CommunityError::NotIntegral { node, value: value.to_f64() })?
            }
        };
        labels.push(label)?;
    }
    Ok(labels)
}

/// `value` as an `i64` if it is one exactly: finite, integral and within
/// `[-2⁶³, 2⁶³)`. `-0` is 0.
fn exact(value: f64) -> Option<i64> {
    const LIMIT: f64 = 9_223_372_036_854_775_808.0; // 2⁶³
    let integral = value.is_finite() && value.fract() == 0.0;
    (integral && (-LIMIT..LIMIT).contains(&value)).then_some(value as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute::{ComputeMemory, Dtype, Vector};

    fn vector<E: Element>(values: Vec<E>) -> Vector<E> {
        let memory = ComputeMemory::new(u64::MAX);
        Vector::from_values(&memory, values.len() as u64, 1, values).unwrap()
    }

    fn live(id_space: u64, tombstoned: &[u64]) -> Tombstones {
        let mut tombstones = Tombstones::new(id_space).unwrap();
        tombstoned.iter().for_each(|&node| tombstones.insert(node).unwrap());
        tombstones
    }

    fn read(vector: AnyVector, tombstones: &Tombstones) -> Result<Labels, CommunityError> {
        Labels::read(&vector, tombstones, &BuildMemory::unlimited())
    }

    #[test]
    fn integral_labels_of_every_numeric_dtype_are_read() {
        let tombstones = live(3, &[]);
        let labels = read(AnyVector::F64(vector(vec![2.0, -0.0, 7.0])), &tombstones).unwrap();
        assert_eq!([0, 1, 2].map(|n| labels.label(n)), [Some(2), Some(0), Some(7)]);
        let labels = read(AnyVector::F32(vector(vec![1.0f32, 3.0, 1.0])), &tombstones).unwrap();
        assert_eq!([0, 1, 2].map(|n| labels.label(n)), [Some(1), Some(3), Some(1)]);
        let labels = read(AnyVector::I64(vector(vec![i64::MIN, 5, i64::MAX])), &tombstones);
        assert_eq!(labels.unwrap().label(2), Some(i64::MAX));
    }

    #[test]
    fn tombstoned_nodes_have_no_label_whatever_their_value() {
        let tombstones = live(3, &[1]);
        let labels = read(AnyVector::F64(vector(vec![4.0, f64::NAN, 4.0])), &tombstones).unwrap();
        assert_eq!([0, 1, 2].map(|n| labels.label(n)), [Some(4), None, Some(4)]);
        assert_eq!(labels.values(), [4, 0, 4]);
    }

    #[test]
    fn labels_that_are_not_integers_are_refused() {
        let tombstones = live(2, &[]);
        for bad in [0.5, f64::NAN, f64::INFINITY, -f64::INFINITY, 9.3e18, -9.3e18] {
            let refused = read(AnyVector::F64(vector(vec![1.0, bad])), &tombstones);
            assert!(
                matches!(refused, Err(CommunityError::NotIntegral { node: 1, value })
                                  if value.to_bits() == bad.to_bits()),
                "{bad}: {refused:?}"
            );
        }
        assert_eq!(
            read(AnyVector::Bool(vector(vec![true, false])), &tombstones),
            Err(CommunityError::LabelDtype(Dtype::Bool))
        );
    }

    #[test]
    fn a_label_vector_has_one_label_per_node_of_the_version() {
        let memory = ComputeMemory::new(u64::MAX);
        let wide = Vector::from_values(&memory, 1, 2, vec![1i64, 2]).unwrap();
        assert_eq!(read(AnyVector::I64(wide), &live(1, &[])), Err(CommunityError::LabelWidth(2)));
        assert_eq!(
            read(AnyVector::I64(vector(vec![1, 2])), &live(3, &[])),
            Err(CommunityError::LabelLength { expected: 3, found: 2 })
        );
    }

    #[test]
    fn the_distinct_labels_of_live_nodes_are_ascending() {
        let tombstones = live(5, &[3]);
        let labels = read(AnyVector::I64(vector(vec![9, -2, 9, 100, 4])), &tombstones).unwrap();
        let distinct = labels.distinct(&BuildMemory::unlimited()).unwrap();
        assert_eq!(&distinct[..], &[-2, 4, 9]);
    }

    #[test]
    fn each_live_node_belongs_to_its_labels_community() {
        let tombstones = live(4, &[2]);
        let labels = read(AnyVector::I64(vector(vec![30, 10, 0, 30])), &tombstones).unwrap();
        let quotient = IdMap::new().rebuild(vec![10, 20, 30]).unwrap();
        let membership = labels.membership(&quotient, &BuildMemory::unlimited()).unwrap().0;
        assert_eq!(membership.quotient_id_space(), 3);
        assert_eq!(
            [0, 1, 2, 3].map(|n| membership.community(n)),
            [Some(2), Some(0), None, Some(2)]
        );
        let missing = IdMap::new().rebuild(vec![10]).unwrap();
        assert_eq!(
            labels.membership(&missing, &BuildMemory::unlimited()).map(|(m, _)| m),
            Err(CommunityError::UnknownLabel { node: 0, label: 30 })
        );
    }
}
