//! Identity switches (DESIGN.md §7): source queries run as the graph owner;
//! catalog, segment and vector statements run as the catalog owner.

use std::cell::Cell;

use pgrx::prelude::*;

use super::AccessError;
use crate::read_only::read_one;

/// The security context flags of a switch: the user id was changed locally,
/// and the statements inside may not change it back or create temporary
/// objects (as for `REFRESH MATERIALIZED VIEW`).
const SWITCH_CONTEXT: i32 =
    (pg_sys::SECURITY_LOCAL_USERID_CHANGE | pg_sys::SECURITY_RESTRICTED_OPERATION) as i32;

/// The owner of `warren.graphs`, provided it is a member of extension
/// pg_warren (so a same-named table outside the extension never answers).
const CATALOG_OWNER: &str = "SELECT c.relowner FROM pg_catalog.pg_class c
     JOIN pg_catalog.pg_depend d ON d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass
                                AND d.objid = c.oid AND d.deptype = 'e'
     JOIN pg_catalog.pg_extension e ON e.oid = d.refobjid AND e.extname = 'pg_warren'
     WHERE c.oid = pg_catalog.to_regclass('warren.graphs')";

thread_local! {
    /// The catalog owner, read once per backend.
    static OWNER: Cell<Option<pg_sys::Oid>> = const { Cell::new(None) };
}

/// The role running the current statement (`current_user`).
pub(crate) fn current_user() -> pg_sys::Oid {
    // SAFETY: reads the backend's current user id; no arguments.
    unsafe { pg_sys::GetUserId() }
}

/// Runs `f` as `role`, security-restricted, then switches back. If `f`
/// raises an `ERROR`, the (sub)transaction abort restores the identity.
pub(crate) fn as_role<R>(role: pg_sys::Oid, f: impl FnOnce() -> R) -> R {
    let mut user = pg_sys::Oid::INVALID;
    let mut context = 0;
    // SAFETY: the out-pointers point to live locals.
    unsafe { pg_sys::GetUserIdAndSecContext(&mut user, &mut context) };
    // SAFETY: sets the backend's user id and security context; the role is
    // only used for privilege checks, which fail cleanly if it is dropped.
    unsafe { pg_sys::SetUserIdAndSecContext(role, context | SWITCH_CONTEXT) };
    let result = f();
    // SAFETY: restores the values read above.
    unsafe { pg_sys::SetUserIdAndSecContext(user, context) };
    result
}

/// Runs `f` as the catalog owner, the only role with privileges on the
/// `warren.*` tables (DESIGN.md §5.1, §7). Only extension-written SQL
/// against those tables may run inside.
pub(crate) fn as_catalog_owner<R, E: From<AccessError>>(
    f: impl FnOnce() -> Result<R, E>,
) -> Result<R, E> {
    as_role(catalog_owner()?, f)
}

/// The role that owns the `warren.*` tables: the role the install script
/// ran as. That is the installing superuser, or, in a trusted install by a
/// non-superuser, the bootstrap superuser. It is never taken from
/// `pg_extension.extowner`, which in a trusted install is the
/// non-superuser that ran `CREATE EXTENSION` (DESIGN.md §7).
pub(crate) fn catalog_owner() -> Result<pg_sys::Oid, AccessError> {
    if let Some(owner) = OWNER.get() {
        return Ok(owner);
    }
    let owner =
        read_one::<pg_sys::Oid>(CATALOG_OWNER, &[])?.ok_or(AccessError::ExtensionMissing)?;
    OWNER.set(Some(owner));
    Ok(owner)
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use super::*;
    use pgrx::spi::Error;

    fn current_user() -> Result<Option<String>, Error> {
        Spi::get_one::<String>("SELECT current_user::text")
    }

    #[pg_test]
    fn as_role_runs_as_the_role_and_then_switches_back() -> Result<(), Error> {
        Spi::run("CREATE ROLE id_switch")?;
        let role = Spi::get_one::<pg_sys::Oid>("SELECT 'id_switch'::regrole::oid")?;
        let before = current_user()?;
        let inside = as_role(role.unwrap_or(pg_sys::Oid::INVALID), current_user)?;
        assert_eq!(inside.as_deref(), Some("id_switch"));
        assert_eq!(current_user()?, before);
        Ok(())
    }

    #[pg_test]
    fn statements_inside_a_switch_are_security_restricted() -> Result<(), Error> {
        Spi::run("CREATE ROLE id_restricted")?;
        let role = Spi::get_one::<pg_sys::Oid>("SELECT 'id_restricted'::regrole::oid")?;
        let restricted = as_role(role.unwrap_or(pg_sys::Oid::INVALID), || {
            // SAFETY: reads the backend's security context; no arguments.
            unsafe { pg_sys::InSecurityRestrictedOperation() }
        });
        assert!(restricted);
        Ok(())
    }

    fn table_owner() -> Result<Option<pg_sys::Oid>, Error> {
        Spi::get_one::<pg_sys::Oid>(
            "SELECT relowner FROM pg_class WHERE oid = 'warren.graphs'::regclass",
        )
    }

    #[pg_test]
    fn the_catalog_owner_owns_the_catalog_tables() -> Result<(), Error> {
        assert_eq!(catalog_owner().ok(), table_owner()?);
        Ok(())
    }

    /// A trusted install: the tables are the bootstrap superuser's, while
    /// `extowner` is the non-superuser that ran `CREATE EXTENSION`, which
    /// has no privilege on them. The switch must elevate to the tables'
    /// owner (DESIGN.md §7).
    #[pg_test]
    fn the_switch_elevates_to_the_catalog_owner_not_the_extension_owner() -> Result<(), Error> {
        Spi::run("CREATE ROLE id_installer")?;
        Spi::run(
            "UPDATE pg_catalog.pg_extension SET extowner = 'id_installer'::regrole
             WHERE extname = 'pg_warren'",
        )?;
        let owner = table_owner()?;
        assert_eq!(catalog_owner().ok(), owner);
        let inside = as_catalog_owner(|| {
            Ok::<_, AccessError>(Spi::get_one::<pg_sys::Oid>(
                "SELECT oid FROM pg_roles WHERE rolname = current_user",
            )?)
        });
        assert_eq!(inside.ok().flatten(), owner);
        Ok(())
    }
}
