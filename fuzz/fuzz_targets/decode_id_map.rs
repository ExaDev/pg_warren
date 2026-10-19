//! `IdMap::decode` never panics for any external id type, and what it
//! accepts re-encodes to itself.
//!
//! Input: tombstones segment length (u16), the tombstones segment, then the
//! id map segment.
#![no_main]

use libfuzzer_sys::fuzz_target;
use warren_core::graph::{ExternalId, IdMap, Tombstones};

fn check<E: ExternalId>(id_map: &[u8], tombstones: &Tombstones) {
    if let Ok(decoded) = IdMap::<E>::decode(id_map, tombstones.clone()) {
        let bytes = decoded.encode().expect("a decoded id map re-encodes");
        assert_eq!(IdMap::<E>::decode(&bytes, tombstones.clone()), Ok(decoded));
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((len, rest)) = data.split_first_chunk::<2>() else {
        return;
    };
    let (tombstones, id_map) = rest.split_at(usize::from(u16::from_le_bytes(*len)).min(rest.len()));
    let tombstones = Tombstones::decode(tombstones).unwrap_or_default();
    check::<i32>(id_map, &tombstones);
    check::<i64>(id_map, &tombstones);
    check::<[u8; 16]>(id_map, &tombstones);
});
