//! Graph structure: the immutable CSR snapshot of one graph version.
//!
//! Owns forward/reverse CSR per relation, the external-id ↔ dense-id map with
//! stable dense ids across versions, and the tombstone bitmap
//! (DESIGN.md §5.2, §5.3). Arrives in M1.
