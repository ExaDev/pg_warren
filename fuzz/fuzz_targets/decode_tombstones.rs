//! `Tombstones::decode` never panics, and what it accepts re-encodes to
//! itself.
#![no_main]

use libfuzzer_sys::fuzz_target;
use warren_core::graph::Tombstones;

fuzz_target!(|data: &[u8]| {
    if let Ok(tombstones) = Tombstones::decode(data) {
        let bytes = tombstones.encode().expect("decoded tombstones re-encode");
        assert_eq!(Tombstones::decode(&bytes), Ok(tombstones));
    }
});
