# test/

Integration tests that cannot be expressed as `#[pg_test]` in
`crates/pg_warren`: multi-session and multi-transaction scenarios, oracle
suites against recursive CTEs / NetworkX on generated fixtures, and the
Appendix A use cases (DESIGN.md §12). Empty until M2.
