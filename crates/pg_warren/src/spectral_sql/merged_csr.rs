//! Tests: `eigen` and `distance` give the same results, bit for bit,
//! whether the graph stores its merged CSR or merges the relations' rows
//! on the fly (DESIGN.md §5.3).

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{merged_pair, same_with_or_without_merged};

    fn same(query: &str) -> Result<String, Error> {
        same_with_or_without_merged("sm", query)
    }

    #[pg_test]
    fn spectra_and_distances_read_the_same_without_the_merged_csr() -> Result<(), Error> {
        merged_pair("sm")?;
        for (matrix, which) in [("adjacency", "largest"), ("normalized_laplacian", "smallest")] {
            same(&format!(
                "SELECT format('%s %s %s', i, eigenvalue, residual)
                 FROM warren.eigen('{{g}}', 4, '{matrix}', '{which}', dtype := 'f64',
                                   vectors_prefix := 'e', persist := false)"
            ))?;
            same(
                "SELECT format('%s %s', node_id, value)
                  FROM warren.vector_values('{g}', 'e_1', NULL::bigint)",
            )?;
        }
        for method in ["'edit'", "'degree'", "'spectral', k := 3, dtype := 'f64'"] {
            same(&format!(
                "SELECT (warren.distance('{{g}}', {{p}}, {{v}}, {method}) - 'from' - 'to')::text"
            ))?;
        }
        Ok(())
    }

    /// A cross-graph comparison of the two layouts (`graph2`, aligned by
    /// external id) finds no change, and `info` reports the layouts.
    #[pg_test]
    fn the_two_layouts_compare_as_the_same_graph() -> Result<(), Error> {
        merged_pair("sx")?;
        // Each method's measure of change, which must be 0.
        for (method, change) in [
            ("'edit'", "-> 'overall' -> 'distance'"),
            ("'degree'", "-> 'wasserstein'"),
            ("'spectral', k := 3, dtype := 'f64'", "-> 'l2'"),
        ] {
            let found = Spi::get_one::<f64>(&format!(
                "SELECT (warren.distance('sx_s',
                            (SELECT current_version FROM warren.graphs WHERE name = 'sx_s'),
                            (SELECT current_version FROM warren.graphs WHERE name = 'sx_u'),
                            {method}, graph2 := 'sx_u') {change})::float8"
            ))?;
            assert_eq!(found, Some(0.0), "{method}");
        }
        let layouts = Spi::get_one::<String>(
            "SELECT string_agg(name || ':' || (warren.info(name) ->> 'store_merged'), ' ' ORDER BY name)
             FROM warren.graphs",
        )?;
        assert_eq!(layouts.as_deref(), Some("sx_s:true sx_u:false"));
        Ok(())
    }
}
