//! A version's id map in its region (DESIGN.md §5.6, §5.7): which segments
//! a load places, how each is laid out, what a load builds beside them,
//! and the id map viewed in place. A fixed-width id type's pairs are
//! re-laid out as they stream in and its second direction is built at
//! load; a text id map's three arrays are its segments as they are.

use std::mem::size_of;
use std::sync::Arc;

use warren_core::graph::{
    DenseId, FixedKeys, IdMap, IdOrder, IdPair, SegmentBuffer, SegmentKind, TargetWidth, TextKeys,
    Tombstones,
};

use super::fill::{by_dense, PairSink, NATIVE_LE};
use super::plan::{Placed, Planner};
use super::LoadError;
use crate::cache::region::{Array, Region, RegionError, RegionLayout};
use crate::segment::{SegmentStore, StoredIds, ID_BYTES, ID_MAP, ID_OFFSETS, ID_ORDER};
use crate::source::{IdDatum, InPlaceId};

/// Takes the bytes of a segment's payload as they stream in.
pub(crate) type PayloadSink<'a> = Box<dyn FnMut(&[u8]) -> Result<(), LoadError> + 'a>;

/// A key storage a loaded version's id map can have.
pub(crate) trait CachedIds: IdDatum + StoredIds {
    /// Places the id map's segments; returns the id space they imply.
    fn place(planner: &mut Planner<'_, impl SegmentStore>) -> Result<u64, LoadError>;

    /// The region's arrays for `placed`, as (array, element width,
    /// elements): an id-map segment's own and what a load builds beside it
    /// over an id space of `id_space` nodes.
    fn arrays(placed: &Placed, elements: usize, _id_space: usize) -> Vec<(Array, usize, usize)> {
        vec![(placed.array, placed.encoded_width, elements)]
    }

    /// Where `placed`'s payload goes as it streams in: into `into`, as it
    /// is, unless the id type lays it out otherwise.
    fn sink<'a>(_placed: &Placed, into: &'a mut [u8]) -> PayloadSink<'a> {
        copied(into)
    }

    /// Builds in `region` what a load builds beside the segments, once
    /// every array is copied.
    fn finish(_region: &mut [u8], _layout: &RegionLayout) -> Result<(), LoadError> {
        Ok(())
    }

    /// The id map `region` holds, with the tombstones' words `words`.
    fn view(region: &Arc<Region>, words: SegmentBuffer<u64>) -> Result<IdMap<Self>, LoadError>;

    /// The bytes a load adds to a version's stored segments over an id
    /// space of `id_space` nodes.
    fn built_at_load(id_space: u64) -> u64;
}

/// A sink copying the payload into `into`, from its start.
pub(super) fn copied(into: &mut [u8]) -> PayloadSink<'_> {
    let mut at = 0;
    Box::new(move |data: &[u8]| {
        let end = at + data.len();
        let found = into.len() as u64;
        into.get_mut(at..end)
            .ok_or(RegionError::Short { needed: end as u64, found })?
            .copy_from_slice(data);
        at = end;
        Ok(())
    })
}

impl<E: InPlaceId> CachedIds for FixedKeys<E> {
    fn place(planner: &mut Planner<'_, impl SegmentStore>) -> Result<u64, LoadError> {
        let pair = E::WIDTH + size_of::<DenseId>();
        Ok(planner.require(Array::IdPairs, ID_MAP, SegmentKind::IdMap, pair)? / pair as u64)
    }

    fn arrays(placed: &Placed, elements: usize, id_space: usize) -> Vec<(Array, usize, usize)> {
        match placed.array {
            Array::IdPairs => vec![
                (Array::IdPairs, size_of::<IdPair<E>>(), elements),
                (Array::ByDense, size_of::<E>(), id_space),
            ],
            _ => vec![(placed.array, placed.encoded_width, elements)],
        }
    }

    fn sink<'a>(placed: &Placed, into: &'a mut [u8]) -> PayloadSink<'a> {
        let relaid = placed.encoded_width != size_of::<IdPair<E>>() || !NATIVE_LE;
        match placed.array {
            Array::IdPairs if relaid => {
                let mut pairs = PairSink::<E>::new(into, placed.encoded_width);
                Box::new(move |data: &[u8]| pairs.push(data))
            }
            _ => copied(into),
        }
    }

    fn finish(region: &mut [u8], layout: &RegionLayout) -> Result<(), LoadError> {
        by_dense::<E>(region, layout.entry(Array::IdPairs), layout.entry(Array::ByDense))
    }

    fn view(region: &Arc<Region>, words: SegmentBuffer<u64>) -> Result<IdMap<Self>, LoadError> {
        let missing = RegionError::MissingArray;
        let pairs = region.view::<IdPair<E>>(Array::IdPairs)?.ok_or(missing("id map"))?;
        let by_dense = region.view::<E>(Array::ByDense)?.ok_or(missing("id map by dense id"))?;
        let tombstones = Tombstones::from_segment(pairs.len() as u64, words)?;
        Ok(IdMap::from_segments(pairs, by_dense, tombstones)?)
    }

    fn built_at_load(id_space: u64) -> u64 {
        id_space.saturating_mul(size_of::<E>() as u64)
    }
}

impl CachedIds for TextKeys {
    fn place(planner: &mut Planner<'_, impl SegmentStore>) -> Result<u64, LoadError> {
        let word = size_of::<u64>();
        let offsets =
            planner.require(Array::IdOffsets, ID_OFFSETS, SegmentKind::IdOffsets, word)?;
        let id_space = (offsets / word as u64).saturating_sub(1);
        planner.require(Array::IdBytes, ID_BYTES, SegmentKind::IdBytes, 1)?;
        let order = match TargetWidth::for_id_space(id_space) {
            TargetWidth::U32 => size_of::<u32>(),
            TargetWidth::U64 => size_of::<u64>(),
        };
        planner.require(Array::IdOrder, ID_ORDER, SegmentKind::IdOrder, order)?;
        Ok(id_space)
    }

    fn view(region: &Arc<Region>, words: SegmentBuffer<u64>) -> Result<IdMap<Self>, LoadError> {
        let missing = RegionError::MissingArray;
        let offsets = region.view::<u64>(Array::IdOffsets)?.ok_or(missing("id offsets"))?;
        let bytes = region.view::<u8>(Array::IdBytes)?.ok_or(missing("id bytes"))?;
        let id_space = offsets.len().saturating_sub(1) as u64;
        let order = match TargetWidth::for_id_space(id_space) {
            TargetWidth::U32 => IdOrder::U32(region.view(Array::IdOrder)?.ok_or(missing("order"))?),
            TargetWidth::U64 => IdOrder::U64(region.view(Array::IdOrder)?.ok_or(missing("order"))?),
        };
        let tombstones = Tombstones::from_segment(id_space, words)?;
        Ok(IdMap::from_text_segments(bytes, offsets, order, tombstones)?)
    }

    fn built_at_load(_: u64) -> u64 {
        0
    }
}
