//! `AnyCsr::decode` never panics, what it accepts re-encodes to segments
//! that decode and re-encode to the same bytes, and its rows read the same
//! looked up one by one and swept up and down.
//!
//! Input: id space (u16), rows length (u16), offsets length (u16), targets
//! length (u16), flags (u8: bit 0 weights, bit 1 rows, bit 2 the id space
//! plus 2³², so `u64` targets and rows), then the rows, offsets, targets
//! and weights segments.
#![no_main]

use libfuzzer_sys::fuzz_target;
use warren_core::graph::{AnyCsr, CsrRows, CsrSegments};

/// Every source of an id space up to this size is read; past it, the first
/// ones, the stored rows and their neighbours, and the id space's edges.
const READ_WHOLE: u64 = 1 << 12;

fn take<'a>(data: &mut &'a [u8], len: usize) -> &'a [u8] {
    let (head, tail) = data.split_at(len.min(data.len()));
    *data = tail;
    head
}

fn take_u16(data: &mut &[u8]) -> usize {
    let bytes = take(data, 2);
    bytes.iter().rev().fold(0, |acc, &b| acc << 8 | usize::from(b))
}

/// A CSR's segments as bytes: a NaN weight is not equal to itself.
fn bytes(s: &CsrSegments) -> (Option<&[u8]>, &[u8], &[u8], Option<&[u8]>) {
    (s.rows(), s.offsets(), s.targets(), s.weights())
}

/// The sources `read` reads, ascending and distinct, `u64::MAX` included.
fn sources(csr: &AnyCsr, id_space: u64) -> Vec<u64> {
    let stored: Vec<u64> = match csr {
        AnyCsr::U32(csr) => csr.rows().unwrap_or(&[]).iter().map(|&row| u64::from(row)).collect(),
        AnyCsr::U64(csr) => csr.rows().unwrap_or(&[]).to_vec(),
    };
    let mut sources: Vec<u64> = (0..=id_space.min(READ_WHOLE)).collect();
    for row in stored {
        sources.extend([row.saturating_sub(1), row, row.saturating_add(1)]);
    }
    sources.extend([id_space.saturating_sub(1), id_space, u64::MAX]);
    sources.sort_unstable();
    sources.dedup();
    sources
}

/// Reads `csr`'s rows by lookup, then swept ascending and descending.
fn read(csr: &AnyCsr, id_space: u64) {
    let looked_up: Vec<(u64, Vec<u64>, u64)> = sources(csr, id_space)
        .into_iter()
        .map(|source| (source, csr.neighbours(source).collect(), csr.degree(source)))
        .collect();
    for (source, row, degree) in &looked_up {
        assert_eq!(row.len() as u64, *degree, "{source}");
        assert!(row.iter().all(|&target| target < id_space), "{source}");
        assert!(*source < id_space || row.is_empty(), "{source}");
    }
    if id_space <= READ_WHOLE {
        let degrees: u64 = looked_up.iter().map(|(_, _, degree)| degree).sum();
        assert_eq!(degrees, csr.edge_count());
    }
    let rows = CsrRows::from(csr);
    let mut sweep = rows.sweep().expect("a sweep of one CSR allocates nothing");
    for (source, row, degree) in looked_up.iter().chain(looked_up.iter().rev()) {
        assert_eq!(&sweep.neighbours(*source).collect::<Vec<_>>(), row, "{source}");
        assert_eq!(sweep.degree_bound(*source), *degree, "{source}");
    }
}

fuzz_target!(|data: &[u8]| {
    let mut data = data;
    let id_space = take_u16(&mut data) as u64;
    let rows_len = take_u16(&mut data);
    let offsets_len = take_u16(&mut data);
    let targets_len = take_u16(&mut data);
    let flags = take(&mut data, 1).first().copied().unwrap_or(0);
    let (weighted, sparse, wide) = (flags & 1 == 1, flags & 2 == 2, flags & 4 == 4);
    let id_space = if wide { id_space + (1 << 32) } else { id_space };
    let rows = take(&mut data, rows_len);
    let rows = sparse.then_some(rows);
    let offsets = take(&mut data, offsets_len);
    let targets = take(&mut data, targets_len);
    let weights = weighted.then_some(data);
    if let Ok(csr) = AnyCsr::decode(id_space, rows, offsets, targets, weights) {
        let segments = csr.encode().expect("a decoded CSR re-encodes");
        let again = AnyCsr::decode(
            id_space,
            segments.rows(),
            segments.offsets(),
            segments.targets(),
            segments.weights(),
        )
        .expect("a re-encoded CSR decodes");
        let twice = again.encode().expect("a decoded CSR re-encodes");
        assert_eq!(bytes(&twice), bytes(&segments));
        read(&csr, id_space);
    }
});
