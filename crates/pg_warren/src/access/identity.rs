//! Identity switches (DESIGN.md §7): source queries run as the graph owner;
//! catalog, segment and vector statements run as the extension owner.

use std::cell::Cell;

use pgrx::prelude::*;

use super::AccessError;
use crate::read_only::read_one;

/// The security context flags of a switch: the user id was changed locally,
/// and the statements inside may not change it back or create temporary
/// objects (as for `REFRESH MATERIALIZED VIEW`).
const SWITCH_CONTEXT: i32 =
    (pg_sys::SECURITY_LOCAL_USERID_CHANGE | pg_sys::SECURITY_RESTRICTED_OPERATION) as i32;

thread_local! {
    /// The extension owner, read once per backend.
    static EXTENSION_OWNER: Cell<Option<pg_sys::Oid>> = const { Cell::new(None) };
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

/// Runs `f` as the extension owner, the only role with privileges on the
/// `warren.*` tables (DESIGN.md §5.1, §7). Only extension-written SQL
/// against those tables may run inside.
pub(crate) fn as_extension_owner<R, E: From<AccessError>>(
    f: impl FnOnce() -> Result<R, E>,
) -> Result<R, E> {
    as_role(extension_owner()?, f)
}

/// The role that owns the pg_warren extension (`pg_extension.extowner`).
pub(crate) fn extension_owner() -> Result<pg_sys::Oid, AccessError> {
    if let Some(owner) = EXTENSION_OWNER.get() {
        return Ok(owner);
    }
    let owner = read_one::<pg_sys::Oid>(
        "SELECT extowner FROM pg_catalog.pg_extension WHERE extname = 'pg_warren'",
        &[],
    )?
    .ok_or(AccessError::ExtensionMissing)?;
    EXTENSION_OWNER.set(Some(owner));
    Ok(owner)
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
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

    #[pg_test]
    fn the_extension_owner_is_the_role_that_created_the_extension() -> Result<(), Error> {
        let owner = Spi::get_one::<pg_sys::Oid>(
            "SELECT extowner FROM pg_catalog.pg_extension WHERE extname = 'pg_warren'",
        )?;
        assert_eq!(extension_owner().ok(), owner);
        Ok(())
    }
}
