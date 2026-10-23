//! `warren.drop_owned`: the SQL counterpart of `DROP OWNED` for graphs and
//! vectors (DESIGN.md §5.7).

use pgrx::prelude::*;

use super::super::CatalogError;
use super::{clean_up_after, Cleaner};
use crate::access::{as_catalog_owner, entry, refuse_bootstrap_superuser, require_privileges_of};

/// `warren.drop_owned(role regrole)`: drops every graph `role` owns (with
/// its versions and everyone's vectors, as `drop_graph` does), deletes the
/// vectors it wrote on other graphs and removes it from every graph's
/// readers, and its quotas on readers' vectors on graphs whose owner the
/// caller is a member of. The caller needs the privileges of `role`.
/// Returns the number of graphs dropped.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren."drop_owned"(role regrole) RETURNS bigint
STRICT VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'drop_owned_wrapper';
"#
)]
fn drop_owned(role: pg_sys::Oid) -> i64 {
    entry(|_| drop_everything_of(role))
}

fn drop_everything_of(role: pg_sys::Oid) -> Result<i64, CatalogError> {
    require_privileges_of(role, "drop its graphs and vectors")?;
    refuse_bootstrap_superuser(role, "drop")?;
    let by = Cleaner::current()?;
    let dropped = as_catalog_owner(|| {
        Spi::connect_mut(|client| {
            // Deleting a graph's row takes its lock, waiting for a running
            // refresh or set_owner, and re-checks `owner` once it holds it
            // (EvalPlanQual), as `drop_graph`'s lock does (DESIGN.md §5.7).
            let dropped = client
                .update(
                    "DELETE FROM warren.graphs WHERE owner::oid = $1 RETURNING graph_id",
                    None,
                    &[role.into()],
                )?
                .len();
            clean_up_after(client, role, by)?;
            Ok::<_, CatalogError>(dropped)
        })
    })?;
    i64::try_from(dropped).map_err(|_| CatalogError::TooLarge("graphs dropped"))
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, failure_as, neighbouring_graphs, ownership, quotas};

    #[pg_test]
    fn drop_owned_drops_the_roles_graphs_vectors_reader_entries_and_quotas() -> Result<(), Error> {
        neighbouring_graphs()?;
        let dropped = Spi::get_one::<i64>("SELECT warren.drop_owned('dr_owner')")?;
        assert_eq!(dropped, Some(1));
        assert_eq!(ownership()?.as_deref(), Some("dk:dk_owner:{} / dk.theirs:dk_owner"));
        assert_eq!(quotas()?.as_deref(), Some(""));
        Ok(())
    }

    /// As `DROP OWNED`: the privileges of the role, which a member that
    /// does not inherit them has not.
    #[pg_test]
    fn drop_owned_needs_the_privileges_of_the_role() -> Result<(), Error> {
        neighbouring_graphs()?;
        Spi::run(
            "CREATE ROLE dr_stranger; CREATE ROLE dr_member NOINHERIT;
             GRANT dr_owner TO dr_member",
        )?;
        let before = ownership()?;
        for role in ["dr_stranger", "dr_member"] {
            assert_eq!(
                failure_as(role, "warren.drop_owned('dr_owner')")?.as_deref(),
                Some(
                    "42501: must have the privileges of role \"dr_owner\" to drop its graphs and \
                     vectors"
                ),
                "{role}"
            );
        }
        assert_eq!(ownership()?, before);
        Spi::run("GRANT dr_owner TO dr_stranger")?;
        assert_eq!(failure_as("dr_stranger", "warren.drop_owned('dr_owner')")?, None);
        Ok(())
    }

    #[pg_test]
    fn drop_owned_of_a_role_that_does_not_exist_is_refused() -> Result<(), Error> {
        let gone =
            Spi::get_one::<pg_sys::Oid>("CREATE ROLE dr_gone; SELECT 'dr_gone'::regrole::oid")?
                .unwrap_or(pg_sys::Oid::INVALID);
        Spi::run("DROP ROLE dr_gone")?;
        assert_eq!(
            failure(&format!("warren.drop_owned({}::oid::regrole)", gone.to_u32()))?,
            Some(format!("42704: role with OID {} does not exist", gone.to_u32()))
        );
        Ok(())
    }

    /// As `DROP OWNED BY` the bootstrap superuser.
    #[pg_test]
    fn drop_owned_refuses_the_bootstrap_superuser() -> Result<(), Error> {
        let name = Spi::get_one::<String>("SELECT rolname::text FROM pg_roles WHERE oid = 10")?
            .unwrap_or_default();
        assert_eq!(
            failure("warren.drop_owned(10::oid::regrole)")?,
            Some(format!(
                "2BP01: cannot drop objects owned by role \"{name}\" because they are required by \
                 the database system"
            ))
        );
        Ok(())
    }

    /// The security review of PR #59: a reader that drops what it owns
    /// keeps the quota another graph's owner set on it; the sweep removes
    /// it once the role is dropped.
    #[pg_test]
    fn a_role_that_drops_what_it_owns_keeps_its_quotas_on_others_graphs() -> Result<(), Error> {
        neighbouring_graphs()?;
        assert_eq!(failure_as("dr_owner", "warren.drop_owned('dr_owner')")?, None);
        assert_eq!(quotas()?.as_deref(), Some("dk:dr_owner:4096"));
        assert_eq!(ownership()?.as_deref(), Some("dk:dk_owner:{} / dk.theirs:dk_owner"));
        Ok(())
    }

    /// The owner of the graph a quota is on may remove it with the role's
    /// other entries.
    #[pg_test]
    fn the_graphs_owner_removes_a_roles_quota_with_drop_owned() -> Result<(), Error> {
        neighbouring_graphs()?;
        Spi::run("GRANT dr_owner TO dk_owner")?;
        assert_eq!(failure_as("dk_owner", "warren.drop_owned('dr_owner')")?, None);
        assert_eq!(quotas()?.as_deref(), Some(""));
        Ok(())
    }
}
