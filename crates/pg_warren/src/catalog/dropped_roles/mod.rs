//! Cleanup after a dropped role (DESIGN.md §5.7, §14 items 44 and 45).
//! `graphs.owner`, `graphs.readers` and `vectors.created_by` are bare
//! role OIDs that `DROP ROLE` cannot see, so this feature offers what
//! Postgres's own procedure for dropping a role does to objects, for
//! graphs: `warren.drop_owned` (`DROP OWNED`), the optional event trigger
//! `warren.on_drop_owned` on `DROP OWNED`, and the sweep for what neither
//! saw, `warren.orphans` and `warren.drop_orphans`.
//! (`warren.reassign_owned` is ownership transfer, in `catalog::owner`.)

mod drop_owned;
mod event_trigger;
mod orphans;

use pgrx::prelude::*;
use pgrx::spi::SpiClient;

use crate::access::{may_drop_orphans, AccessError};

/// Who is cleaning up after a role, for which of its quotas go.
#[derive(Debug, Clone, Copy)]
struct Cleaner {
    /// The calling role.
    caller: pg_sys::Oid,
    /// Whether it may drop orphans (DESIGN.md §14 item 45).
    may_drop_orphans: bool,
}

impl Cleaner {
    /// The calling role; before any identity switch.
    fn current() -> Result<Self, AccessError> {
        // SAFETY: reads the backend's current user id: the caller.
        let caller = unsafe { pg_sys::GetUserId() };
        Ok(Self { caller, may_drop_orphans: may_drop_orphans()? })
    }
}

/// Removes what `DROP OWNED` removes of `role` besides what it owns: the
/// vectors it wrote on other roles' graphs (those on its own graphs go
/// with them), its entries in every graph's readers and its quotas on
/// readers' vectors where `by` may set them (a member of the graph's
/// owner) or may drop orphans: a role cannot shed a quota another owner
/// set on it, which then waits for `warren.drop_orphans` once the role is
/// dropped (DESIGN.md §5.7). `client` must run as the catalog owner.
fn clean_up_after(
    client: &mut SpiClient<'_>,
    role: pg_sys::Oid,
    by: Cleaner,
) -> Result<(), pgrx::spi::Error> {
    client.update("DELETE FROM warren.vectors WHERE created_by::oid = $1", None, &[role.into()])?;
    client.update(
        "UPDATE warren.graphs SET readers = ARRAY(
             SELECT r.reader FROM pg_catalog.unnest(readers) WITH ORDINALITY AS r (reader, position)
             WHERE r.reader::oid <> $1 ORDER BY r.position)::regrole[]
         WHERE $1 = ANY (readers::oid[])",
        None,
        &[role.into()],
    )?;
    client.update(
        "DELETE FROM warren.vector_quotas q USING warren.graphs g
         WHERE q.role::oid = $1 AND g.graph_id = q.graph_id
           AND ($3 OR pg_catalog.pg_has_role($2::oid, g.owner::oid, 'MEMBER'))",
        None,
        &[role.into(), by.caller.into(), by.may_drop_orphans.into()],
    )?;
    Ok(())
}
