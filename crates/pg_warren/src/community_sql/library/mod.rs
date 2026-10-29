//! The community library (DESIGN.md §6.7, group 3): Louvain as PL/pgSQL
//! over `contract`, `project`, `lift` and the partition primitives,
//! shipped as a readable template that, like the compute library, pins its
//! version, keeps its intermediate vectors transient under names
//! `_warren_louvain_<part>`, drops them (and its levels' graphs) when it
//! finishes, and persists only its output; and Leiden, native (`leiden`),
//! its levels held in memory, each contracted from the one before, and
//! persisting only its output and the levels asked for.

mod leiden;

pgrx::extension_sql_file!("louvain.sql", name = "louvain", requires = [extension::warren]);

#[cfg(any(test, feature = "pg_test"))]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests;
