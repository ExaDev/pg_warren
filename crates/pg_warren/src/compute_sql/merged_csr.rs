//! Tests: every compute function reading all relations (`spmv`, `spmm`
//! and `vector_degree` without `rels`, `gather`, `triangles`, the
//! structural kernels and the library) gives the same values, bit for
//! bit, whether the graph stores its merged CSR or merges the relations'
//! rows on the fly (DESIGN.md §5.3).

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{
        merged_pair, merged_pair_with, on_both, same_failure, same_with_or_without_merged,
    };

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
            ("SELECT warren.core_number('{g}', 'k')", "k"),
            ("SELECT warren.strong_components_vector('{g}', 'scc')", "scc"),
            ("SELECT warren.pagerank('{g}', 'pr')", "pr"),
            ("SELECT warren.components_vector('{g}', 'cc')", "cc"),
            ("SELECT warren.label_propagation('{g}', 'lp')", "lp"),
            ("SELECT warren.sssp_vector('{g}', 'sp', 1::bigint)", "sp"),
            ("SELECT warren.eigenvector_centrality('{g}', 'ev', max_iter := 20)", "ev"),
            ("SELECT warren.katz_centrality('{g}', 'k', max_iter := 20)", "k"),
            ("SELECT warren.hits('{g}', 'h', 'a', max_iter := 20)", "a"),
            ("SELECT warren.clustering('{g}', 'cl')", "cl"),
            ("SELECT warren.label_propagation_majority('{g}', 'lpm')", "lpm"),
            ("SELECT warren.betweenness_centrality('{g}', 'bc', k := 20)", "bc"),
            ("SELECT warren.betweenness_centrality('{g}', 'bu', directed := false)", "bu"),
            ("SELECT warren.closeness_centrality('{g}', 'cl', weighted := false)", "cl"),
            ("SELECT warren.harmonic_centrality('{g}', 'hc', k := 10, directed := false)", "hc"),
        ] {
            same_after(setup, "vector_values", name)?;
        }
        let aggregate = "SELECT warren.aggregate_neighbours('{g}', 'm', 'agg', hops := 2)";
        same_after(aggregate, "matrix_rows", "agg")?;
        same(
            "SELECT format('%s %s %s', source, target, weight)
             FROM warren.minimum_spanning_forest('{g}', NULL::bigint)",
        )?;
        for measure in ["jaccard", "adamic_adar", "common_neighbours"] {
            same(&format!(
                "SELECT format('%s %s %s', source, target, score)
                 FROM warren.similar_pairs('{{g}}', NULL::bigint, k := 3, measure := '{measure}')"
            ))?;
            same(&format!(
                "SELECT format('%s %s %s', source, target, score)
                 FROM warren.similarity('{{g}}', 'SELECT a, b FROM generate_series(1, 30) a,
                                                  generate_series(1, 30) b', NULL::bigint,
                                        '{measure}')"
            ))?;
        }
        for kernel in ["topological_order", "dag_longest_path"] {
            let refused = same_failure("cm", &format!("warren.{kernel}('{{g}}', 'o')"))?;
            assert!(
                refused.is_some_and(|refused| refused.starts_with("22023: the graph has a cycle"))
            );
        }
        // Without the self-loops and the edges from a larger id to a
        // smaller, every relation's edges form one DAG.
        on_both("cm", "DELETE FROM cm_edges WHERE src >= dst; SELECT warren.refresh('{g}')")?;
        same_after("SELECT warren.topological_order('{g}', 'o')", "vector_values", "o")?;
        same_after("SELECT warren.dag_longest_path('{g}', 'l')", "vector_values", "l")?;
        Ok(())
    }

    /// Products pushed over the forward CSRs (no reverse CSR), on an
    /// unweighted graph.
    #[pg_test]
    fn pushed_unweighted_products_read_the_same() -> Result<(), Error> {
        merged_pair_with("cu", false, false)?;
        on_both(
            "cu",
            "SELECT warren.vector_from_query('{g}', 'x', 'SELECT id, (id % 7)::float8 / 3 FROM cu_nodes')",
        )?;
        for semiring in ["plus_times", "min_plus", "any_pair"] {
            for transpose in [false, true] {
                on_both(
                    "cu",
                    &format!(
                        "SELECT warren.spmv('{{g}}', 'x', 'y', semiring := '{semiring}',
                                            transpose := {transpose})"
                    ),
                )?;
                same_with_or_without_merged(
                    "cu",
                    "SELECT format('%s %s', node_id, value)
                     FROM warren.vector_values('{g}', 'y', NULL::bigint)",
                )?;
            }
        }
        same_with_or_without_merged(
            "cu",
            "SELECT format('%s %s', node_id, value)
             FROM warren.gather('{g}', 'SELECT id, id::text FROM cu_nodes', NULL::bigint,
                                NULL::text[], direction := 'in')",
        )?;
        Ok(())
    }
}
