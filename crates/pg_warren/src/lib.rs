//! `pg_warren`: the pgrx extension — the Postgres adapter around the
//! `warren-core` domain (DESIGN.md §11). One module per feature; each talks to
//! Postgres itself and delegates graph logic to `warren-core`.

::pgrx::pg_module_magic!(name, version);

mod build;
mod cache;
mod catalog;
mod compute_sql;
mod extension;
mod traversal_sql;

/// Required by `cargo pgrx test`; must be visible at the crate root.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
