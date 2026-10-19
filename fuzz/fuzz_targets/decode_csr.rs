//! `AnyCsr::decode` never panics, and what it accepts re-encodes to itself.
//!
//! Input: id space (u16), offsets length (u16), targets length (u16), a
//! weights flag (u8), then the offsets, targets and weights segments.
#![no_main]

use libfuzzer_sys::fuzz_target;
use warren_core::graph::AnyCsr;

fn take<'a>(data: &mut &'a [u8], len: usize) -> &'a [u8] {
    let (head, tail) = data.split_at(len.min(data.len()));
    *data = tail;
    head
}

fn take_u16(data: &mut &[u8]) -> usize {
    let bytes = take(data, 2);
    bytes.iter().rev().fold(0, |acc, &b| acc << 8 | usize::from(b))
}

fuzz_target!(|data: &[u8]| {
    let mut data = data;
    let id_space = take_u16(&mut data) as u64;
    let offsets_len = take_u16(&mut data);
    let targets_len = take_u16(&mut data);
    let weighted = take(&mut data, 1).first().is_some_and(|flag| flag & 1 == 1);
    let offsets = take(&mut data, offsets_len);
    let targets = take(&mut data, targets_len);
    let weights = weighted.then_some(data);
    if let Ok(csr) = AnyCsr::decode(id_space, offsets, targets, weights) {
        let segments = csr.encode().expect("a decoded CSR re-encodes");
        let again = AnyCsr::decode(
            id_space,
            &segments.offsets,
            &segments.targets,
            segments.weights.as_deref(),
        );
        assert_eq!(again.as_ref(), Ok(&csr));
    }
});
