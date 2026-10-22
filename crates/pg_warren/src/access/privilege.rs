//! Membership checks (DESIGN.md §7): management functions require
//! membership in the graph's owner role; query functions accept members of
//! the owner role or of a role in `graphs.readers`. A dropped owner is an
//! error for both; reader roles that no longer exist are ignored.

use pgrx::prelude::*;

use super::AccessError;
use crate::read_only::{read_one, read_two};

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

/// Whether the caller is a member of `owner` or of one of `readers`; not
/// if the owner role was dropped.
pub(crate) fn is_reader(owner: pg_sys::Oid, readers: &[pg_sys::Oid]) -> Result<bool, AccessError> {
    let (name, member) = membership(owner, readers)?;
    Ok(name.is_some() && member)
}

/// Whether the caller is a member of `owner`; not if it was dropped.
pub(crate) fn is_owner(owner: pg_sys::Oid) -> Result<bool, AccessError> {
    is_reader(owner, &[])
}

/// Checks that the caller may replace or drop persisted vector `vector`
/// of graph `graph`, written by role `creator`: as a member of `creator`
/// or of `owner`, the graph's owner role (DESIGN.md §7).
pub(crate) fn require_creator_or_owner(
    creator: pg_sys::Oid,
    owner: pg_sys::Oid,
    graph: &str,
    vector: &str,
) -> Result<(), AccessError> {
    let (creator_name, member) = membership(creator, &[owner])?;
    let (owner_name, _) = membership(owner, &[])?;
    let owner = owner_name.ok_or_else(|| AccessError::OwnerDropped(graph.to_owned()))?;
    let (vector, graph) = (vector.to_owned(), graph.to_owned());
    match (member, creator_name) {
        (true, _) => Ok(()),
        (false, Some(creator)) => Err(AccessError::NotCreator { vector, graph, creator, owner }),
        (false, None) => Err(AccessError::CreatorDropped { vector, graph, owner }),
    }
}

/// Whether the caller is a superuser.
pub(crate) fn is_superuser() -> Result<bool, AccessError> {
    Ok(read_one::<bool>(
        "SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = pg_catalog.current_user()",
        &[],
    )?
    .unwrap_or(false))
}

/// The owner's name (`None` if it no longer exists), and whether the caller
/// is a member of the owner or of an existing role in `readers`.
fn membership(
    owner: pg_sys::Oid,
    readers: &[pg_sys::Oid],
) -> Result<(Option<String>, bool), AccessError> {
    let (name, member) = read_two::<String, bool>(
        "SELECT (SELECT rolname::text FROM pg_catalog.pg_roles WHERE oid = $1),
                EXISTS (SELECT 1 FROM pg_catalog.pg_roles r
                        WHERE (r.oid = $1 OR r.oid = ANY ($2))
                          AND pg_catalog.pg_has_role(r.oid, 'MEMBER'))",
        &[owner.into(), readers.to_vec().into()],
    )?;
    Ok((name, member.unwrap_or(false)))
}
