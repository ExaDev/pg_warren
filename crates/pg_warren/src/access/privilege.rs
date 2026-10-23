//! Membership checks (DESIGN.md §7): management functions require
//! membership in the graph's owner role; query functions accept members of
//! the owner role or of a role in `graphs.readers`. A dropped owner is an
//! error for both; reader roles that no longer exist are ignored.

use std::ffi::CStr;

use pgrx::prelude::*;

use super::AccessError;
use crate::read_only::read_one;

/// Checks that the caller is a member of `owner`, the owner role of graph
/// `graph`; `action` names the function for the error.
pub(crate) fn require_owner(
    owner: pg_sys::Oid,
    graph: &str,
    action: &'static str,
) -> Result<(), AccessError> {
    let (name, member) = membership(owner, &[]);
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
    let (name, member) = membership(owner, readers);
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
pub(crate) fn is_reader(owner: pg_sys::Oid, readers: &[pg_sys::Oid]) -> bool {
    let (name, member) = membership(owner, readers);
    name.is_some() && member
}

/// Whether the caller is a member of `owner`; not if it was dropped.
pub(crate) fn is_owner(owner: pg_sys::Oid) -> bool {
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
    let (creator_name, member) = membership(creator, &[owner]);
    let (owner_name, _) = membership(owner, &[]);
    let owner = owner_name.ok_or_else(|| AccessError::OwnerDropped(graph.to_owned()))?;
    let (vector, graph) = (vector.to_owned(), graph.to_owned());
    match (member, creator_name) {
        (true, _) => Ok(()),
        (false, Some(creator)) => Err(AccessError::NotCreator { vector, graph, creator, owner }),
        (false, None) => Err(AccessError::CreatorDropped { vector, graph, owner }),
    }
}

/// Checks that the caller may make `role` the owner of graph `graph`, as
/// `ALTER … OWNER TO` checks a new owner (DESIGN.md §5.7): on PG 16+, the
/// caller can `SET ROLE` to it (`member_can_set_role`, the test of
/// `check_can_set_role`); on PG 15, which has no `SET` option, it is a
/// member of it. A superuser may always.
pub(crate) fn require_can_become(role: pg_sys::Oid, graph: &str) -> Result<(), AccessError> {
    let name = role_name(role).ok_or(AccessError::RoleNotFound(role.to_u32()))?;
    match can_set_role(role) {
        true => Ok(()),
        false => Err(AccessError::CannotBecome { role: name, graph: graph.to_owned() }),
    }
}

/// Whether the current user may `SET ROLE` to `role`.
#[cfg(not(feature = "pg15"))]
fn can_set_role(role: pg_sys::Oid) -> bool {
    // SAFETY: reads the current user and its cached role memberships, on
    // the main thread.
    unsafe { pg_sys::member_can_set_role(pg_sys::GetUserId(), role) }
}

/// Whether the current user may `SET ROLE` to `role`: on PG 15, whether
/// it is a member of it.
#[cfg(feature = "pg15")]
fn can_set_role(role: pg_sys::Oid) -> bool {
    is_member(role)
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
/// is a member of the owner or of an existing role in `readers`, as
/// `pg_has_role(role, 'MEMBER')` judges it (a superuser is a member of
/// every role). Read through the role caches, not SQL: every query
/// function checks it on every call, and planning a query per call cost
/// most of a point lookup (DESIGN.md §5.6, step 8).
fn membership(owner: pg_sys::Oid, readers: &[pg_sys::Oid]) -> (Option<String>, bool) {
    let name = role_name(owner);
    let member = name.is_some() && is_member(owner)
        || readers.iter().any(|&reader| role_name(reader).is_some() && is_member(reader));
    (name, member)
}

/// The name of role `role`, if it exists.
fn role_name(role: pg_sys::Oid) -> Option<String> {
    // SAFETY: a syscache lookup on the main thread; with `noerr` it returns
    // NULL for a role that does not exist, else a palloc'd name.
    let name = unsafe { pg_sys::GetUserNameFromId(role, true) };
    // SAFETY: NULL or a NUL-terminated string Postgres just allocated.
    (!name.is_null()).then(|| unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned())
}

/// Whether the current user is a member of role `role`, as
/// `pg_has_role(role, 'MEMBER')` judges it.
fn is_member(role: pg_sys::Oid) -> bool {
    // SAFETY: reads the current user and its cached role memberships, on
    // the main thread.
    unsafe { pg_sys::is_member_of_role(pg_sys::GetUserId(), role) }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::membership;

    /// `membership` as SQL defines it: the owner's name, and whether the
    /// current user is a member (`pg_has_role(…, 'MEMBER')`) of the owner
    /// or of an existing role among `readers`.
    fn by_sql(
        owner: pg_sys::Oid,
        readers: &[pg_sys::Oid],
    ) -> Result<(Option<String>, bool), Error> {
        let (name, member) = Spi::get_two_with_args::<String, bool>(
            "SELECT (SELECT rolname::text FROM pg_catalog.pg_roles WHERE oid = $1),
                    EXISTS (SELECT 1 FROM pg_catalog.pg_roles r
                            WHERE (r.oid = $1 OR r.oid = ANY ($2))
                              AND pg_catalog.pg_has_role(r.oid, 'MEMBER'))",
            &[owner.into(), readers.to_vec().into()],
        )?;
        Ok((name, member.unwrap_or(false)))
    }

    fn oid(role: &str) -> Result<pg_sys::Oid, Error> {
        Spi::get_one::<pg_sys::Oid>(&format!("SELECT '{role}'::regrole::oid"))?
            .ok_or(Error::InvalidPosition)
    }

    #[pg_test]
    fn membership_agrees_with_pg_has_role() -> Result<(), Error> {
        Spi::run(
            "CREATE ROLE m_owner; CREATE ROLE m_reader; CREATE ROLE m_nested;
             CREATE ROLE m_stranger; CREATE ROLE m_gone; CREATE ROLE m_noinh NOINHERIT;
             GRANT m_reader TO m_nested; GRANT m_owner TO m_nested; GRANT m_owner TO m_noinh",
        )?;
        let (owner, reader, gone) = (oid("m_owner")?, oid("m_reader")?, oid("m_gone")?);
        Spi::run("DROP ROLE m_gone")?;
        let cases: [(pg_sys::Oid, Vec<pg_sys::Oid>); 6] = [
            (owner, vec![]),
            (owner, vec![reader]),
            (owner, vec![gone]),
            (owner, vec![gone, reader]),
            (gone, vec![reader]),
            (gone, vec![]),
        ];
        // m_noinh is a member of m_owner without inheriting its privileges:
        // membership is MEMBER (`is_member_of_role`), not USAGE.
        for user in ["m_owner", "m_reader", "m_nested", "m_noinh", "m_stranger", "m_superuser"] {
            match user {
                "m_superuser" => Spi::run("RESET ROLE")?,
                _ => Spi::run(&format!("SET ROLE {user}"))?,
            }
            for (owner, readers) in &cases {
                let found = membership(*owner, readers);
                assert_eq!(found, by_sql(*owner, readers)?, "{user}: {owner:?} {readers:?}");
            }
        }
        Spi::run("RESET ROLE")?;
        Ok(())
    }
}
