//! A region's layout (DESIGN.md §5.6, §14 items 29 and 38): a header, a
//! table of contents, then every array of the loaded version (or the one
//! array of a persisted vector) at a 64-byte-aligned offset, the region
//! ending with the last array. The header and table
//! are little-endian; the arrays are in native layout.
//!
//! | Bytes | Header field |
//! |---|---|
//! | 0..4 | magic `WRRG` |
//! | 4..8 | format (`u32`) |
//! | 8..16 | the version id, or a vector's region's vector id (`i64`) |
//! | 16..24 | the region's length in bytes (`u64`) |
//! | 24..32 | the number of entries in the table (`u64`) |
//! | 32..64 | reserved, 0 |
//!
//! Each 32-byte entry of the table names an array (its kind; for a CSR's
//! array, its direction, part and scope) and gives its element width,
//! offset and element count.

use std::collections::HashMap;
use std::ops::Range;

use super::RegionError;
use crate::catalog::VersionId;
use crate::segment::{CsrPart, CsrScope, Direction};

/// The alignment of a region and of every array in it.
pub(crate) const ALIGNMENT: usize = 64;
/// The format, changed whenever the layout does.
pub(crate) const FORMAT: u32 = 2;
const MAGIC: [u8; 4] = *b"WRRG";
const HEADER_LEN: usize = 64;
const ENTRY_LEN: usize = 32;
/// The scope a merged CSR's entry records.
const MERGED: u64 = u64::MAX;

/// An array of a loaded version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Array {
    /// The id map's (external id, dense id) pairs, sorted by external id.
    IdPairs,
    /// The id map's second direction: each dense id's external id.
    ByDense,
    /// The tombstones' words.
    Tombstones,
    /// A text id map's ids, concatenated in dense-id order (DESIGN.md
    /// §5.7).
    IdBytes,
    /// Where each dense id's id starts in `IdBytes`, and where the last
    /// ends.
    IdOffsets,
    /// The dense ids sorted by text id.
    IdOrder,
    /// The relation dictionary, as its encoded segment.
    RelationDictionary,
    /// One part of one CSR.
    Csr(Direction, CsrPart, CsrScope),
    /// A persisted vector's elements, row-major: the one array of a
    /// vector's region.
    Values,
}

/// Where an array lies in a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The array.
    pub(crate) array: Array,
    /// The width of its elements in bytes.
    pub(crate) width: usize,
    /// The number of its elements.
    pub(crate) elements: usize,
    /// Its first byte's offset from the region's start.
    pub(crate) offset: usize,
}

impl Entry {
    /// The bytes it occupies.
    pub(crate) fn range(&self) -> Range<usize> {
        self.offset..self.offset + self.width * self.elements
    }
}

/// The layout of one loaded version's region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegionLayout {
    version: VersionId,
    entries: Vec<Entry>,
    /// Each entry's index in `entries`, by its array's encoding: a version
    /// has an entry per relation and direction, so a scan per lookup, one
    /// per array loaded, would be quadratic in its relations.
    index: HashMap<ArrayKey, usize>,
    len: usize,
}

/// An array as (kind, direction, part, scope), as its entry records it.
type ArrayKey = (u8, u8, u8, u64);

impl RegionLayout {
    /// The layout of `version`'s arrays, each given as (array, element
    /// width, elements) and placed in that order at the next aligned
    /// offset after the header and table of contents.
    pub(crate) fn plan(
        version: VersionId,
        arrays: &[(Array, usize, usize)],
    ) -> Result<Self, RegionError> {
        let table = arrays.len().checked_mul(ENTRY_LEN).ok_or(RegionError::TooLarge)?;
        let mut len = HEADER_LEN.checked_add(table).ok_or(RegionError::TooLarge)?;
        let mut entries = Vec::with_capacity(arrays.len());
        for &(array, width, elements) in arrays {
            let offset = len.checked_next_multiple_of(ALIGNMENT).ok_or(RegionError::TooLarge)?;
            let bytes = width.checked_mul(elements).ok_or(RegionError::TooLarge)?;
            len = offset.checked_add(bytes).ok_or(RegionError::TooLarge)?;
            entries.push(Entry { array, width, elements, offset });
        }
        // An allocation's size may not exceed `isize::MAX` once aligned.
        if len > isize::MAX as usize - ALIGNMENT {
            return Err(RegionError::TooLarge);
        }
        Self::indexed(version, entries, len)
    }

    /// The layout of `entries`, indexed by array: an `Entry` error for the
    /// first entry of an array that already has one.
    fn indexed(version: VersionId, entries: Vec<Entry>, len: usize) -> Result<Self, RegionError> {
        let mut index = HashMap::with_capacity(entries.len());
        for (at, entry) in entries.iter().enumerate() {
            if index.insert(encode_array(entry.array), at).is_some() {
                return Err(RegionError::Entry { index: at });
            }
        }
        Ok(Self { version, entries, index, len })
    }

    /// The region's length in bytes.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Where `array` lies, if the region holds it.
    pub(crate) fn entry(&self, array: Array) -> Option<&Entry> {
        self.index.get(&encode_array(array)).map(|&at| &self.entries[at])
    }

    /// Writes the header and table of contents at the start of `region`,
    /// which must be at least as long as the layout.
    pub(crate) fn write_header(&self, region: &mut [u8]) {
        let mut out = Writer { region, at: 0 };
        out.bytes(&MAGIC);
        out.bytes(&FORMAT.to_le_bytes());
        out.bytes(&self.version.to_le_bytes());
        out.u64(self.len as u64);
        out.u64(self.entries.len() as u64);
        out.at = HEADER_LEN;
        for entry in &self.entries {
            let (kind, direction, part, scope) = encode_array(entry.array);
            out.bytes(&[kind, direction, part, 0]);
            out.bytes(&(entry.width as u32).to_le_bytes());
            out.u64(scope);
            out.u64(entry.offset as u64);
            out.u64(entry.elements as u64);
        }
    }

    /// Reads the layout at the start of `region` and checks it: written in
    /// this format for `version`, no longer than `region`, and
    /// every array aligned and inside it, after the table of contents,
    /// and named by one entry.
    pub(crate) fn read(region: &[u8], version: VersionId) -> Result<Self, RegionError> {
        let header = prefix(region, HEADER_LEN)?;
        if header[..4] != MAGIC {
            return Err(RegionError::Magic);
        }
        let found = u32::from_le_bytes(array_at(header, 4));
        if found != FORMAT {
            return Err(RegionError::Format { found });
        }
        let found = i64::from_le_bytes(array_at(header, 8));
        if found != version {
            return Err(RegionError::Version { expected: version, found });
        }
        let len = u64::from_le_bytes(array_at(header, 16));
        let count = u64::from_le_bytes(array_at(header, 24));
        let table =
            count.checked_mul(ENTRY_LEN as u64).and_then(|t| t.checked_add(HEADER_LEN as u64));
        let table =
            table.ok_or(RegionError::Short { needed: u64::MAX, found: region.len() as u64 })?;
        let region = prefix(region, usize::try_from(len.max(table)).unwrap_or(usize::MAX))?;
        let (len, table) = (len as usize, table as usize);
        let entries = region[HEADER_LEN..table]
            .chunks_exact(ENTRY_LEN)
            .enumerate()
            .map(|(index, bytes)| {
                read_entry(bytes)
                    .filter(|entry| fits(entry, table, len))
                    .ok_or(RegionError::Entry { index })
            })
            .collect::<Result<_, _>>()?;
        Self::indexed(version, entries, len)
    }
}

/// The first `len` bytes of `region`, or `Short`.
fn prefix(region: &[u8], len: usize) -> Result<&[u8], RegionError> {
    region.get(..len).ok_or(RegionError::Short { needed: len as u64, found: region.len() as u64 })
}

/// The `N` bytes of `bytes` at `at`, which the caller has checked exist.
fn array_at<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    let mut array = [0; N];
    array.copy_from_slice(&bytes[at..at + N]);
    array
}

/// Whether `entry` is aligned and lies between the table's end and `len`.
fn fits(entry: &Entry, table: usize, len: usize) -> bool {
    let end = entry.width.checked_mul(entry.elements).and_then(|b| b.checked_add(entry.offset));
    entry.offset.is_multiple_of(ALIGNMENT)
        && entry.offset >= table
        && end.is_some_and(|end| end <= len)
}

/// An entry of the table, if it names an array.
fn read_entry(bytes: &[u8]) -> Option<Entry> {
    let array = decode_array(bytes[0], bytes[1], bytes[2], u64::from_le_bytes(array_at(bytes, 8)))?;
    let width = u32::from_le_bytes(array_at(bytes, 4)) as usize;
    let offset = usize::try_from(u64::from_le_bytes(array_at(bytes, 16))).ok()?;
    let elements = usize::try_from(u64::from_le_bytes(array_at(bytes, 24))).ok()?;
    (bytes[3] == 0).then_some(Entry { array, width, elements, offset })
}

/// An array as (kind, direction, part, scope).
fn encode_array(array: Array) -> ArrayKey {
    match array {
        Array::IdPairs => (1, 0, 0, 0),
        Array::ByDense => (2, 0, 0, 0),
        Array::Tombstones => (3, 0, 0, 0),
        Array::RelationDictionary => (4, 0, 0, 0),
        Array::Csr(direction, part, scope) => {
            let direction = match direction {
                Direction::Forward => 0,
                Direction::Reverse => 1,
            };
            let part = match part {
                CsrPart::Offsets => 0,
                CsrPart::Targets => 1,
                CsrPart::Weights => 2,
                CsrPart::Rows => 3,
            };
            let scope = match scope {
                CsrScope::Relation(index) => index as u64,
                CsrScope::Merged => MERGED,
            };
            (5, direction, part, scope)
        }
        Array::Values => (6, 0, 0, 0),
        Array::IdBytes => (7, 0, 0, 0),
        Array::IdOffsets => (8, 0, 0, 0),
        Array::IdOrder => (9, 0, 0, 0),
    }
}

/// The array (kind, direction, part, scope) names, if any.
fn decode_array(kind: u8, direction: u8, part: u8, scope: u64) -> Option<Array> {
    match (kind, direction, part, scope) {
        (1, 0, 0, 0) => Some(Array::IdPairs),
        (2, 0, 0, 0) => Some(Array::ByDense),
        (3, 0, 0, 0) => Some(Array::Tombstones),
        (4, 0, 0, 0) => Some(Array::RelationDictionary),
        (6, 0, 0, 0) => Some(Array::Values),
        (7, 0, 0, 0) => Some(Array::IdBytes),
        (8, 0, 0, 0) => Some(Array::IdOffsets),
        (9, 0, 0, 0) => Some(Array::IdOrder),
        (5, ..) => {
            let direction = match direction {
                0 => Direction::Forward,
                1 => Direction::Reverse,
                _ => return None,
            };
            let part = match part {
                0 => CsrPart::Offsets,
                1 => CsrPart::Targets,
                2 => CsrPart::Weights,
                3 => CsrPart::Rows,
                _ => return None,
            };
            let scope = match scope {
                MERGED => CsrScope::Merged,
                index => CsrScope::Relation(usize::try_from(index).ok()?),
            };
            Some(Array::Csr(direction, part, scope))
        }
        _ => None,
    }
}

/// Writes fields one after another into a region's header.
struct Writer<'a> {
    region: &'a mut [u8],
    at: usize,
}

impl Writer<'_> {
    fn bytes(&mut self, bytes: &[u8]) {
        self.region[self.at..self.at + bytes.len()].copy_from_slice(bytes);
        self.at += bytes.len();
    }

    fn u64(&mut self, value: u64) {
        self.bytes(&value.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrays() -> Vec<(Array, usize, usize)> {
        let offsets = Array::Csr(Direction::Forward, CsrPart::Offsets, CsrScope::Relation(0));
        let targets = Array::Csr(Direction::Reverse, CsrPart::Targets, CsrScope::Merged);
        let rows = Array::Csr(Direction::Reverse, CsrPart::Rows, CsrScope::Relation(2));
        vec![
            (Array::IdPairs, 16, 3),
            (Array::ByDense, 4, 3),
            (Array::Tombstones, 8, 1),
            (Array::RelationDictionary, 1, 21),
            (offsets, 8, 4),
            (targets, 4, 0),
            (rows, 4, 2),
        ]
    }

    #[test]
    fn every_array_is_aligned_and_the_region_ends_with_the_last() {
        let layout = RegionLayout::plan(7, &arrays()).unwrap();
        let entries: Vec<_> = arrays().iter().map(|&(a, ..)| *layout.entry(a).unwrap()).collect();
        let first = entries[0].offset;
        assert!(first >= 64 + 7 * 32, "{first}");
        let mut end = first;
        for (entry, &(array, width, elements)) in entries.iter().zip(&arrays()) {
            assert_eq!((entry.array, entry.width, entry.elements), (array, width, elements));
            assert_eq!(entry.offset % ALIGNMENT, 0);
            assert!(entry.offset >= end);
            assert!(entry.offset < end + ALIGNMENT);
            assert_eq!(entry.range(), entry.offset..entry.offset + width * elements);
            end = entry.range().end;
        }
        assert_eq!(layout.len(), end);
        assert_eq!(
            layout.entry(Array::Csr(Direction::Reverse, CsrPart::Weights, CsrScope::Merged)),
            None
        );
    }

    #[test]
    fn a_layout_reads_back_from_its_header() {
        let layout = RegionLayout::plan(7, &arrays()).unwrap();
        let mut region = vec![0u8; layout.len()];
        layout.write_header(&mut region);
        assert_eq!(RegionLayout::read(&region, 7).unwrap(), layout);
    }

    #[test]
    fn a_region_of_another_version_or_format_is_refused() {
        let layout = RegionLayout::plan(7, &arrays()).unwrap();
        let mut region = vec![0u8; layout.len()];
        layout.write_header(&mut region);
        assert_eq!(
            RegionLayout::read(&region, 8),
            Err(RegionError::Version { expected: 8, found: 7 })
        );
        let mut bad = region.clone();
        bad[0] = b'X';
        assert_eq!(RegionLayout::read(&bad, 7), Err(RegionError::Magic));
        let mut bad = region;
        bad[4] = 99;
        assert_eq!(RegionLayout::read(&bad, 7), Err(RegionError::Format { found: 99 }));
    }

    #[test]
    fn a_short_region_is_refused() {
        let layout = RegionLayout::plan(7, &arrays()).unwrap();
        let mut region = vec![0u8; layout.len()];
        layout.write_header(&mut region);
        let short = layout.len() - 1;
        assert_eq!(
            RegionLayout::read(&region[..short], 7),
            Err(RegionError::Short { needed: layout.len() as u64, found: short as u64 })
        );
        assert!(matches!(RegionLayout::read(&region[..40], 7), Err(RegionError::Short { .. })));
        assert!(matches!(RegionLayout::read(&region[..100], 7), Err(RegionError::Short { .. })));
    }

    #[test]
    fn an_entry_outside_the_region_or_misaligned_is_refused() {
        let layout = RegionLayout::plan(7, &arrays()).unwrap();
        let mut region = vec![0u8; layout.len()];
        layout.write_header(&mut region);
        // The second entry's offset, one byte on.
        let offset = 64 + 32 + 16;
        let mut bad = region.clone();
        bad[offset] += 1;
        assert_eq!(RegionLayout::read(&bad, 7), Err(RegionError::Entry { index: 1 }));
        // Its element count, past the end.
        let mut bad = region.clone();
        bad[offset + 8..offset + 16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(RegionLayout::read(&bad, 7), Err(RegionError::Entry { index: 1 }));
        // Its kind of array, unknown.
        let mut bad = region;
        bad[64 + 32] = 0;
        assert_eq!(RegionLayout::read(&bad, 7), Err(RegionError::Entry { index: 1 }));
    }

    #[test]
    fn every_entry_is_found_by_its_array() {
        let many: Vec<_> = (0..1000)
            .flat_map(|index| {
                let scope = CsrScope::Relation(index);
                [Direction::Forward, Direction::Reverse]
                    .map(|direction| (Array::Csr(direction, CsrPart::Targets, scope), 4, index % 7))
            })
            .chain(arrays())
            .collect();
        let layout = RegionLayout::plan(7, &many).unwrap();
        let mut region = vec![0u8; layout.len()];
        layout.write_header(&mut region);
        let read = RegionLayout::read(&region, 7).unwrap();
        for layout in [layout, read] {
            for (index, &(array, ..)) in many.iter().enumerate() {
                assert_eq!(layout.entry(array), Some(&layout.entries[index]));
            }
            let absent = Array::Csr(Direction::Forward, CsrPart::Targets, CsrScope::Merged);
            assert_eq!(layout.entry(absent), None);
        }
    }

    #[test]
    fn an_array_with_two_entries_is_refused() {
        let mut twice = arrays();
        twice.push(twice[1]);
        assert_eq!(RegionLayout::plan(7, &twice), Err(RegionError::Entry { index: 6 }));
        let layout = RegionLayout::plan(7, &arrays()).unwrap();
        let mut region = vec![0u8; layout.len()];
        layout.write_header(&mut region);
        // The third entry renamed as the second: tombstones as `ByDense`.
        region[64 + 2 * 32] = 2;
        region[64 + 2 * 32 + 4] = 4;
        assert_eq!(RegionLayout::read(&region, 7), Err(RegionError::Entry { index: 2 }));
    }

    #[test]
    fn a_layout_too_large_to_address_is_refused() {
        let huge = [(Array::IdPairs, 16, usize::MAX / 8)];
        assert_eq!(RegionLayout::plan(7, &huge), Err(RegionError::TooLarge));
    }
}
