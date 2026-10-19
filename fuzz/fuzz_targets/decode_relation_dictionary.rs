//! `RelationDictionary::decode` never panics, and what it accepts re-encodes
//! to itself.
#![no_main]

use libfuzzer_sys::fuzz_target;
use warren_core::graph::RelationDictionary;

fuzz_target!(|data: &[u8]| {
    if let Ok(relations) = RelationDictionary::decode(data) {
        let bytes = relations.encode().expect("a decoded dictionary re-encodes");
        assert_eq!(RelationDictionary::decode(&bytes), Ok(relations));
    }
});
