//! `warren-core`: the domain of pg_warren — graph structure, traversal and
//! compute kernels (DESIGN.md §11).
//!
//! This crate must never depend on pgrx or anything Postgres-specific
//! (DESIGN.md §15.4; enforced in CI). It takes plain slices and a thread
//! count, and returns results. One module per domain concept.

// Offsets and dense ids are u64 and index in-memory arrays; a 32-bit target
// could not address a graph-sized buffer and would make those casts lossy.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("warren-core requires a 64-bit target");

pub mod compute;
pub mod graph;
pub mod interrupts;
pub mod spectral;
pub mod traversal;
pub mod version;

/// The version of this crate, as recorded in its `Cargo.toml`.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_cargo_package_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
        assert!(!version().is_empty());
    }

    #[test]
    fn version_is_semver_shaped() {
        let parts: Vec<&str> = version().split('.').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.parse::<u64>().is_ok()));
    }
}
