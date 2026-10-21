//! Seeded pseudo-random values: the spectral kernels' random vectors and
//! the centralities' sampling keys. Each value is a function of the seed,
//! a dense id and, for a vector, its stream number, alone (a counter-based
//! generator), so it is the same for every thread count (DESIGN.md
//! Appendix A.2: identical results across runs).

use crate::graph::DenseId;

/// The 64-bit golden ratio, an odd constant that spreads consecutive
/// integers over the 64 bits when multiplied by them (a bijection).
pub(crate) const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;

/// The SplitMix64 finaliser: a bijective mix of 64 bits.
pub(crate) fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A uniform value in `[-1, 1)` for element `node` of stream `stream`.
pub(crate) fn uniform(seed: u64, stream: u64, node: DenseId) -> f64 {
    let bits = mix(mix(seed ^ mix(stream.wrapping_mul(GOLDEN))) ^ node.wrapping_mul(GOLDEN));
    // The top 53 bits as a fraction in [0, 1), then mapped to [-1, 1).
    (bits >> 11) as f64 / (1u64 << 52) as f64 - 1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_depend_only_on_seed_stream_and_node() {
        assert_eq!(uniform(7, 0, 3), uniform(7, 0, 3));
        assert_ne!(uniform(7, 0, 3), uniform(8, 0, 3));
        assert_ne!(uniform(7, 0, 3), uniform(7, 1, 3));
        assert_ne!(uniform(7, 0, 3), uniform(7, 0, 4));
    }

    #[test]
    fn values_are_spread_over_minus_one_to_one() {
        let values: Vec<f64> = (0..10_000).map(|node| uniform(1, 0, node)).collect();
        assert!(values.iter().all(|v| (-1.0..1.0).contains(v)));
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        assert!(mean.abs() < 0.05, "mean {mean}");
        assert!(values.iter().any(|&v| v < -0.9) && values.iter().any(|&v| v > 0.9));
    }
}
