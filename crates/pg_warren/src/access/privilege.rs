//! Membership checks (DESIGN.md §7): management functions require
//! membership in the graph's owner role; query functions accept members of
//! the owner role or of a role in `graphs.readers`. A dropped owner is an
//! error for both; reader roles that no longer exist are ignored.

use pgrx::prelude::*;

use super::AccessError;

/// Checks that the caller is a member of `owner`, the owner role of graph
/// `graph`; `action` names the function for the error.
pub(crate) fn require_owner(
    owner: pg_sys::Oid,
    graph: &str,
    action: &'static str,
) -> Result<(), AccessError> {
    let (name, member) = membership(owner, &[])?;
    match (name, member) {
        (Some(_), true) => Ok(()),
        (Some(owner), false) => {
            Err(AccessError::NotOwner { owner, action, graph: graph.to_owned() })
        }
        (None, _) => Err(AccessError::OwnerDropped(graph.to_owned())),
    }
}

/// Checks that the caller is a member of `owner` or of one of `readers`.
pub(crate) fn require_reader(
    owner: pg_sys::Oid,
    readers: &[pg_sys::Oid],
    graph: &str,
    action: &'static str,
) -> Result<(), AccessError> {
    let (name, member) = membership(owner, readers)?;
    match (name, member) {
        (Some(_), true) => Ok(()),
        (Some(owner), false) => {
            Err(AccessError::NotReader { owner, action, graph: graph.to_owned() })
        }
        (None, _) => Err(AccessError::OwnerDropped(graph.to_owned())),
    }
}

/// Whether the caller is a superuser.
pub(crate) fn is_superuser() -> Result<bool, AccessError> {
    Ok(Spi::get_one::<bool>(
        "SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = pg_catalog.current_user()",
    )?
    .unwrap_or(false))
}

/// The owner's name (`None` if it no longer exists), and whether the caller
/// is a member of the owner or of an existing role in `readers`.
fn membership(
    owner: pg_sys::Oid,
    readers: &[pg_sys::Oid],
) -> Result<(Option<String>, bool), AccessError> {
    let (name, member) = Spi::get_two_with_args::<String, bool>(
        "SELECT (SELECT rolname::text FROM pg_catalog.pg_roles WHERE oid = $1),
                EXISTS (SELECT 1 FROM pg_catalog.pg_roles r
                        WHERE (r.oid = $1 OR r.oid = ANY ($2))
                          AND pg_catalog.pg_has_role(r.oid, 'MEMBER'))",
        &[owner.into(), readers.to_vec().into()],
    )?;
    Ok((name, member.unwrap_or(false)))
}
