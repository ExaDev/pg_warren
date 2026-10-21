//! Seeded randomness for the community kernels: counter-based, so each
//! value is a function of the seed, a stream and a node alone, the same
//! for any thread count (as `spectral`'s start vectors are).

use crate::graph::DenseId;

/// The SplitMix64 finaliser: a bijective mix of 64 bits.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;

/// A pseudo-random 64-bit value for `node` of stream `stream`.
fn bits(seed: u64, stream: u64, node: DenseId) -> u64 {
    mix(mix(seed ^ mix(stream.wrapping_mul(GOLDEN))) ^ node.wrapping_mul(GOLDEN))
}

/// `node`'s place in the seeded order of the nodes: sorting by it shuffles
/// them.
pub(super) fn priority(seed: u64, node: DenseId) -> u64 {
    bits(seed, 0, node)
}

/// A uniform value in `[0, 1)` for `node` of stream `stream` (streams 1
/// and up; stream 0 is [`priority`]'s).
pub(super) fn unit(seed: u64, stream: u64, node: DenseId) -> f64 {
    (bits(seed, stream, node) >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_depend_only_on_seed_stream_and_node() {
        assert_eq!(priority(7, 3), priority(7, 3));
        assert_ne!(priority(7, 3), priority(8, 3));
        assert_ne!(priority(7, 3), priority(7, 4));
        assert_ne!(unit(7, 1, 3), unit(7, 2, 3));
    }

    #[test]
    fn units_are_spread_over_zero_to_one() {
        let values: Vec<f64> = (0..10_000).map(|node| unit(1, 1, node)).collect();
        assert!(values.iter().all(|v| (0.0..1.0).contains(v)));
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        assert!((mean - 0.5).abs() < 0.02, "mean {mean}");
    }
}
