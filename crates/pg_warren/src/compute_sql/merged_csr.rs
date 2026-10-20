//! Tests: every compute function reading all relations (`spmv`, `spmm`
//! and `vector_degree` without `rels`, `gather`, `triangles` and the
//! library) gives the same values, bit for bit, whether the graph stores
//! its merged CSR or merges the relations' rows on the fly (DESIGN.md
//! §5.3).

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{merged_pair, on_both, same_with_or_without_merged};

    fn same(query: &str) -> Result<String, Error> {
        same_with_or_without_merged("cm", query)
    }

    /// The rows of vector `name` (`vector_values`) or of matrix `name`
    /// (`matrix_rows`) after `setup` writes it on both graphs.
    fn same_after(setup: &str, read: &str, name: &str) -> Result<String, Error> {
        on_both("cm", setup)?;
        let column = if read == "matrix_rows" { "row" } else { "value" };
        same(&format!(
            "SELECT format('%s %s', r.node_id, r.{column})
             FROM warren.{read}('{{g}}', '{name}', NULL::bigint) r"
        ))
    }

    #[pg_test]
    fn products_and_gathers_read_the_same_without_the_merged_csr() -> Result<(), Error> {
        merged_pair("cm")?;
        for direction in ["out", "in", "both"] {
            let degree = format!("SELECT warren.vector_degree('{{g}}', 'deg', '{direction}')");
            same_after(&degree, "vector_values", "deg")?;
        }
        on_both(
            "cm",
            "SELECT warren.vector_from_query('{g}', 'x', 'SELECT id, (id % 7)::float8 / 3 FROM cm_nodes');
             SELECT warren.vector_from_query('{g}', 'm',
                 'SELECT id, ARRAY[id::float8 / 7, (id % 5)::float8] FROM cm_nodes')",
        )?;
        for semiring in
            ["plus_times", "min_plus", "max_times", "plus_pair", "any_pair", "min_second"]
        {
            for transpose in [false, true] {
                let product = |function: &str, x: &str| {
                    format!(
                        "SELECT warren.{function}('{{g}}', '{x}', 'y', semiring := '{semiring}',
                                                  transpose := {transpose})"
                    )
                };
                same_after(&product("spmv", "x"), "vector_values", "y")?;
                same_after(&product("spmm", "m"), "matrix_rows", "y")?;
            }
        }
        for direction in ["in", "out", "both"] {
            for (messages, witness, combine) in [
                ("SELECT id, id::text FROM cm_nodes", "NULL::text[]", "collect"),
                ("SELECT id, id::float8 / 3 FROM cm_nodes", "NULL::float8", "sum"),
                ("SELECT id, 1 FROM cm_nodes", "NULL::bigint", "count"),
            ] {
                same(&format!(
                    "SELECT format('%s %s', node_id, value)
                     FROM warren.gather('{{g}}', '{messages}', NULL::bigint, {witness},
                                        combine := '{combine}', direction := '{direction}')"
                ))?;
            }
        }
        for (setup, name) in [
            ("SELECT warren.triangles('{g}', 't')", "t"),
            ("SELECT warren.pagerank('{g}', 'pr')", "pr"),
            ("SELECT warren.components_vector('{g}', 'cc')", "cc"),
            ("SELECT warren.label_propagation('{g}', 'lp')", "lp"),
            ("SELECT warren.sssp_vector('{g}', 'sp', 1::bigint)", "sp"),
        ] {
            same_after(setup, "vector_values", name)?;
        }
        let aggregate = "SELECT warren.aggregate_neighbours('{g}', 'm', 'agg', hops := 2)";
        same_after(aggregate, "matrix_rows", "agg")?;
        Ok(())
    }
}
