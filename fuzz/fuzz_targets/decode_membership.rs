//! `Membership::decode` never panics, and what it accepts re-encodes to
//! itself. The first byte picks the quotient's id space.
#![no_main]

use libfuzzer_sys::fuzz_target;
use warren_core::graph::Membership;

fuzz_target!(|data: &[u8]| {
    let Some((&quotient, segment)) = data.split_first() else { return };
    let quotient_id_space = u64::from(quotient);
    if let Ok(membership) = Membership::decode(segment, quotient_id_space) {
        let bytes = membership.encode().expect("a decoded membership re-encodes");
        assert_eq!(Membership::decode(&bytes, quotient_id_space), Ok(membership));
    }
});
