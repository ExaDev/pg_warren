//! Extension identity: the `warren` schema and `warren.version()`.
//!
//! The schema is declared once, here, as `#[pg_schema] mod warren`; pgrx emits
//! `CREATE SCHEMA IF NOT EXISTS warren` into the extension script, so the
//! schema is a member of the extension and is dropped with it. See the note in
//! `pg_warren.control` for why the control file has no `schema` key. Feature
//! modules place SQL functions in it with `#[pg_extern(schema = "warren")]`.

use pgrx::prelude::*;

/// The `warren` SQL schema. Intentionally empty: it exists so pgrx creates
/// the schema; functions live in their feature modules.
#[pg_schema]
mod warren {}

/// `warren.version()`: the version of the installed extension library.
#[pg_extern(schema = "warren", immutable, parallel_safe)]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn warren_version_returns_crate_version() {
        let v = Spi::get_one::<String>("SELECT warren.version()").unwrap();
        assert_eq!(v.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }

    #[pg_test]
    fn warren_version_lives_in_warren_schema() {
        let schema = Spi::get_one::<String>(
            "SELECT n.nspname::text FROM pg_proc p
             JOIN pg_namespace n ON n.oid = p.pronamespace
             WHERE p.proname = 'version' AND n.nspname = 'warren'",
        )
        .unwrap();
        assert_eq!(schema.as_deref(), Some("warren"));
    }

    #[pg_test]
    fn extension_creates_warren_schema() {
        let owns = Spi::get_one::<bool>(
            "SELECT EXISTS (
                 SELECT 1 FROM pg_depend d
                 JOIN pg_extension e ON e.oid = d.refobjid
                 JOIN pg_namespace n ON n.oid = d.objid
                 WHERE e.extname = 'pg_warren' AND n.nspname = 'warren'
                   AND d.classid = 'pg_namespace'::regclass)",
        )
        .unwrap();
        assert_eq!(owns, Some(true), "schema warren must be created by the extension");
    }

    #[pg_test]
    fn drop_extension_drops_warren_schema() {
        Spi::run("DROP EXTENSION pg_warren CASCADE").unwrap();
        let exists = Spi::get_one::<bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = 'warren')",
        )
        .unwrap();
        assert_eq!(exists, Some(false));
    }
}
