//! The community library (DESIGN.md §6.7, group 3): Louvain and Leiden as
//! PL/pgSQL over `contract`, `project`, `lift` and the partition
//! primitives, shipped as readable templates. Like the compute library,
//! each pins its version, keeps its intermediate vectors transient under
//! names `_warren_<algorithm>_<part>`, drops them (and its levels' graphs)
//! when it finishes, and persists only its output.

pgrx::extension_sql_file!("louvain.sql", name = "louvain", requires = [extension::warren]);
pgrx::extension_sql_file!("leiden.sql", name = "leiden", requires = [extension::warren]);

#[cfg(any(test, feature = "pg_test"))]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests;
