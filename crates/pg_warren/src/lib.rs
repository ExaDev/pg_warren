//! `pg_warren`: the pgrx extension — the Postgres adapter around the
//! `warren-core` domain (DESIGN.md §11). One module per feature; each talks to
//! Postgres itself and delegates graph logic to `warren-core`.

::pgrx::pg_module_magic!(name, version);

mod access;
mod build;
mod cache;
mod catalog;
mod community_sql;
mod compute_sql;
mod extension;
mod external_ids;
#[cfg(any(test, feature = "pg_test"))]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod fixture;
mod interrupts;
#[cfg(any(test, feature = "pg_test"))]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod parallel_labels;
mod read_only;
mod segment;
mod selection;
mod source;
mod spectral_sql;
mod traversal_sql;
mod vector;
mod version_sql;

/// Library initialisation: registers the GUCs and, when preloaded on
/// PG 15 and 16, the hooks that place the shared cache's registry.
#[pgrx::pg_guard]
pub extern "C-unwind" fn _PG_init() {
    build::define_max_build_memory();
    cache::define_memory_settings();
    cache::define_shared_cache();
    traversal_sql::define_max_walk_paths();
    compute_sql::define_compute_settings();
}

/// Required by `cargo pgrx test`; must be visible at the crate root.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    /// The preload places the shared cache's registry on PG 15 and 16
    /// (DESIGN.md §5.6); `warren.shared_cache_memory` stays 0, so only the
    /// shared cache's own tests use it.
    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec!["shared_preload_libraries = 'pg_warren'"]
    }
}
