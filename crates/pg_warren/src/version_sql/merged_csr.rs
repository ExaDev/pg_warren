//! Tests: `diff` (its `all_relations` delta and degree shifts included)
//! gives the same result whether the graph stores its merged CSR or merges
//! the relations' rows on the fly, and `compact` keeps the graph's choice
//! (DESIGN.md §5.3).

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{merged_pair, on_both, same_with_or_without_merged};

    /// The `:merged` segments of graph `graph`'s current version.
    fn merged_segments(graph: &str) -> Result<Option<i64>, Error> {
        Spi::get_one::<i64>(&format!(
            "SELECT count(DISTINCT s.segment) FROM warren.segments s
             JOIN warren.graphs g ON s.version_id = g.current_version
             WHERE g.name = '{graph}' AND s.segment LIKE '%:merged'"
        ))
    }

    #[pg_test]
    fn diff_and_compact_read_the_same_without_the_merged_csr() -> Result<(), Error> {
        merged_pair("vm")?;
        let diff = same_with_or_without_merged(
            "vm",
            "SELECT (warren.diff('{g}', {p}, {v}) - 'from' - 'to')::text",
        )?;
        assert!(diff.contains("\"all_relations\""), "{diff}");
        on_both("vm", "SELECT warren.compact('{g}')")?;
        assert_eq!(merged_segments("vm_s")?, Some(6), "forward and reverse, with weights");
        assert_eq!(merged_segments("vm_u")?, Some(0));
        same_with_or_without_merged(
            "vm",
            "SELECT format('%s %s', node_id, component_id)
             FROM warren.components('{g}', NULL::bigint)",
        )?;
        Ok(())
    }
}
