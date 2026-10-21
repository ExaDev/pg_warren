//! The algorithm library (DESIGN.md §6.7): PL/pgSQL functions over the
//! compute primitives, shipped as readable templates. Each pins the
//! version it starts on, keeps its intermediate vectors transient
//! (`persist := false`) under names `_warren_<algorithm>_<part>`, drops
//! them when it finishes, and persists only its output.

pgrx::extension_sql_file!("pagerank.sql", name = "pagerank", requires = [extension::warren]);
pgrx::extension_sql_file!(
    "degree_vector.sql",
    name = "degree_vector",
    requires = [extension::warren]
);
pgrx::extension_sql_file!(
    "components_vector.sql",
    name = "components_vector",
    requires = [extension::warren]
);
pgrx::extension_sql_file!(
    "label_propagation.sql",
    name = "label_propagation",
    requires = [extension::warren]
);
pgrx::extension_sql_file!("sssp_vector.sql", name = "sssp_vector", requires = [extension::warren]);
pgrx::extension_sql_file!(
    "aggregate_neighbours.sql",
    name = "aggregate_neighbours",
    requires = [extension::warren]
);
pgrx::extension_sql_file!(
    "eigenvector_centrality.sql",
    name = "eigenvector_centrality",
    requires = [extension::warren]
);
pgrx::extension_sql_file!(
    "katz_centrality.sql",
    name = "katz_centrality",
    requires = [extension::warren]
);
pgrx::extension_sql_file!("hits.sql", name = "hits", requires = [extension::warren]);

#[cfg(any(test, feature = "pg_test"))]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests;

#[cfg(any(test, feature = "pg_test"))]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod centrality_tests;
