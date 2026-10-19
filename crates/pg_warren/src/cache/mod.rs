//! Backend-local cache: loads a version's segments and vectors on first use,
//! keyed by version id; `prewarm` / `evict` (DESIGN.md §5.5). Arrives in M3.
