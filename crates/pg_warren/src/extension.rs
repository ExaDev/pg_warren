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
#[pg_extern(schema = "warren", stable, parallel_safe)]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    // `#[pg_test]` functions are not `#[test]` to clippy, so they propagate
    // SPI errors with `?` instead of unwrapping (workspace lints deny unwrap).

    #[pg_test]
    fn warren_version_returns_crate_version() -> Result<(), Error> {
        let v = Spi::get_one::<String>("SELECT warren.version()")?;
        assert_eq!(v.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        Ok(())
    }

    #[pg_test]
    fn warren_version_lives_in_warren_schema() -> Result<(), Error> {
        let in_warren = Spi::get_one::<bool>(
            "SELECT pronamespace = 'warren'::regnamespace
             FROM pg_proc WHERE oid = 'warren.version()'::regprocedure",
        )?;
        assert_eq!(in_warren, Some(true));
        Ok(())
    }

    #[pg_test]
    fn warren_version_is_stable_not_immutable() -> Result<(), Error> {
        // The answer changes when the library is upgraded in place, so it must
        // not be constant-folded into stored plans or index expressions.
        let volatility = Spi::get_one::<String>(
            "SELECT provolatile::text FROM pg_proc
             WHERE oid = 'warren.version()'::regprocedure",
        )?;
        assert_eq!(volatility.as_deref(), Some("s"));
        Ok(())
    }

    #[pg_test]
    fn extension_creates_warren_schema() -> Result<(), Error> {
        let owns = Spi::get_one::<bool>(
            "SELECT EXISTS (
                 SELECT 1 FROM pg_depend d
                 JOIN pg_extension e ON e.oid = d.refobjid
                 JOIN pg_namespace n ON n.oid = d.objid
                 WHERE e.extname = 'pg_warren' AND n.nspname = 'warren'
                   AND d.classid = 'pg_namespace'::regclass)",
        )?;
        assert_eq!(owns, Some(true), "schema warren must be created by the extension");
        Ok(())
    }

    #[pg_test]
    fn drop_extension_drops_warren_schema() -> Result<(), Error> {
        Spi::run("DROP EXTENSION pg_warren CASCADE")?;
        let exists = Spi::get_one::<bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = 'warren')",
        )?;
        assert_eq!(exists, Some(false));
        Ok(())
    }
}
