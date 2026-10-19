//! Membership checks (DESIGN.md §7): management functions require
//! membership in the graph's owner role.

use pgrx::prelude::*;

use super::AccessError;

/// Checks that the caller is a member of `owner`, the owner role of graph
/// `graph`; `action` names the function for the error. A dropped owner is
/// an error of its own.
pub(crate) fn require_owner(
    owner: pg_sys::Oid,
    graph: &str,
    action: &'static str,
) -> Result<(), AccessError> {
    let (name, member) = Spi::get_two_with_args::<String, bool>(
        "SELECT r.rolname::text,
                CASE WHEN r.oid IS NOT NULL THEN pg_has_role(r.oid, 'MEMBER') END
         FROM (SELECT $1::oid AS oid) o LEFT JOIN pg_roles r ON r.oid = o.oid",
        &[owner.into()],
    )?;
    match (name, member) {
        (Some(_), Some(true)) => Ok(()),
        (Some(owner), _) => Err(AccessError::NotOwner { owner, action, graph: graph.to_owned() }),
        (None, _) => Err(AccessError::OwnerDropped(graph.to_owned())),
    }
}
