//! `IdMap::decode_text` never panics, and what it accepts re-encodes to
//! itself, in its encoding, and answers both lookups consistently.
//!
//! Input: the encoding (u8), then the lengths (u16 each) of the tombstones,
//! `id_bytes` and `id_offsets` segments, those segments, and the rest is
//! the `id_order` segment.
#![no_main]

use libfuzzer_sys::fuzz_target;
use warren_core::graph::{IdMap, TextKeys, Tombstones};

/// The next `len`-prefixed part of `data`.
fn part<'a>(data: &mut &'a [u8]) -> Option<&'a [u8]> {
    let (len, rest) = data.split_first_chunk::<2>()?;
    let (part, rest) = rest.split_at(usize::from(u16::from_le_bytes(*len)).min(rest.len()));
    *data = rest;
    Some(part)
}

fuzz_target!(|data: &[u8]| {
    let Some((&encoding, mut data)) = data.split_first() else {
        return;
    };
    let (Some(tombstones), Some(id_bytes), Some(id_offsets)) =
        (part(&mut data), part(&mut data), part(&mut data))
    else {
        return;
    };
    let tombstones = Tombstones::decode(tombstones).unwrap_or_default();
    let decode = |bytes: &[u8], offsets: &[u8], order: &[u8]| {
        IdMap::<TextKeys>::decode_text(bytes, offsets, order, tombstones.clone(), encoding)
    };
    if let Ok(decoded) = decode(id_bytes, id_offsets, data) {
        let segments = decoded.encode_text(encoding).expect("a decoded id map re-encodes");
        let again = decode(segments.id_bytes(), segments.id_offsets(), segments.id_order());
        assert_eq!(again.as_ref(), Ok(&decoded));
        for dense in 0..decoded.id_space() {
            let id = decoded.external_id(dense).expect("every dense id has an id");
            let live = !decoded.tombstones().contains(dense);
            assert_eq!(decoded.dense_id(id), live.then_some(dense));
        }
    }
});
