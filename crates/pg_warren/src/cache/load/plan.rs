//! Where each segment of a version goes in its region, sized exactly from
//! the stored segments' lengths before anything but the small relation
//! dictionary is read (DESIGN.md §5.6).

use std::mem::size_of;

use warren_core::graph::{
    GraphError, RelationDictionary, SegmentKind, TargetWidth, SEGMENT_HEADER_LEN,
};

use super::{CachedIds, LoadError};
use crate::cache::region::{Array, RegionLayout};
use crate::catalog::VersionId;
use crate::segment::{
    csr_segment, read_segment, CsrPart, CsrScope, Direction, SegmentError, SegmentStore,
    RELATION_DICTIONARY, TOMBSTONES,
};

/// A segment whose payload is copied into an array of the region.
pub(crate) struct Placed {
    /// The array it fills.
    pub(super) array: Array,
    /// The segment's name.
    pub(super) segment: String,
    /// What the segment holds.
    pub(super) kind: SegmentKind,
    /// The width of its encoded elements.
    pub(super) encoded_width: usize,
    /// The bytes of its payload.
    pub(super) payload: u64,
}

/// What a version's region holds and where.
pub(in crate::cache) struct Plan {
    /// The region's layout.
    pub(super) layout: RegionLayout,
    /// The segments copied into it, in order.
    pub(super) placed: Vec<Placed>,
    /// The relation dictionary's segment, already read.
    pub(super) relations: Vec<u8>,
    /// The id space.
    pub(super) id_space: u64,
    /// The database encoding, which an `id_bytes` header must record.
    pub(super) encoding: u8,
}

impl Plan {
    /// The bytes of the region it lays out.
    pub(in crate::cache) fn len(&self) -> usize {
        self.layout.len()
    }
}

/// The plan of `version`'s region: the id map's arrays (a fixed type's
/// pairs and second direction, or a text id map's three), the tombstones
/// and relation dictionary, then each
/// relation's CSRs and the merged CSR's, if stored (it must be where the
/// graph stores it, `store_merged`, or has no relations), each forward
/// and, if stored, reverse.
pub(in crate::cache) fn plan<E: CachedIds>(
    segments: &mut impl SegmentStore,
    version: VersionId,
    store_merged: bool,
) -> Result<Plan, LoadError> {
    let relations = read_segment(segments, version, RELATION_DICTIONARY)?;
    let relation_count = RelationDictionary::decode(&relations)?.len();
    let encoding = segments.encoding();
    let mut planner = Planner { segments, version, placed: Vec::new() };
    let id_space = E::place(&mut planner)?;
    planner.require(Array::Tombstones, TOMBSTONES, SegmentKind::Tombstones, size_of::<u64>())?;
    let target = match TargetWidth::for_id_space(id_space) {
        TargetWidth::U32 => size_of::<u32>(),
        TargetWidth::U64 => size_of::<u64>(),
    };
    for index in 0..relation_count {
        planner.relation(CsrScope::Relation(index), target)?;
    }
    // With several relations the merged CSR is stored if the graph stores
    // it (`store_merged`, fixed at `create_graph` and kept by `compact`, so
    // it is every version's layout): then a missing merged segment is an
    // error, not a version that merges on the fly.
    match (relation_count, store_merged) {
        (1, _) | (2.., false) => {}
        (0, _) | (2.., true) => planner.relation(CsrScope::Merged, target)?,
    }
    let Planner { placed, .. } = planner;
    let id_space_elements = usize::try_from(id_space).map_err(|_| GraphError::IdSpaceTooLarge)?;
    let mut arrays = Vec::with_capacity(placed.len() + 2);
    for placed in &placed {
        let elements = elements(placed.payload, placed.encoded_width);
        arrays.extend(E::arrays(placed, elements, id_space_elements));
    }
    arrays.push((Array::RelationDictionary, 1, relations.len()));
    let layout = RegionLayout::plan(version, &arrays)?;
    Ok(Plan { layout, placed, relations, id_space, encoding })
}

/// The whole elements of `width` bytes in `payload` bytes: a payload that
/// is not whole is refused by its header check before it is copied.
fn elements(payload: u64, width: usize) -> usize {
    usize::try_from(payload / width.max(1) as u64).unwrap_or(usize::MAX)
}

/// Finds the segments of a version and their sizes.
pub(crate) struct Planner<'s, S> {
    segments: &'s mut S,
    version: VersionId,
    placed: Vec<Placed>,
}

impl<S: SegmentStore> Planner<'_, S> {
    /// Places segment `name`, which must exist; returns its payload bytes.
    pub(super) fn require(
        &mut self,
        array: Array,
        name: &str,
        kind: SegmentKind,
        encoded_width: usize,
    ) -> Result<u64, LoadError> {
        self.place(array, name, kind, encoded_width)?.ok_or_else(|| {
            SegmentError::SegmentMissing { version: self.version, segment: name.to_owned() }.into()
        })
    }

    /// Places segment `name` if it exists; returns its payload bytes.
    fn place(
        &mut self,
        array: Array,
        name: &str,
        kind: SegmentKind,
        encoded_width: usize,
    ) -> Result<Option<u64>, LoadError> {
        let Some(bytes) = self.segments.segment_bytes(self.version, name)? else {
            return Ok(None);
        };
        let payload = bytes.checked_sub(SEGMENT_HEADER_LEN as u64);
        let payload = payload.ok_or(GraphError::SegmentTruncated)?;
        let segment = name.to_owned();
        self.placed.push(Placed { array, segment, kind, encoded_width, payload });
        Ok(Some(payload))
    }

    /// Places the CSRs of `scope`: forward, which must exist, and reverse,
    /// if stored.
    fn relation(&mut self, scope: CsrScope, target: usize) -> Result<(), LoadError> {
        for direction in [Direction::Forward, Direction::Reverse] {
            let array = |part| Array::Csr(direction, part, scope);
            let name = |part| csr_segment(direction, part, scope);
            let (offsets, word) = (array(CsrPart::Offsets), size_of::<u64>());
            let segment = name(CsrPart::Offsets);
            if direction == Direction::Forward {
                self.require(offsets, &segment, SegmentKind::Offsets, word)?;
            } else if self.place(offsets, &segment, SegmentKind::Offsets, word)?.is_none() {
                continue;
            }
            let targets = (array(CsrPart::Targets), name(CsrPart::Targets));
            self.require(targets.0, &targets.1, SegmentKind::Targets, target)?;
            let weights = (array(CsrPart::Weights), name(CsrPart::Weights));
            self.place(weights.0, &weights.1, SegmentKind::Weights, word)?;
        }
        Ok(())
    }
}
