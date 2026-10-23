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

/// Removes what `DROP OWNED` removes of `role` besides what it owns: the
/// vectors it wrote on other roles' graphs (those on its own graphs go
/// with them), its entries in every graph's readers and its quotas on
/// readers' vectors. `client` must run as the catalog owner.
fn clean_up_after(client: &mut SpiClient<'_>, role: pg_sys::Oid) -> Result<(), pgrx::spi::Error> {
    client.update("DELETE FROM warren.vectors WHERE created_by::oid = $1", None, &[role.into()])?;
    client.update(
        "UPDATE warren.graphs SET readers = ARRAY(
             SELECT r.reader FROM pg_catalog.unnest(readers) WITH ORDINALITY AS r (reader, position)
             WHERE r.reader::oid <> $1 ORDER BY r.position)::regrole[]
         WHERE $1 = ANY (readers::oid[])",
        None,
        &[role.into()],
    )?;
    client.update("DELETE FROM warren.vector_quotas WHERE role::oid = $1", None, &[role.into()])?;
    Ok(())
}
